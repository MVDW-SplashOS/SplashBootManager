//! Detection of everything the picker can boot: removable media (USB
//! sticks, CD/DVD), other installed operating systems (UEFI boot entries and
//! ESP loaders on internal disks) and other Linux roots.
//!
//! kexec can only load Linux kernels, so every non-Linux target goes through
//! the firmware instead: an existing `Boot####` entry (Windows Boot Manager,
//! a distro's GRUB, ...) is selected with `efibootmgr --bootnext` and the
//! machine warm-reboots into it. That route needs UEFI — in BIOS mode only
//! kexec-able targets exist and none of the UEFI discs are shown.
//!
//! The boot-manager phase normally runs *without* udev (the dev init starts
//! udevd only for the graphical menu), so this watches sysfs directly. Once
//! a second it enumerates `/sys/class/block`, keeps the devices that hang off
//! a USB bus (or an optical drive, or — on UEFI systems — internal disks with
//! a bootable tree), read-only mounts them and looks for a boot configuration:
//!
//!   * a BLS tree (`loader/entries/*.conf`)              -> kexec
//!   * a GRUB config (`boot/grub/grub.cfg`, ...)         -> kexec
//!   * an isolinux/syslinux config                       -> kexec
//!   * `EFI/BOOT/BOOT*.EFI` (ESP loaders, media)         -> firmware / kexec
//!   * the firmware's own `Boot####` entries             -> firmware
//!
//! Mounting is the only reliable way to tell a bootable USB stick from a
//! random data stick, but it is too slow (and can spin up an optical drive
//! for seconds) to sit in the GTK main loop. `spawn_watcher` therefore runs
//! the scan on a thread and hands complete lists to the UI, which rebuilds
//! its disc row only when the list actually changed. Results are cached by
//! (device node, size, filesystem UUID) so an unchanged device is never
//! mounted twice.
//!
//! The module deliberately sticks to sysfs, `blkid` and `mount`: the image
//! has no libudev/udisks and the kernel is built with the storage drivers
//! (USB_STORAGE, BLK_DEV_SR, ISO9660, VFAT) built in, not modular.

use crate::bls::{self, BootEntry};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;
use std::thread;
use std::time::{Duration, Instant};

const SYS_BLOCK: &str = "/sys/class/block";
const PROBE_ROOT: &str = "/run/sbm-probe";
/// How often sysfs is polled while the menu is open.
const POLL_INTERVAL: Duration = Duration::from_secs(1);
/// A device that mounted but had no boot configuration is retried this
/// often (media can be replaced in an optical drive without a size change).
const NEGATIVE_RETRY: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum DeviceKind {
    /// An internal disk / firmware boot entry (an installed OS). Sorted
    /// first: internal boot choices before removable media.
    Internal,
    /// USB mass storage (stick, external disk) or any other removable disk.
    Usb,
    /// Optical drive (CD/DVD).
    Optical,
}

impl DeviceKind {
    pub fn label(self) -> &'static str {
        match self {
            DeviceKind::Internal => "UEFI",
            DeviceKind::Usb => "USB",
            DeviceKind::Optical => "CD-ROM",
        }
    }
}

/// How a detected device can be booted.
#[derive(Clone, Debug, PartialEq)]
pub enum BootKind {
    /// A Linux kernel/initrd pair was found on the media. The kernel is
    /// loaded while the media is mounted, then the media is unmounted and
    /// `kexec -e` runs.
    Kexec {
        fs: String,
        entries: Vec<BootEntry>,
        default: Option<String>,
    },
    /// An EFI application on a partition (`EFI/BOOT/BOOTX64.EFI`, ...).
    /// Booted by handing the next boot to the firmware (`efibootmgr`).
    Efi { loader: String, part: u32 },
    /// An existing firmware boot entry (`Boot0003 Windows Boot Manager`):
    /// selected with `efibootmgr --bootnext` and a warm reboot. This is the
    /// route for every installed non-Linux OS.
    FirmwareEntry { number: String },
    /// Bootable optical media with no kexec-able kernel. Firmware boot
    /// entries are matched against the drive model.
    Firmware { model: String },
}

/// One `Boot####` entry from `efibootmgr` output.
#[derive(Clone, Debug, PartialEq)]
pub struct FirmwareEntry {
    /// Hex entry number, uppercase (e.g. `0003`).
    pub number: String,
    /// Human label, e.g. `Windows Boot Manager`.
    pub label: String,
    /// Loader path from the device path (`File(\EFI\...EFI)`), normalized
    /// for dedupe against ESP scans; `None` when the entry points at
    /// something else (firmware UI, shell, network).
    pub file: Option<String>,
}

/// A bootable device shown in the picker.
#[derive(Clone, Debug, PartialEq)]
pub struct BootDevice {
    /// Stable widget identity (device node + media identity).
    pub key: String,
    pub devnode: String,
    /// Whole-disk device node (`/dev/sdb` for `/dev/sdb1`); efibootmgr and
    /// sysfs model lookups use this.
    pub disk: String,
    /// Filesystem UUID/serial (used to keep firmware boot entries unique).
    pub uuid: String,
    pub kind: DeviceKind,
    pub title: String,
    /// Small line under the caption, e.g. "USB · 14.9 GB".
    pub detail: String,
    pub boot: BootKind,
}

struct BlkInfo {
    fstype: String,
    label: String,
    uuid: String,
}

impl BlkInfo {
    fn fingerprint(&self, size: u64) -> String {
        format!("{size}|{}|{}|{}", self.fstype, self.uuid, self.label)
    }
}

struct Candidate {
    devnode: String,
    disk: String,
    kind: DeviceKind,
    partition: Option<u32>,
    size: u64,
    model: String,
    devnum: String,
}

struct DiskGroup {
    name: String,
    link: PathBuf,
    whole: Option<PathBuf>,
    parts: Vec<(String, PathBuf)>,
}

