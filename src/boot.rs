//! Boot execution: kexec into Linux targets and hand foreign EFI media to
//! the firmware.
//!
//! Linux targets use kexec, which needs no firmware reboot and keeps the
//! whole flow in-process. Media that only carries an EFI application
//! (Windows/Ubuntu installers) is booted by creating a one-shot firmware
//! boot entry (`efibootmgr --bootnext`) and rebooting.

use crate::bls::{self, BootEntry};
use crate::devices::{self, BootDevice, BootKind};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::process::Command;

/// EFI global variable namespace (Boot####, BootOrder, BootNext).
const EFI_GLOBAL_GUID: &str = "8be4df61-93ca-11d2-aa0d-00e098032b8c";

fn kexec_bin() -> String {
    first_existing(&["/usr/sbin/kexec", "/sbin/kexec", "/usr/bin/kexec"])
}

fn efibootmgr_bin() -> String {
    first_existing(&[
        "/usr/sbin/efibootmgr",
        "/sbin/efibootmgr",
        "/usr/bin/efibootmgr",
    ])
}

fn first_existing(paths: &[&str]) -> String {
    for path in paths {
        if Path::new(path).exists() {
            return path.to_string();
        }
    }
    paths[paths.len() - 1].to_string()
}

/// Build the argv vectors for the two-step kexec boot:
/// `kexec -l <kernel> [--initrd <f>]... --command-line <options>` then
/// `kexec -e`.
pub fn kexec_argv(entry: &BootEntry) -> Vec<Vec<String>> {
    let mut load = vec![kexec_bin(), "-l".to_string(), entry.linux.clone()];
    for initrd in &entry.initrd {
        load.push("--initrd".to_string());
        load.push(initrd.clone());
    }
    if !entry.options.is_empty() {
        load.push("--command-line".to_string());
        load.push(entry.options.clone());
    }
    vec![load, vec![kexec_bin(), "-e".to_string()]]
}

/// Execute the boot. With `dry_run` the commands are only printed — useful
/// for development outside the boot environment (no /sbin/kexec, no root).
///
/// Returns an error message when the load step failed; a successful
/// `kexec -e` never returns (the kernel replaces this process).
pub fn boot(entry: &BootEntry, dry_run: bool) -> Result<(), String> {
    for argv in kexec_argv(entry) {
        if dry_run {
            println!("dry-run: {}", shell_join(&argv));
            continue;
        }
        let status = Command::new(&argv[0])
            .args(&argv[1..])
            .status()
            .map_err(|e| format!("failed to spawn {}: {e}", argv[0]))?;
        if !status.success() && argv.last().map(String::as_str) == Some("-e") {
            // A failed `kexec -e` is fatal for the boot flow but must not
            // take the whole UI down: report and let the caller fall back.
            return Err(format!("kexec -e exited with {status}"));
        }
    }
    Ok(())
}

/// Boot a hot-plugged device or firmware disc.
pub fn boot_device(device: &BootDevice, dry_run: bool) -> Result<(), String> {
    match &device.boot {
        BootKind::Kexec {
            fs,
            entries,
            default,
        } => {
            let entry = bls::default_entry(entries, default)
                .cloned()
                .ok_or_else(|| "no usable boot entry on the device".to_string())?;
            boot_media_kexec(device, fs, &entry, dry_run)
        }
        BootKind::Efi { loader, part } => efi_boot(device, loader, *part, dry_run),
        BootKind::FirmwareEntry { number } => firmware_entry_boot(number, dry_run),
        BootKind::Firmware { model, loader } => {
            firmware_boot(device, model, loader.as_deref(), dry_run)
        }
    }
}

