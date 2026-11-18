//! Boot execution: kexec into Linux targets and hand foreign EFI media to
//! the firmware.
//!
//! Linux targets use kexec, which needs no firmware reboot and keeps the
//! whole flow in-process. Media that only carries an EFI application
//! (Windows/Ubuntu installers) is booted by creating a one-shot firmware
//! boot entry (`efibootmgr --bootnext`) and rebooting.

use crate::bls::{self, BootEntry};
use crate::devices::{self, BootDevice, BootKind};
use std::fs;
use std::path::Path;
use std::process::Command;

fn kexec_bin() -> String {
    first_existing(&["/usr/sbin/kexec", "/sbin/kexec", "/usr/bin/kexec"])
}

fn efibootmgr_bin() -> String {
    first_existing(&["/usr/sbin/efibootmgr", "/sbin/efibootmgr", "/usr/bin/efibootmgr"])
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
    let mut load = vec![
        kexec_bin(),
        "-l".to_string(),
        entry.linux.clone(),
    ];
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

/// Boot a hot-plugged device.
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
        BootKind::Firmware { model } => firmware_boot(model, dry_run),
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

    let mut load = vec![
        kexec_bin(),
        "-l".to_string(),
        kernel.display().to_string(),
    ];
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
fn efi_boot(
    device: &BootDevice,
    loader: &str,
    part: u32,
    dry_run: bool,
) -> Result<(), String> {
    if !Path::new("/sys/firmware/efi").exists() {
        return Err(
            "UEFI firmware boot is unavailable (this system booted in BIOS mode)".to_string(),
        );
    }

    let efibootmgr = efibootmgr_bin();
    let label = format!("SplashOS {}", device.title);

    if dry_run {
        println!(
            "dry-run: {efibootmgr} -c -d {} -p {part} -L '{}' -l '{}'",
            device.disk, label, loader
        );
        println!("dry-run: {efibootmgr} --bootnext <new entry>");
        println!("dry-run: sync; echo b > /proc/sysrq-trigger");
        return Ok(());
    }

    ensure_efivars()?;
    let number = match find_boot_entry(&efibootmgr, &label)? {
        Some(number) => number,
        None => {
            let output = Command::new(&efibootmgr)
                .args([
                    "-c",
                    "-d",
                    &device.disk,
                    "-p",
                    &part.to_string(),
                    "-L",
                    &label,
                    "-l",
                    loader,
                ])
                .output()
                .map_err(|e| format!("failed to spawn {efibootmgr}: {e}"))?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(format!(
                    "efibootmgr could not create a boot entry: {}",
                    stderr.trim()
                ));
            }
            find_boot_entry(&efibootmgr, &label)?
                .ok_or_else(|| "efibootmgr did not create a boot entry".to_string())?
        }
    };

    set_boot_next(&efibootmgr, &number)?;
    reboot_now()
}

/// Optical media without a kexec-able kernel: the firmware already owns a
/// boot option for the drive. Match it by model and hand over the next boot.
fn firmware_boot(model: &str, dry_run: bool) -> Result<(), String> {
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

    if dry_run {
        println!("dry-run: {efibootmgr}  # find a Boot#### option matching '{model}'");
        println!("dry-run: {efibootmgr} --bootnext <matching entry>");
        println!("dry-run: sync; echo b > /proc/sysrq-trigger");
        return Ok(());
    }

    ensure_efivars()?;
    let entries = boot_entries(&efibootmgr)?;
    let (number, label) = entries
        .iter()
        .find(|(_, label)| label.to_lowercase().contains(&needle))
        .ok_or_else(|| format!("the firmware has no boot option for “{model}”"))?;

    println!("firmware boot: {label}");
    set_boot_next(&efibootmgr, number)?;
    reboot_now()
}

fn ensure_efivars() -> Result<(), String> {
    let dir = Path::new("/sys/firmware/efi/efivars");
    if dir.exists() {
        return Ok(());
    }
    // The directory is a sysfs mountpoint placeholder on UEFI systems; if it
    // is missing the firmware interface is not available at all.
    let _ = fs::create_dir_all(dir);
    let status = Command::new(devices::mount_bin())
        .args(["-t", "efivarfs", "efivarfs", "/sys/firmware/efi/efivars"])
        .status()
        .map_err(|e| format!("could not mount efivarfs: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err("could not mount efivarfs (no EFI runtime variables?)".to_string())
    }
}

/// Parse `efibootmgr` output into (Boot####, label) pairs.
fn boot_entries(efibootmgr: &str) -> Result<Vec<(String, String)>, String> {
    let output = Command::new(efibootmgr)
        .output()
        .map_err(|e| format!("failed to spawn {efibootmgr}: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("efibootmgr failed: {}", stderr.trim()));
    }

    let mut entries = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let line = line.trim_start();
        let Some(rest) = line.strip_prefix("Boot") else {
            continue;
        };
        if rest.len() < 4 {
            continue;
        }
        let (number, tail) = rest.split_at(4);
        if !number.chars().all(|c| c.is_ascii_hexdigit()) {
            continue; // BootCurrent/BootOrder/...
        }
        let label = tail.trim_start_matches('*').trim();
        entries.push((number.to_ascii_uppercase(), label.to_string()));
    }
    Ok(entries)
}

fn find_boot_entry(efibootmgr: &str, label: &str) -> Result<Option<String>, String> {
    Ok(boot_entries(efibootmgr)?
        .into_iter()
        .find(|(_, existing)| existing == label)
        .map(|(number, _)| number))
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
    argv.iter().map(|a| shell_words_quote(a)).collect::<Vec<_>>().join(" ")
}

fn shell_words_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    if s.chars().all(|c| c.is_ascii_alphanumeric() || "-_/.,:@%+=:".contains(c)) {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}