/// Start the background watcher. `tx` receives a new list whenever the set
/// of bootable devices changes; the receiver is normally polled from a
/// `glib::timeout_add_local` callback.
pub fn spawn_watcher(tx: Sender<Vec<BootDevice>>) {
    let _ = thread::Builder::new()
        .name("splash-device-watch".to_string())
        .spawn(move || {
            let mut state = ScanState::default();
            let mut last: Option<Vec<BootDevice>> = None;
            loop {
                let list = scan_with_cache(&mut state);
                if last.as_ref() != Some(&list) {
                    if tx.send(list.clone()).is_err() {
                        break; // UI is gone (kexec / exit)
                    }
                    last = Some(list);
                }
                thread::sleep(POLL_INTERVAL);
            }
        });
}

/// One-shot scan (used by `--list-devices` and tests).
pub fn scan() -> Vec<BootDevice> {
    scan_with_cache(&mut ScanState::default())
}

#[derive(Default)]
struct ScanState {
    /// Per-device probe cache (device node -> result).
    media: HashMap<String, CacheEntry>,
    /// Firmware boot entries, refreshed every `FW_REFRESH` (they rarely
    /// change while the menu is open, and efivar reads are not free).
    fw: Option<FwCache>,
}

struct FwCache {
    at: Instant,
    entries: Vec<FirmwareEntry>,
}

/// How often the firmware boot entry list is re-read.
const FW_REFRESH: Duration = Duration::from_secs(10);

struct CacheEntry {
    fingerprint: String,
    device: Option<BootDevice>,
    last_try: Instant,
}

fn scan_with_cache(state: &mut ScanState) -> Vec<BootDevice> {
    let now = Instant::now();
    let busy = busy_devnums();
    let swaps = swap_devnodes();
    let fw = fw_entries_cached(state, now);
    // Loaders already covered by a firmware entry: the ESP scan must not
    // show "Windows Boot Manager" twice (once as Boot0003, once as a
    // scanned partition).
    let fw_files: HashSet<String> = fw.iter().filter_map(|e| e.file.clone()).collect();

    let mut found: Vec<BootDevice> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    for entry in fw {
        let device = BootDevice {
            key: format!("fw|{}", entry.number),
            devnode: String::new(),
            disk: String::new(),
            uuid: String::new(),
            kind: DeviceKind::Internal,
            title: entry.label,
            detail: format!("UEFI \u{b7} Boot{}", entry.number),
            boot: BootKind::FirmwareEntry {
                number: entry.number,
            },
        };
        if seen.insert(device.key.clone()) {
            found.push(device);
        }
    }

    for cand in candidates() {
        if busy.contains(&cand.devnum) || swaps.contains(&cand.devnode) {
            continue;
        }

        let info = blkid(&cand.devnode);
        if info.fstype.is_empty() {
            continue;
        }
        let fp = info.fingerprint(cand.size);

        let device = match state.media.get(&cand.devnode) {
            Some(c) if c.fingerprint == fp => match &c.device {
                Some(d) => Some(d.clone()),
                None if now.duration_since(c.last_try) < NEGATIVE_RETRY => None,
                None => probe_cached(state, &cand, &info, &fp, now),
            },
            _ => probe_cached(state, &cand, &info, &fp, now),
        };

        if let Some(device) = device {
            // An ESP loader already owned by a firmware entry would show
            // twice (e.g. Boot0003 and the same bootmgfw.efi on disk).
            if let BootKind::Efi { loader, .. } = &device.boot {
                if let Some(file) = fw_entries_file(loader) {
                    if fw_files.contains(&file) {
                        continue;
                    }
                }
            }
            if seen.insert(device.key.clone()) {
                found.push(device);
            }
        }
    }

    found.sort_by(|a, b| (a.kind, a.title.to_lowercase()).cmp(&(b.kind, b.title.to_lowercase())));
    found
}

fn fw_entries_cached(state: &mut ScanState, now: Instant) -> Vec<FirmwareEntry> {
    if !uefi_available() {
        return Vec::new();
    }
    if let Some(cached) = &state.fw {
        if now.duration_since(cached.at) < FW_REFRESH {
            return cached.entries.clone();
        }
    }
    let entries = firmware_entries().unwrap_or_default();
    state.fw = Some(FwCache {
        at: now,
        entries: entries.clone(),
    });
    entries
}

fn probe_cached(
    state: &mut ScanState,
    cand: &Candidate,
    info: &BlkInfo,
    fingerprint: &str,
    now: Instant,
) -> Option<BootDevice> {
    let device = probe(cand, info);
    state.media.insert(
        cand.devnode.clone(),
        CacheEntry {
            fingerprint: fingerprint.to_string(),
            device: device.clone(),
            last_try: now,
        },
    );
    device
}

fn probe(cand: &Candidate, info: &BlkInfo) -> Option<BootDevice> {
    // EXFAT/NTFS/UDF are not in this kernel (the media builders use
    // FAT32/iso9660 for bootable images), so an unmountable filesystem is
    // not a boot candidate here.
    let mount = mount_media(&cand.devnode, &info.fstype).ok()?;
    let (boot, preferred_title) = probe_boot_tree(
        mount.path(),
        &info.fstype,
        cand.kind,
        cand.partition,
        &cand.model,
    )?;
    drop(mount);

    let title = if let Some(title) = preferred_title {
        // ESP loaders carry their own identity ("Windows Boot Manager",
        // "Ubuntu") which beats the useless ESP volume label.
        title
    } else if !info.label.trim().is_empty() {
        info.label.trim().to_string()
    } else if !cand.model.trim().is_empty() {
        cand.model.trim().to_string()
    } else {
        match cand.kind {
            DeviceKind::Internal => "UEFI boot".to_string(),
            DeviceKind::Optical => "CD-ROM".to_string(),
            DeviceKind::Usb => "USB drive".to_string(),
        }
    };

    Some(BootDevice {
        key: format!("{}|{}|{}", cand.devnode, cand.size, info.uuid),
        devnode: cand.devnode.clone(),
        disk: cand.disk.clone(),
        uuid: info.uuid.clone(),
        kind: cand.kind,
        title,
        detail: format!("{} · {}", cand.kind.label(), human_size(cand.size)),
        boot,
    })
}