/// Mount the media read-only, load the kernel/initrd with `kexec -l`, then
/// unmount and execute it. kexec_file_load copies the files into kernel
/// memory during the load, so the mount is no longer needed for `-e`.
fn boot_media_kexec(
    device: &BootDevice,
    fs: &str,
    entry: &BootEntry,
    dry_run: bool,
) -> Result<(), String> {
    let dir = devices::probe_dir(&device.devnode);
    let kernel = dir.join(entry.linux.trim_start_matches('/'));
    let initrds: Vec<std::path::PathBuf> = entry
        .initrd
        .iter()
        .map(|p| dir.join(p.trim_start_matches('/')))
        .collect();

    let mut load = vec![kexec_bin(), "-l".to_string(), kernel.display().to_string()];
    for initrd in &initrds {
        load.push("--initrd".to_string());
        load.push(initrd.display().to_string());
    }
    if !entry.options.is_empty() {
        load.push("--command-line".to_string());
        load.push(entry.options.clone());
    }

    if dry_run {
        println!(
            "dry-run: mount -o ro -t {fs} {} {}",
            device.devnode,
            dir.display()
        );
        println!("dry-run: {}", shell_join(&load));
        println!("dry-run: umount {}", dir.display());
        println!("dry-run: {} -e", kexec_bin());
        return Ok(());
    }

    let mount = devices::mount_media(&device.devnode, fs)?;
    let status = Command::new(&load[0])
        .args(&load[1..])
        .output()
        .map_err(|e| format!("failed to spawn {}: {e}", load[0]))?;
    // Unmount before kexec -e: the next kernel starts with no mounts anyway,
    // but leaving a mounted foreign filesystem behind confuses the kexec
    // shutdown path.
    mount.umount();

    if !status.status.success() {
        let stderr = String::from_utf8_lossy(&status.stderr);
        let stderr = stderr.trim();
        return Err(if stderr.is_empty() {
            format!("kexec -l exited with {}", status.status)
        } else {
            format!("kexec -l failed: {stderr}")
        });
    }

    let status = Command::new(kexec_bin())
        .arg("-e")
        .status()
        .map_err(|e| format!("failed to spawn {}: {e}", kexec_bin()))?;
    Err(format!("kexec -e exited with {status}"))
}

/// Create (or reuse) a firmware boot entry for the EFI application on the
/// device, make it the next boot target and reboot into the firmware.
fn efi_boot(device: &BootDevice, loader: &str, part: u32, dry_run: bool) -> Result<(), String> {
    if !Path::new("/sys/firmware/efi").exists() {
        return Err(
            "UEFI firmware boot is unavailable (this system booted in BIOS mode)".to_string(),
        );
    }

    let efibootmgr = efibootmgr_bin();
    let label = if device.uuid.is_empty() {
        format!("SplashOS {}", device.title)
    } else {
        format!("SplashOS {} [{}]", device.title, device.uuid)
    };

    if dry_run {
        println!(
            "dry-run: {efibootmgr} -c -d {} -p {part} -L '{}' -l '{}'",
            device.disk, label, loader
        );
        println!("dry-run: {efibootmgr} --bootnext <new entry>");
        println!("dry-run: sync; echo b > /proc/sysrq-trigger");
        return Ok(());
    }

    devices::ensure_efivars()?;
    let number = match find_boot_entry(&label)? {
        Some(number) => number,
        None => create_boot_entry(
            &efibootmgr,
            &label,
            vec!["-d", &device.disk, "-p", &part.to_string(), "-l", loader],
        )?,
    };

    set_boot_next(&efibootmgr, &number)?;
    reboot_now()
}

/// An existing firmware boot entry (Windows Boot Manager, a distro GRUB,
/// the firmware UI, ...): make it the next boot target and warm-reboot.
/// This is the route for every installed non-Linux OS — kexec cannot load
/// them, but the firmware can.
pub fn firmware_entry_boot(number: &str, dry_run: bool) -> Result<(), String> {
    if !Path::new("/sys/firmware/efi").exists() {
        return Err(
            "UEFI firmware boot is unavailable (this system booted in BIOS mode)".to_string(),
        );
    }

    let efibootmgr = efibootmgr_bin();
    if dry_run {
        println!("dry-run: {efibootmgr} --bootnext {number}");
        println!("dry-run: sync; echo b > /proc/sysrq-trigger");
        return Ok(());
    }

    devices::ensure_efivars()?;
    set_boot_next(&efibootmgr, number)?;
    reboot_now()
}

/// Optical media without a kexec-able kernel: three-tier firmware boot —
/// 1. the one-shot entry this app created for the disc earlier,
/// 2. an existing firmware entry whose label names the drive,
/// 3. an existing entry whose device path is a CD/DVD,
/// 4. otherwise create a one-shot entry for the drive.
///
/// The created entry is rewritten to its **short form** (device path without
/// a file node). efibootmgr always appends a file path, but for optical media
/// it cannot add the CDROM/filesystem node that would make that path
/// resolvable, so firmware silently skips the entry. Short form makes the
/// firmware expand the path itself: it searches `\EFI\BOOT\BOOTX64.EFI` on
/// every filesystem of the drive, which finds the El Torito boot image —
/// the one Windows install media actually boots from.
fn firmware_boot(
    device: &BootDevice,
    model: &str,
    loader: Option<&str>,
    dry_run: bool,
) -> Result<(), String> {
    if !Path::new("/sys/firmware/efi").exists() {
        return Err(
            "UEFI firmware boot is unavailable (this system booted in BIOS mode)".to_string(),
        );
    }

    let efibootmgr = efibootmgr_bin();
    let needle = if model.trim().is_empty() {
        "dvd".to_string()
    } else {
        model.to_lowercase()
    };
    let loader_path = loader.unwrap_or("\\EFI\\BOOT\\BOOTX64.EFI");
    let label = device.firmware_label();

    if dry_run {
        println!("dry-run: {efibootmgr}  # look for an entry labelled '{label}'");
        println!(
            "dry-run: (if absent) {efibootmgr} -c -d {} -L '{label}' -l '{loader_path}'",
            device.disk
        );
        println!("dry-run: rewrite Boot#### to the short-form device path (drop the File node)");
        println!("dry-run: {efibootmgr} --bootnext <entry>");
        println!("dry-run: sync; echo b > /proc/sysrq-trigger");
        return Ok(());
    }

    devices::ensure_efivars()?;
    let entries = devices::firmware_entries()?;

    // Reuse the one-shot entry this app created for the disc on an earlier
    // attempt (same label); never create a second entry for the same disc.
    let mut own: Vec<&devices::FirmwareEntry> = entries
        .iter()
        .filter(|entry| entry.label == label)
        .collect();
    if !own.is_empty() {
        let first = own.remove(0);
        for extra in own {
            delete_boot_entry(&efibootmgr, &extra.number);
        }
        ensure_short_form(&first.number)?;
        println!("firmware boot: {} (existing entry)", first.label);
        set_boot_next(&efibootmgr, &first.number)?;
        return reboot_now();
    }

    // Label scoring first ("QEMU QEMU DVD-ROM" -> "UEFI QEMU DVD-ROM"),
    // then the device path: some firmware gives the drive an unhelpful
    // label ("UEFI Misc Device") while the path still identifies the
    // optical device class.
    if let Some((number, label)) =
        firmware_entry_matching(&entries, &needle).or_else(|| optical_entry_by_path(&entries))
    {
        println!("firmware boot: {label}");
        set_boot_next(&efibootmgr, &number)?;
        return reboot_now();
    }

    // No firmware entry for the drive (OVMF does not create one for the
    // DVD at all): create a one-shot entry, then rewrite it to short form.
    // efibootmgr cannot add the CDROM/filesystem node that would make the
    // appended file path resolvable, and the firmware would silently skip
    // such an entry.
    let number = create_boot_entry(
        &efibootmgr,
        &label,
        vec!["-d", &device.disk, "-l", loader_path],
    )?;
    ensure_short_form(&number)?;
    println!("created firmware entry Boot{number} for the drive");
    set_boot_next(&efibootmgr, &number)?;
    reboot_now()
}

/// Rewrite a `Boot####` entry to its short form: drop the trailing file path
/// node so the firmware performs removable-media expansion itself (searching
/// `\EFI\BOOT\BOOTX64.EFI` on every filesystem of the device). For optical
/// media this is the only form that works: efibootmgr's file path has no
/// filesystem node in front of it, which firmware cannot resolve — the entry
/// is skipped without any message. Short form finds the El Torito image.
fn ensure_short_form(number: &str) -> Result<(), String> {
    let path = format!("/sys/firmware/efi/efivars/Boot{number}-{EFI_GLOBAL_GUID}");
    let raw = std::fs::read(&path).map_err(|e| format!("cannot read {path}: {e}"))?;
    let Some(short) = shorten_boot_option(&raw) else {
        return Ok(()); // already short form
    };
    // efivarfs entries are marked immutable on some kernels; clearing it is
    // best effort (a missing chattr is fine, the write then decides).
    let _ = Command::new("chattr").arg("-i").arg(&path).output();
    let mut file = OpenOptions::new()
        .write(true)
        .open(&path)
        .map_err(|e| format!("cannot open {path}: {e}"))?;
    file.write_all(&short)
        .map_err(|e| format!("cannot rewrite {path}: {e}"))?;
    Ok(())
}