/// Look for a boot configuration on a mounted filesystem. Returns the boot
/// route plus an optional display title that should take precedence over
/// the volume label (ESP loaders know their own name).
fn probe_boot_tree(
    root: &Path,
    fstype: &str,
    kind: DeviceKind,
    partition: Option<u32>,
    model: &str,
) -> Option<(BootKind, Option<String>)> {
    // 1. BLS: the native layout (SplashOS, systemd-boot installs).
    let entries = bls::scan_entries(root);
    if !entries.is_empty() {
        let (default, _timeout) = bls::parse_loader_conf(root);
        return Some((
            BootKind::Kexec {
                fs: fstype.to_string(),
                entries,
                default,
            },
            None,
        ));
    }

    // 2. GRUB / syslinux (distro install and live media).
    let grub_configs = [
        "boot/grub/grub.cfg",
        "boot/grub2/grub.cfg",
        "grub/grub.cfg",
        "grub2/grub.cfg",
    ];
    for rel in grub_configs {
        let cfg = root.join(rel);
        if let Some(entry) = parse_grub(root, &cfg) {
            return Some((
                BootKind::Kexec {
                    fs: fstype.to_string(),
                    entries: vec![entry],
                    default: None,
                },
                None,
            ));
        }
    }

    let syslinux_configs = [
        "isolinux/isolinux.cfg",
        "boot/isolinux/isolinux.cfg",
        "syslinux/syslinux.cfg",
        "syslinux.cfg",
    ];
    for rel in syslinux_configs {
        let cfg = root.join(rel);
        if let Some(entry) = parse_isolinux(root, &cfg) {
            return Some((
                BootKind::Kexec {
                    fs: fstype.to_string(),
                    entries: vec![entry],
                    default: None,
                },
                None,
            ));
        }
    }

    // 3. An EFI application (Windows/Ubuntu installers, UEFI-only media,
    //    installed ESPs).
    if kind == DeviceKind::Internal {
        // Internal ESPs: the full loader set, with distro-aware names
        // (grubx64/shimx64/systemd-boot/bootmgfw), so an installed Windows
        // or GRUB shows up with its own identity.
        if let Some((loader, title)) = find_esp_loader(root) {
            return Some((
                BootKind::Efi {
                    loader,
                    part: partition.unwrap_or(1),
                },
                Some(title),
            ));
        }
        return None;
    }

    if let Some(loader) = find_efi_loader(root) {
        if kind == DeviceKind::Optical {
            // El Torito images are not addressable as files: let the
            // firmware boot the drive via its own boot option.
            return Some((
                BootKind::Firmware {
                    model: model.to_string(),
                },
                None,
            ));
        }
        return Some((
            BootKind::Efi {
                loader,
                part: partition.unwrap_or(1),
            },
            None,
        ));
    }

    None
}

/// Parse the first usable `menuentry` of a GRUB config.
fn parse_grub(root: &Path, cfg: &Path) -> Option<BootEntry> {
    let text = fs::read_to_string(cfg).ok()?;
    let base = cfg
        .parent()
        .and_then(|p| p.strip_prefix(root).ok())
        .unwrap_or(Path::new(""));

    let mut in_entry = false;
    let mut title = String::new();
    let mut kernel: Option<(String, String)> = None;
    let mut initrd: Vec<String> = Vec::new();

    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with("menuentry") {
            if in_entry && kernel.is_some() {
                break;
            }
            if !line.starts_with("menuentry ") {
                continue;
            }
            in_entry = true;
            title = first_quoted(line).unwrap_or_else(|| "Boot media".to_string());
            kernel = None;
            initrd.clear();
            continue;
        }
        if !in_entry {
            continue;
        }

        let mut it = line.splitn(2, char::is_whitespace);
        let key = it.next().unwrap_or("");
        let value = it.next().unwrap_or("").trim();
        match key {
            "linux" | "linuxefi" | "linux16" => {
                if kernel.is_none() && !value.is_empty() {
                    let mut toks = value.splitn(2, char::is_whitespace);
                    let path = unquote(toks.next().unwrap_or(""));
                    let options = toks.next().unwrap_or("").trim();
                    kernel = Some((
                        media_path(base, &expand_vars(&path)),
                        clean_options(&expand_vars(options)),
                    ));
                }
            }
            "initrd" | "initrdefi" | "initrd16" => {
                for path in value.split_whitespace() {
                    initrd.push(media_path(base, &expand_vars(&unquote(path))));
                }
            }
            _ => {}
        }
    }

    let (linux, options) = kernel?;
    Some(BootEntry {
        id: "media".to_string(),
        title: clean_title(&title),
        linux,
        initrd,
        options,
    })
}

/// Parse an isolinux/syslinux config. Paths are relative to the config's
/// directory, which is why `root`/`base` are needed.
fn parse_isolinux(root: &Path, cfg: &Path) -> Option<BootEntry> {
    #[derive(Default)]
    struct Label {
        name: String,
        title: String,
        kernel: String,
        initrd: Vec<String>,
        append: String,
        is_default: bool,
    }

    let text = fs::read_to_string(cfg).ok()?;
    let base = cfg
        .parent()
        .and_then(|p| p.strip_prefix(root).ok())
        .unwrap_or(Path::new(""));

    let mut labels: Vec<Label> = Vec::new();
    let mut current = Label::default();
    let mut default_label: Option<String> = None;

    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut it = line.splitn(2, char::is_whitespace);
        let key = it.next().unwrap_or("");
        let value = it.next().unwrap_or("").trim();
        match key {
            "label" => {
                labels.push(std::mem::take(&mut current));
                current.name = value.to_string();
            }
            "menu" => {
                if let Some(title) = value.strip_prefix("label ") {
                    current.title = title.trim().to_string();
                } else if value == "default" {
                    current.is_default = true;
                }
            }
            "kernel" | "linux" => current.kernel = unquote(value),
            "initrd" => {
                current.initrd = value
                    .split_whitespace()
                    .map(|p| unquote(p))
                    .filter(|p| !p.is_empty())
                    .collect();
            }
            "append" => current.append = expand_vars(value),
            "default" => default_label = Some(value.to_string()),
            _ => {}
        }
    }
    labels.push(current);

    let chosen = default_label
        .as_ref()
        .and_then(|want| labels.iter().find(|l| &l.name == want))
        .or_else(|| labels.iter().find(|l| l.is_default && !l.kernel.is_empty()))
        .or_else(|| labels.iter().find(|l| !l.kernel.is_empty()))?;

    Some(BootEntry {
        id: "media".to_string(),
        title: clean_title(if chosen.title.is_empty() {
            &chosen.name
        } else {
            &chosen.title
        }),
        linux: media_path(base, &chosen.kernel),
        initrd: chosen.initrd.iter().map(|p| media_path(base, p)).collect(),
        options: clean_options(&chosen.append),
    })
}