/// Build the short-form image of a `Boot####` EFI variable. The efivarfs file
/// is the 4-byte variable attributes followed by an `EFI_LOAD_OPTION`:
/// option attributes (4), `FilePathListLength` (2), a NUL-terminated UTF-16
/// description, then the device path. Returns `None` when the device path has
/// no file node (`file_node_offset` finds none).
fn shorten_boot_option(raw: &[u8]) -> Option<Vec<u8>> {
    if raw.len() < 10 {
        return None;
    }
    let (var_attrs, data) = raw.split_at(4);
    let file_len = u16::from_le_bytes([data[4], data[5]]) as usize;
    if file_len < 6 || file_len + 6 > data.len() {
        return None;
    }
    let description_end = data.len() - file_len;
    let dp = &data[description_end..];
    let offset = file_node_offset(dp)?;
    let mut short_dp = dp[..offset].to_vec();
    short_dp.extend_from_slice(&[0x7f, 0xff, 0x04, 0x00]);
    let mut out = Vec::with_capacity(raw.len());
    out.extend_from_slice(var_attrs);
    out.extend_from_slice(&data[..4]); // EFI_LOAD_OPTION attributes
    out.extend_from_slice(&(short_dp.len() as u16).to_le_bytes());
    out.extend_from_slice(&data[6..description_end]); // description + NUL
    out.extend_from_slice(&short_dp);
    Some(out)
}

/// Offset of the `FilePath` node in an EFI device path, if any.
fn file_node_offset(dp: &[u8]) -> Option<usize> {
    let mut offset = 0;
    while offset + 4 <= dp.len() {
        let node_type = dp[offset];
        let node_subtype = dp[offset + 1];
        let len = u16::from_le_bytes([dp[offset + 2], dp[offset + 3]]) as usize;
        if len < 4 || offset + len > dp.len() {
            return None;
        }
        if node_type == 0x7f && node_subtype == 0xff {
            return None;
        }
        if node_type == 0x04 && node_subtype == 0x04 {
            return Some(offset);
        }
        offset += len;
    }
    None
}

/// Best-effort removal of a stale entry created by an older build.
fn delete_boot_entry(efibootmgr: &str, number: &str) {
    let _ = Command::new(efibootmgr).args(["-B", "-b", number]).output();
}

/// Create a firmware boot entry, restoring the firmware's own boot order
/// afterwards: `efibootmgr -c` prepends the new entry to BootOrder, which
/// would otherwise make every future firmware boot prefer the created
/// entry (e.g. the inserted CD) over the installed OS.
fn create_boot_entry(efibootmgr: &str, label: &str, args: Vec<&str>) -> Result<String, String> {
    let order = devices::firmware_boot_order();
    let output = Command::new(efibootmgr)
        .arg("-c")
        .args(["-L", label])
        .args(&args)
        .output()
        .map_err(|e| format!("failed to spawn {efibootmgr}: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "efibootmgr could not create a boot entry: {}",
            stderr.trim()
        ));
    }
    let number = find_boot_entry(label)?
        .ok_or_else(|| "efibootmgr did not create a boot entry".to_string())?;
    if !order.is_empty() {
        // Best effort: some firmware rejects BootOrder rewrites.
        let _ = Command::new(efibootmgr)
            .arg("-o")
            .arg(order.join(","))
            .output();
    }
    Ok(number)
}

/// Pick the firmware entry that belongs to a drive: score every entry by
/// how many of the drive model's words its label contains ("QEMU QEMU
/// DVD-ROM" → "UEFI QEMU DVD-ROM" scores higher than "UEFI QEMU Hard
/// Disk"), preferring entries earlier in the boot order on ties.
fn firmware_entry_matching(
    entries: &[devices::FirmwareEntry],
    model: &str,
) -> Option<(String, String)> {
    let tokens: Vec<&str> = model
        .split_whitespace()
        .map(|t| t.trim_matches(|c: char| !c.is_ascii_alphanumeric()))
        .filter(|t| t.len() >= 3)
        .collect();

    let mut best: Option<(&devices::FirmwareEntry, usize)> = None;
    for entry in entries {
        let label = entry.label.to_lowercase();
        let score = tokens.iter().filter(|t| label.contains(*t)).count();
        if score == 0 {
            continue;
        }
        if best.map(|(_, s)| score > s).unwrap_or(true) {
            best = Some((entry, score));
        }
    }
    best.map(|(entry, _)| (entry.number.clone(), entry.label.clone()))
}

/// Fallback for optical drives: the firmware entry whose device path
/// identifies a CD/DVD device (`.../CdRom(...)`), regardless of its label.
fn optical_entry_by_path(entries: &[devices::FirmwareEntry]) -> Option<(String, String)> {
    entries
        .iter()
        .find(|entry| {
            entry
                .path
                .as_deref()
                .map(|p| p.to_lowercase().contains("cdrom("))
                .unwrap_or(false)
        })
        .map(|entry| (entry.number.clone(), entry.label.clone()))
}