/// Resolve a kernel/initrd path from a boot config to a media-absolute path
/// (the caller prefixes the mount point at boot time).
fn media_path(base: &Path, path: &str) -> String {
    let path = path.trim();
    if path.starts_with('/') {
        path.to_string()
    } else if base.as_os_str().is_empty() {
        format!("/{path}")
    } else {
        format!("/{}/{}", base.to_string_lossy(), path)
    }
}

fn expand_vars(value: &str) -> String {
    // Debian live's GRUB config uses findiso=${iso_path}; when booting the
    // media itself the path is empty (live-boot then finds the CD/USB).
    value.replace("${iso_path}", "").replace("$iso_path", "")
}

/// Collapse whitespace and drop now-empty assignments such as `findiso=`
/// left behind by expanding `${iso_path}`.
fn clean_options(value: &str) -> String {
    value
        .split_whitespace()
        .filter(|token| !token.ends_with('='))
        .collect::<Vec<_>>()
        .join(" ")
}

fn unquote(value: &str) -> String {
    let value = value.trim();
    let bytes = value.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return value[1..value.len() - 1].to_string();
        }
    }
    value.to_string()
}

fn first_quoted(line: &str) -> Option<String> {
    let mut chars = line.char_indices();
    let (_, quote) = chars.find(|(_, c)| *c == '"' || *c == '\'')?;
    let start = line.find(quote)? + quote.len_utf8();
    let rest = &line[start..];
    let end = rest.find(quote)?;
    Some(rest[..end].to_string())
}

fn clean_title(title: &str) -> String {
    let title = title.trim().trim_start_matches('^');
    if title.is_empty() {
        "Boot media".to_string()
    } else {
        title.to_string()
    }
}

/// Find `EFI/BOOT/BOOT*.EFI` (or the Microsoft fallback) on a mounted media
/// and return the backslash-separated path efibootmgr expects.
fn find_efi_loader(root: &Path) -> Option<String> {
    let efi = find_dir_ci(root, "efi")?;
    let mut found: Vec<(PathBuf, String, String)> = Vec::new();

    if let Some(boot) = find_dir_ci(&efi, "boot") {
        for entry in read_dir_sorted(&boot) {
            let name = entry
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            let lower = name.to_ascii_lowercase();
            if lower.starts_with("boot") && lower.ends_with(".efi") {
                let weight = if lower == "bootx64.efi" { 0 } else { 1 };
                found.push((entry, name, format!("{weight}")));
            }
        }
    }

    if let Some(microsoft) = find_dir_ci(&efi, "microsoft") {
        if let Some(boot) = find_dir_ci(&microsoft, "boot") {
            for entry in read_dir_sorted(&boot) {
                let name = entry
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                if name.eq_ignore_ascii_case("bootmgfw.efi") {
                    found.push((entry, name, "2".to_string()));
                }
            }
        }
    }

    found.sort_by(|a, b| a.2.cmp(&b.2));
    let (path, _file, _) = found.into_iter().next()?;

    // Rebuild the path from the *actual* directory names (FAT is case
    // insensitive, iso9660 is not; efibootmgr wants the on-disk case).
    let parts: Vec<String> = path
        .strip_prefix(root)
        .ok()?
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_string())
        .collect();
    Some(format!("\\{}", parts.join("\\")))
}

fn find_dir_ci(parent: &Path, name: &str) -> Option<PathBuf> {
    for entry in read_dir_sorted(parent) {
        if entry
            .file_name()
            .map(|n| n.to_string_lossy().eq_ignore_ascii_case(name))
            .unwrap_or(false)
            && entry.is_dir()
        {
            return Some(entry);
        }
    }
    None
}

/// True when this system has the UEFI runtime interface (efibootmgr /
/// efivarfs are usable). BIOS systems cannot reboot into a firmware target,
/// so no firmware discs are offered there.
pub fn uefi_available() -> bool {
    Path::new("/sys/firmware/efi").exists()
}

/// Read the firmware's boot entries (`efibootmgr`), ordered like the
/// firmware's own boot menu (`BootOrder`).
pub fn firmware_entries() -> Result<Vec<FirmwareEntry>, String> {
    let bin = first_existing(&[
        "/usr/sbin/efibootmgr",
        "/sbin/efibootmgr",
        "/usr/bin/efibootmgr",
    ]);
    let output = Command::new(bin)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot run {bin}: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("efibootmgr failed: {}", stderr.trim()));
    }
    Ok(parse_efibootmgr(&String::from_utf8_lossy(&output.stdout)))
}