/// Find an existing firmware entry by its exact label (used when reusing a
/// previously created one-shot entry).
fn find_boot_entry(label: &str) -> Result<Option<String>, String> {
    Ok(devices::firmware_entries()?
        .into_iter()
        .find(|entry| entry.label == label)
        .map(|entry| entry.number))
}

fn set_boot_next(efibootmgr: &str, number: &str) -> Result<(), String> {
    let output = Command::new(efibootmgr)
        .args(["--bootnext", number])
        .output()
        .map_err(|e| format!("failed to spawn {efibootmgr}: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!("efibootmgr --bootnext failed: {}", stderr.trim()))
    }
}

fn reboot_now() -> Result<(), String> {
    let _ = Command::new("sync").status();
    std::fs::write("/proc/sysrq-trigger", "b")
        .map_err(|e| format!("reboot via sysrq failed: {e}"))?;
    Ok(())
}

fn shell_join(argv: &[String]) -> String {
    argv.iter()
        .map(|a| shell_words_quote(a))
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_words_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || "-_/.,:@%+=:".contains(c))
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::{file_node_offset, shorten_boot_option};

    fn node(bytes: &[u8]) -> Vec<u8> {
        bytes.to_vec()
    }

    fn optical_dp() -> Vec<u8> {
        let mut dp = Vec::new();
        dp.extend(node(&[
            0x02, 0x01, 0x0c, 0x00, 0xd0, 0x41, 0x03, 0x0a, 0x00, 0x00, 0x00, 0x00,
        ]));
        dp.extend(node(&[0x01, 0x01, 0x06, 0x00, 0x02, 0x1f]));
        dp.extend(node(&[
            0x03, 0x12, 0x0a, 0x00, 0x02, 0x00, 0xff, 0xff, 0x00, 0x00,
        ]));
        dp
    }

    #[test]
    fn finds_file_node_in_optical_entry() {
        let mut dp = optical_dp();
        dp.extend(node(&[
            0x04, 0x04, 0x0c, 0x00, 0x5c, 0x00, 0x45, 0x00, 0x46, 0x00,
        ]));
        dp.extend(node(&[0x7f, 0xff, 0x04, 0x00]));
        assert_eq!(file_node_offset(&dp), Some(28));
    }

    #[test]
    fn short_form_has_no_file_node() {
        let mut dp = optical_dp();
        dp.extend(node(&[0x7f, 0xff, 0x04, 0x00]));
        assert_eq!(file_node_offset(&dp), None);
    }

    #[test]
    fn empty_file_node_is_still_a_file_node() {
        let mut dp = optical_dp();
        dp.extend(node(&[0x04, 0x04, 0x06, 0x00, 0x00, 0x00]));
        dp.extend(node(&[0x7f, 0xff, 0x04, 0x00]));
        assert_eq!(file_node_offset(&dp), Some(28));
    }

    #[test]
    fn shortens_a_boot_option_and_fixes_its_length() {
        // efivarfs attribute prefix + EFI_LOAD_OPTION around a file-backed
        // optical device path.
        let mut dp = optical_dp();
        dp.extend(node(&[
            0x04, 0x04, 0x0c, 0x00, 0x5c, 0x00, 0x45, 0x00, 0x46, 0x00,
        ]));
        dp.extend(node(&[0x7f, 0xff, 0x04, 0x00]));
        let description = [b'T', 0x00, 0x00, 0x00]; // "T" + NUL, UTF-16LE

        let mut raw = vec![0x07, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00];
        raw.extend_from_slice(&(dp.len() as u16).to_le_bytes());
        raw.extend_from_slice(&description);
        raw.extend_from_slice(&dp);

        let out = shorten_boot_option(&raw).expect("entry with a file node shortens");
        // var attrs + load attrs + new length + description + short dp + end
        assert_eq!(&out[..8], &[0x07, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00]);
        let new_len = u16::from_le_bytes([out[8], out[9]]) as usize;
        assert_eq!(new_len, 32);
        assert_eq!(out.len(), 4 + 4 + 2 + 4 + 32);
        assert_eq!(file_node_offset(&out[4 + 6 + 4..]), None);
        // Already short form: nothing to do.
        assert!(shorten_boot_option(&out).is_none());
    }
}