/// Parse `efibootmgr` output. Entry lines look like:
///
/// ```text
/// BootOrder: 0003,0001,0000
/// Boot0000* Firmware UI  	FvVol(7cb8...)
/// Boot0001  Debian       	HD(2,GPT,...)/File(\EFI\DEBIAN\SHIMX64.EFI)
/// Boot0002* Windows Boot Manager	HD(2,...)/File(\EFI\MICROSOFT\BOOT\BOOTMGFW.EFI)
/// ```
///
/// The label ends at the first tab or run of 2+ spaces; everything after it
/// is the device path.
fn parse_efibootmgr(text: &str) -> Vec<FirmwareEntry> {
    let mut boot_order: Vec<String> = Vec::new();
    let mut entries: Vec<FirmwareEntry> = Vec::new();

    for raw in text.lines() {
        let line = raw.trim_end();
        if let Some(order) = line.strip_prefix("BootOrder:") {
            boot_order = order
                .split(',')
                .map(|n| n.trim().to_ascii_uppercase())
                .filter(|n| n.len() == 4 && n.chars().all(|c| c.is_ascii_hexdigit()))
                .collect();
            continue;
        }
        let Some(rest) = line.strip_prefix("Boot") else {
            continue;
        };
        if rest.len() < 5 {
            continue;
        }
        let (number, tail) = rest.split_at(4);
        if !number.chars().all(|c| c.is_ascii_hexdigit()) {
            continue; // BootCurrent/BootNext/Timeout/...
        }
        let tail = tail.strip_prefix('*').unwrap_or(tail);
        // After the number: "* " for active entries, "  " for inactive.
        let tail = tail.trim_start_matches(' ');
        let (label, path) = split_label_path(tail);
        if label.is_empty() {
            continue;
        }
        entries.push(FirmwareEntry {
            number: number.to_ascii_uppercase(),
            label: label.to_string(),
            file: path
                .and_then(|p| device_path_file(&p))
                .map(|p| normalize_loader(&p)),
        });
    }

    // Follow the firmware's own boot order; entries not listed go last.
    entries.sort_by_key(|e| {
        boot_order
            .iter()
            .position(|n| *n == e.number)
            .unwrap_or(usize::MAX)
    });
    entries
}

/// Split `label  HD(...)/File(...)` into (label, device-path tail). The
/// separator is a tab or a run of at least two spaces (labels may contain
/// single spaces: "Windows Boot Manager").
fn split_label_path(tail: &str) -> (&str, Option<&str>) {
    let bytes = tail.as_bytes();
    let mut end = None;
    for (i, window) in bytes.windows(2).enumerate() {
        if window == b"\t\t" || window[0] == b'\t' {
            end = Some(i);
            break;
        }
        if window == b"  " {
            end = Some(i);
            break;
        }
    }
    match end {
        Some(i) => (tail[..i].trim_end(), Some(tail[i..].trim())),
        None => (tail.trim_end(), None),
    }
}

/// Extract the `File(\EFI\...EFI)` path from an efibootmgr device path.
fn device_path_file(path: &str) -> Option<String> {
    let start = path.find("File(")? + 5;
    let rest = &path[start..];
    let end = rest.find(')')?;
    Some(rest[..end].to_string())
}

/// Normalize a loader path (backslashes or slashes, any case) for dedupe
/// comparisons: `"\EFI\Microsoft\Boot\BOOTMGFW.EFI"` and
/// `"/efi/microsoft/boot/bootmgfw.efi"` collapse to the same key.
fn normalize_loader(path: &str) -> String {
    format!(
        "/{}",
        path.trim()
            .replace('\\', "/")
            .trim_start_matches('/')
            .to_lowercase()
    )
}

/// Dedupe helper: the normalized form of a scanned loader path, used to
/// compare it against firmware entry `File(...)` paths.
fn fw_entries_file(loader: &str) -> Option<String> {
    Some(normalize_loader(loader))
}

/// Find the bootable EFI application on an internal ESP and return the
/// backslash path efibootmgr expects plus a display title. Unlike removable
/// media, installed systems are recognized beyond the Microsoft fallback:
/// distro GRUB/shim installs, systemd-boot, rEFInd, Apple loaders.
fn find_esp_loader(root: &Path) -> Option<(String, String)> {
    let efi = find_dir_ci(root, "efi")?;
    let mut found: Vec<(PathBuf, u8, String)> = Vec::new();
    collect_esp_loaders(&efi, 0, &mut found);

    found.sort_by(|a, b| (a.1, &a.2).cmp(&(b.1, &b.2)));
    let (path, _, title) = found.into_iter().next()?;

    // Rebuild the path from the *actual* directory names (FAT is case
    // insensitive; efibootmgr wants the on-disk case).
    let parts: Vec<String> = path
        .strip_prefix(root)
        .ok()?
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_string())
        .collect();
    Some((format!("\\{}", parts.join("\\")), title))
}

/// Walk `EFI/<vendor>/<maybe subdir>/<file>.efi` (depth-limited) and
/// classify every recognized loader with a match weight.
fn collect_esp_loaders(dir: &Path, depth: u8, out: &mut Vec<(PathBuf, u8, String)>) {
    if depth > 2 {
        return;
    }
    let vendor = dir
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();

    for path in read_dir_sorted(dir) {
        if path.is_dir() {
            collect_esp_loaders(&path, depth + 1, out);
            continue;
        }
        let Some(name) = path.file_name().map(|n| n.to_string_lossy().to_lowercase()) else {
            continue;
        };
        if !name.ends_with(".efi") {
            continue;
        }

        let (weight, title) = if name == "bootmgfw.efi" {
            (0u8, "Windows Boot Manager".to_string())
        } else if vendor == "apple" {
            (1u8, "macOS".to_string())
        } else if name.starts_with("shim") {
            (2u8, esp_distro_title(&vendor, "shim"))
        } else if name.starts_with("systemd-boot") {
            (2u8, "systemd-boot".to_string())
        } else if name.starts_with("refind") {
            (2u8, "rEFInd".to_string())
        } else if name.starts_with("grub") {
            (3u8, esp_distro_title(&vendor, "grub"))
        } else if name.starts_with("boot") {
            // EFI/BOOT/BOOTX64.EFI and friends: generic fallback loader.
            (4u8, "EFI Boot".to_string())
        } else {
            continue;
        };
        out.push((path, weight, title));
    }
}

/// Display title for a distro loader living in `EFI/<vendor>/`
/// (`ubuntu` -> "Ubuntu", `pop_os` -> "Pop os", `debian` -> "Debian").
fn esp_distro_title(vendor: &str, loader: &str) -> String {
    let mut title = String::new();
    for word in vendor.split(['_', '-']).filter(|w| !w.is_empty()) {
        let mut chars = word.chars();
        title.extend(
            chars
                .next()
                .map(|c| c.to_ascii_uppercase())
                .into_iter()
                .chain(chars),
        );
        title.push(' ');
    }
    let title = title.trim_end();
    if title.is_empty() || title.eq_ignore_ascii_case("efi") || title.eq_ignore_ascii_case("boot") {
        return format!("UEFI ({loader})");
    }
    title.to_string()
}

fn read_dir_sorted(dir: &Path) -> Vec<PathBuf> {
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .map(|rd| rd.filter_map(Result::ok).map(|e| e.path()).collect())
        .unwrap_or_default();
    entries.sort();
    entries
}

/// Mount a device read-only under /run/sbm-probe. The returned guard
/// unmounts on drop; `umount` consumes it when the caller needs the
/// unmount to happen before a kexec.
pub struct Mount {
    dir: PathBuf,
    mounted: bool,
}

impl Mount {
    pub fn path(&self) -> &Path {
        &self.dir
    }

    pub fn umount(mut self) {
        self.cleanup();
    }

    fn cleanup(&mut self) {
        if !self.mounted {
            return;
        }
        let dir = self.dir.clone();
        self.mounted = false;
        let dir_s = dir.to_string_lossy().to_string();
        if run_quiet(umount_bin(), &[&dir_s], 10).is_err() {
            let _ = run_quiet(umount_bin(), &["-l", &dir_s], 10);
        }
        let _ = fs::remove_dir(&dir);
    }
}

impl Drop for Mount {
    fn drop(&mut self) {
        self.cleanup();
    }
}

pub fn mount_media(devnode: &str, fstype: &str) -> Result<Mount, String> {
    let dir = probe_dir(devnode);
    let dir_s = dir.to_string_lossy().to_string();
    // A previous probe may have been killed mid-mount (e.g. by a kexec).
    let _ = run_quiet(umount_bin(), &[&dir_s], 5);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    match run_quiet(
        mount_bin(),
        &["-o", "ro", "-t", fstype, devnode, &dir_s],
        15,
    ) {
        Ok(()) => Ok(Mount { dir, mounted: true }),
        Err(err) => {
            let _ = fs::remove_dir(&dir);
            Err(err)
        }
    }
}

/// The (deterministic) mount directory used for a device; also handy for
/// printing a dry-run command line.
pub fn probe_dir(devnode: &str) -> PathBuf {
    let name = devnode
        .trim_start_matches("/dev/")
        .replace('/', "_")
        .replace(' ', "_");
    PathBuf::from(PROBE_ROOT).join(name)
}

fn run_quiet(program: &str, args: &[&str], timeout_secs: u64) -> Result<(), String> {
    let mut cmd;
    if timeout_secs > 0 {
        cmd = Command::new("timeout");
        cmd.arg(timeout_secs.to_string()).arg(program);
    } else {
        cmd = Command::new(program);
    }
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    match cmd.status() {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(format!("{program} exited with {status}")),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound && timeout_secs > 0 => {
            // No coreutils timeout in an extremely minimal image: run the
            // command directly.
            let status = Command::new(program)
                .args(args)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map_err(|e| format!("cannot run {program}: {e}"))?;
            if status.success() {
                Ok(())
            } else {
                Err(format!("{program} exited with {status}"))
            }
        }
        Err(err) => Err(format!("cannot run {program}: {err}")),
    }
}

pub fn mount_bin() -> &'static str {
    first_existing(&["/usr/bin/mount", "/bin/mount", "mount"])
}

fn umount_bin() -> &'static str {
    first_existing(&["/usr/bin/umount", "/bin/umount", "umount"])
}

fn blkid_bin() -> &'static str {
    first_existing(&["/sbin/blkid", "/usr/sbin/blkid", "/usr/bin/blkid", "blkid"])
}

fn first_existing(paths: &[&'static str]) -> &'static str {
    for path in paths {
        if Path::new(path).exists() {
            return path;
        }
    }
    paths[paths.len() - 1]
}

/// `blkid -o export` escapes spaces (and backslashes) in values; labels
/// like "d-live 13.7.0 kd amd64" come back as `d-live\ 13.7.0...`.
fn unescape_blkid(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some(' ') => out.push(' '),
                Some('\\') => out.push('\\'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn blkid(devnode: &str) -> BlkInfo {
    let mut info = BlkInfo {
        fstype: String::new(),
        label: String::new(),
        uuid: String::new(),
    };
    let Ok(output) = Command::new(blkid_bin())
        .args(["-o", "export", devnode])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    else {
        return info;
    };
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key {
            "TYPE" => info.fstype = value.to_string(),
            "LABEL" => info.label = unescape_blkid(value),
            "UUID" => info.uuid = value.to_string(),
            _ => {}
        }
    }
    info
}

fn busy_devnums() -> HashSet<String> {
    let mut busy = HashSet::new();
    if let Ok(text) = fs::read_to_string("/proc/self/mountinfo") {
        for line in text.lines() {
            if let Some(field) = line.split_whitespace().nth(2) {
                busy.insert(field.to_string());
            }
        }
    }
    busy
}

fn swap_devnodes() -> HashSet<String> {
    let mut swaps = HashSet::new();
    if let Ok(text) = fs::read_to_string("/proc/swaps") {
        for line in text.lines().skip(1) {
            if let Some(path) = line.split_whitespace().next() {
                swaps.insert(path.to_string());
            }
        }
    }
    swaps
}

fn candidates() -> Vec<Candidate> {
    let mut groups: BTreeMap<String, DiskGroup> = BTreeMap::new();

    let Ok(rd) = fs::read_dir(SYS_BLOCK) else {
        return Vec::new();
    };
    for entry in rd.filter_map(Result::ok) {
        let name = entry.file_name().to_string_lossy().to_string();
        let link = PathBuf::from(SYS_BLOCK).join(&name);
        let is_partition = link.join("partition").exists();

        let disk = if is_partition {
            match fs::canonicalize(&link)
                .ok()
                .and_then(|p| p.parent().map(|p| p.to_path_buf()))
                .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
            {
                Some(disk) => disk,
                None => continue,
            }
        } else {
            name.clone()
        };

        let group = groups.entry(disk.clone()).or_insert_with(|| DiskGroup {
            name: disk.clone(),
            link: PathBuf::from(SYS_BLOCK).join(&disk),
            whole: None,
            parts: Vec::new(),
        });
        if is_partition {
            group.parts.push((name, link));
        } else {
            group.whole = Some(link);
        }
    }

    let mut out = Vec::new();
    for group in groups.values() {
        if ignored_disk(&group.name) {
            continue;
        }

        let optical = group.name.starts_with("sr");
        let disk_path = fs::canonicalize(&group.link).unwrap_or_else(|_| group.link.clone());
        let path_s = disk_path.to_string_lossy();
        let removable = read_trim(&group.link.join("removable")) == "1";
        let usb = usb_ancestor(&path_s) || removable;
        let kind = if optical {
            DeviceKind::Optical
        } else if usb {
            DeviceKind::Usb
        } else if uefi_available() {
            // Internal disks carry installed operating systems (ESP loaders,
            // other Linux roots). Only interesting on UEFI systems — in BIOS
            // mode nothing on them can be booted by the firmware route and
            // the installed OSes are unreachable for kexec.
            DeviceKind::Internal
        } else {
            continue;
        };
        let model = device_model(&group.link);
        let disk_devnode = format!("/dev/{}", group.name);
        let disk_size = read_size(&group.link);
        if disk_size == 0 {
            continue;
        }

        if optical {
            let devnum = read_trim(&group.link.join("dev"));
            out.push(Candidate {
                devnode: disk_devnode,
                disk: format!("/dev/{}", group.name),
                kind,
                partition: None,
                size: disk_size,
                model: model.clone(),
                devnum,
            });
            continue;
        }

        // A hybrid image dd'ed to a stick exposes iso9660 on the *whole*
        // disk and a partition table with an EFI partition next to it. The
        // boot config lives on the iso9660 filesystem, so prefer the whole
        // disk and ignore its partitions in that case.
        if kind == DeviceKind::Usb {
            let whole_info = blkid(&disk_devnode);
            if whole_info.fstype == "iso9660" {
                let devnum = read_trim(&group.link.join("dev"));
                out.push(Candidate {
                    devnode: disk_devnode,
                    disk: format!("/dev/{}", group.name),
                    kind,
                    partition: None,
                    size: disk_size,
                    model,
                    devnum,
                });
                continue;
            }
        }

        if kind == DeviceKind::Internal && group.parts.is_empty() {
            // A partitionless internal disk is not a boot target.
            continue;
        }

        if group.parts.is_empty() {
            let devnum = read_trim(&group.link.join("dev"));
            out.push(Candidate {
                devnode: disk_devnode,
                disk: format!("/dev/{}", group.name),
                kind,
                partition: None,
                size: disk_size,
                model,
                devnum,
            });
            continue;
        }

        for (part, link) in &group.parts {
            let size = read_size(link);
            if size == 0 {
                continue;
            }
            let partition = read_trim(&link.join("partition")).parse::<u32>().ok();
            out.push(Candidate {
                devnode: format!("/dev/{part}"),
                disk: format!("/dev/{}", group.name),
                kind,
                partition,
                size,
                model: model.clone(),
                devnum: read_trim(&link.join("dev")),
            });
        }
    }

    out
}

fn ignored_disk(name: &str) -> bool {
    const PREFIXES: [&str; 10] = [
        "loop", "ram", "zram", "dm-", "md", "nbd", "fd", "mtdblock", "drbd", "rbd",
    ];
    PREFIXES.iter().any(|p| name.starts_with(p))
}

fn usb_ancestor(path: &str) -> bool {
    path.split('/').any(|component| {
        component
            .strip_prefix("usb")
            .map(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()))
            .unwrap_or(false)
    })
}

fn read_trim(path: &Path) -> String {
    fs::read_to_string(path)
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

fn read_size(link: &Path) -> u64 {
    read_trim(&link.join("size"))
        .parse::<u64>()
        .unwrap_or(0)
        .saturating_mul(512)
}

fn device_model(link: &Path) -> String {
    let model = read_trim(&link.join("device").join("model"));
    let vendor = read_trim(&link.join("device").join("vendor"));
    let model = model.trim();
    let vendor = vendor.trim();
    match (vendor.is_empty(), model.is_empty()) {
        (true, true) => String::new(),
        (true, false) => model.to_string(),
        (false, true) => vendor.to_string(),
        (false, false) => format!("{vendor} {model}"),
    }
}

fn human_size(bytes: u64) -> String {
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    let b = bytes as f64;
    if b >= GIB {
        format!("{:.1} GB", b / GIB)
    } else if b >= MIB {
        format!("{:.0} MB", b / MIB)
    } else {
        format!("{} KB", (b / 1024.0).round().max(1.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "sbm-devices-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn parses_grub_and_isolinux() {
        let root = temp_dir("configs");
        fs::create_dir_all(root.join("boot/grub")).unwrap();
        fs::write(
            root.join("boot/grub/grub.cfg"),
            "set timeout=5\n\
             menuentry \"Live system (amd64)\" {\n\
             \tlinux /live/vmlinuz boot=live components findiso=${iso_path}\n\
             \tinitrd /live/initrd.img\n\
             }\n\
             menuentry \"Second\" { linux /live/vmlinuz2; }\n",
        )
        .unwrap();

        let grub = parse_grub(&root, &root.join("boot/grub/grub.cfg")).unwrap();
        assert_eq!(grub.title, "Live system (amd64)");
        assert_eq!(grub.linux, "/live/vmlinuz");
        assert_eq!(grub.initrd, vec!["/live/initrd.img".to_string()]);
        assert!(grub.options.contains("boot=live"));
        assert!(!grub.options.contains("iso_path"));

        // isolinux paths are relative to the config's directory.
        fs::create_dir_all(root.join("isolinux")).unwrap();
        fs::write(
            root.join("isolinux/isolinux.cfg"),
            "default live\n\
             label live\n\
             \tmenu label ^Live system\n\
             \tkernel vmlinuz\n\
             \tinitrd initrd.img\n\
             \tappend boot=live components quiet\n",
        )
        .unwrap();
        let sys = parse_isolinux(&root, &root.join("isolinux/isolinux.cfg")).unwrap();
        assert_eq!(sys.title, "Live system");
        assert_eq!(sys.linux, "/isolinux/vmlinuz");
        assert_eq!(sys.initrd, vec!["/isolinux/initrd.img".to_string()]);
        assert_eq!(sys.options, "boot=live components quiet");

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn parses_efibootmgr_entries() {
        let text = "BootCurrent: 0002\n\
                    Timeout: 1 seconds\n\
                    BootOrder: 0002,0000\n\
                    Boot0000* Firmware UI  \tFvVol(7cb8bdc9)\n\
                    Boot0001  Debian  \tHD(2,GPT,1234)/File(\\EFI\\debian\\shimx64.EFI)\n\
                    Boot0002* Windows Boot Manager\tHD(2,GPT,5678)/File(\\EFI\\microsoft\\Boot\\bootmgfw.EFI)\n\
                    Boot0003* Orphan Entry\tHD(1,GPT,9abc)\n";

        let entries = parse_efibootmgr(text);
        assert_eq!(entries.len(), 4);

        // Ordered like the firmware's boot menu (BootOrder first, then
        // unlisted entries in output order), labels keep their spaces,
        // inactive entries (no '*') are still listed.
        let numbers: Vec<&str> = entries.iter().map(|e| e.number.as_str()).collect();
        assert_eq!(numbers, vec!["0002", "0000", "0001", "0003"]);

        assert_eq!(entries[0].label, "Windows Boot Manager");
        assert_eq!(
            entries[0].file.as_deref(),
            Some("/efi/microsoft/boot/bootmgfw.efi")
        );
        assert_eq!(entries[1].label, "Firmware UI");
        assert_eq!(entries[1].file, None);
        assert_eq!(entries[2].label, "Debian");
        assert_eq!(entries[2].file.as_deref(), Some("/efi/debian/shimx64.efi"));

        // Duplicate ESP loaders collapse: a scanned bootmgfw.efi matches the
        // Boot0002 File path.
        assert_eq!(
            normalize_loader("\\EFI\\Microsoft\\Boot\\BOOTMGFW.EFI"),
            entries[0].file.clone().unwrap()
        );
    }

    #[test]
    fn finds_esp_loaders_with_distro_titles() {
        let win = temp_dir("esp-win");
        fs::create_dir_all(win.join("EFI/Microsoft/Boot")).unwrap();
        fs::write(win.join("EFI/Microsoft/Boot/bootmgfw.efi"), b"x").unwrap();
        fs::create_dir_all(win.join("EFI/BOOT")).unwrap();
        fs::write(win.join("EFI/BOOT/BOOTX64.EFI"), b"x").unwrap();
        // Microsoft wins over the generic fallback.
        assert_eq!(
            find_esp_loader(&win),
            Some((
                "\\EFI\\Microsoft\\Boot\\bootmgfw.efi".to_string(),
                "Windows Boot Manager".to_string()
            ))
        );
        fs::remove_dir_all(&win).unwrap();

        let distro = temp_dir("esp-distro");
        fs::create_dir_all(distro.join("EFI/ubuntu")).unwrap();
        fs::write(distro.join("EFI/ubuntu/shimx64.efi"), b"x").unwrap();
        assert_eq!(
            find_esp_loader(&distro)
                .map(|(_, title)| title)
                .unwrap_or_default(),
            "Ubuntu"
        );
        fs::remove_dir_all(&distro).unwrap();

        let systemd = temp_dir("esp-sd");
        fs::create_dir_all(systemd.join("EFI/systemd")).unwrap();
        fs::write(systemd.join("EFI/systemd/systemd-bootx64.efi"), b"x").unwrap();
        assert_eq!(
            find_esp_loader(&systemd)
                .map(|(_, title)| title)
                .unwrap_or_default(),
            "systemd-boot"
        );
        fs::remove_dir_all(&systemd).unwrap();
    }

    #[test]
    fn finds_efi_loader_case_insensitively() {
        let root = temp_dir("efi");
        fs::create_dir_all(root.join("EFI/BOOT")).unwrap();
        fs::write(root.join("EFI/BOOT/BOOTX64.EFI"), b"x").unwrap();
        assert_eq!(
            find_efi_loader(&root),
            Some("\\EFI\\BOOT\\BOOTX64.EFI".to_string())
        );

        let lower = temp_dir("efi-lower");
        fs::create_dir_all(lower.join("efi/boot")).unwrap();
        fs::write(lower.join("efi/boot/bootx64.efi"), b"x").unwrap();
        assert_eq!(
            find_efi_loader(&lower),
            Some("\\efi\\boot\\bootx64.efi".to_string())
        );

        fs::remove_dir_all(&root).unwrap();
        fs::remove_dir_all(&lower).unwrap();
    }

    #[test]
    fn detects_bls_tree() {
        let root = temp_dir("bls");
        fs::create_dir_all(root.join("loader/entries")).unwrap();
        fs::write(
            root.join("loader/entries/splashos.conf"),
            "title SplashOS\nlinux /linux\ninitrd /initrd\noptions root=UUID=x ro\n",
        )
        .unwrap();
        fs::write(root.join("loader/loader.conf"), "default splashos\n").unwrap();

        match probe_boot_tree(&root, "vfat", DeviceKind::Usb, Some(1), "") {
            Some((
                BootKind::Kexec {
                    entries, default, ..
                },
                _title,
            )) => {
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0].title, "SplashOS");
                assert_eq!(default.as_deref(), Some("splashos"));
            }
            other => panic!("unexpected: {other:?}"),
        }

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn formats_sizes() {
        assert_eq!(human_size(16 * 1024 * 1024 * 1024), "16.0 GB");
        assert_eq!(human_size(700 * 1024 * 1024), "700 MB");
    }
}
