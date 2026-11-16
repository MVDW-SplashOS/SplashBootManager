//! Boot Loader Specification (BLS) type-1 entry parsing.
//!
//! Entries live under `$BOOT/loader/entries/<id>.conf` and look like:
//!
//! ```ini
//! title  SplashOS 0.0.1-dev (Kokomi)
//! linux  /splashos/7.1.8/linux
//! initrd /splashos/7.1.8/initrd
//! options root=UUID=... ro
//! ```
//!
//! `$BOOT/loader/loader.conf` selects the default entry and the menu
//! timeout (`default`, `timeout`).
//!
//! Deliberately dependency-free: a tiny hand-rolled parser keeps the
//! vendored-crate tree small for offline from-source builds.

use std::path::Path;

/// A bootable entry (BLS type 1).
#[derive(Debug, Clone)]
pub struct BootEntry {
    /// Entry id: the config file name without its .conf suffix.
    pub id: String,
    /// Human title (BLS `title`), falls back to the id.
    pub title: String,
    /// Absolute (from $BOOT) or /-absolute kernel path (BLS `linux`).
    pub linux: String,
    /// Initrds in BLS order (every `initrd` key appends one).
    pub initrd: Vec<String>,
    /// Kernel command line (BLS `options`), empty when absent.
    pub options: String,
}

fn read_lines(path: &Path) -> Vec<String> {
    match std::fs::read_to_string(path) {
        Ok(content) => content.lines().map(str::to_string).collect(),
        Err(_) => Vec::new(),
    }
}

/// Parse `loader.conf` -> (default entry id, timeout in seconds).
pub fn parse_loader_conf(boot_dir: &Path) -> (Option<String>, u32) {
    let mut default = None;
    let mut timeout: u32 = 5;
    for line in read_lines(&boot_dir.join("loader").join("loader.conf")) {
        let line = line.trim();
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        if let Some((key, value)) = line.split_once(' ') {
            match key.trim() {
                "default" => default = Some(value.trim().trim_end_matches('*').to_string()),
                "timeout" => timeout = value.trim().parse().unwrap_or(timeout),
                _ => {}
            }
        }
    }
    (default, timeout)
}

/// Scan `loader/entries/*.conf` in BLS order (sorted by file name).
/// Entries without a `linux` key are skipped.
pub fn scan_entries(boot_dir: &Path) -> Vec<BootEntry> {
    let dir = boot_dir.join("loader").join("entries");
    let mut ids: Vec<String> = std::fs::read_dir(&dir)
        .map(|rd| {
            rd.filter_map(Result::ok)
                .filter(|e| e.path().extension().map_or(false, |x| x == "conf"))
                .filter_map(|e| e.file_name().to_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    ids.sort();

    ids.into_iter()
        .filter_map(|file_name| {
            let mut entry = BootEntry {
                id: file_name.trim_end_matches(".conf").to_string(),
                title: String::new(),
                linux: String::new(),
                initrd: Vec::new(),
                options: String::new(),
            };
            for line in read_lines(&dir.join(&file_name)) {
                let line = line.trim();
                if line.starts_with('#') || line.is_empty() {
                    continue;
                }
                if let Some((key, value)) = line.split_once(' ') {
                    let value = value.trim();
                    match key.trim() {
                        "title" => entry.title = value.to_string(),
                        "linux" => entry.linux = value.to_string(),
                        "initrd" => entry.initrd.push(value.to_string()),
                        "options" => entry.options = value.to_string(),
                        _ => {}
                    }
                }
            }
            if entry.linux.is_empty() {
                None
            } else {
                if entry.title.is_empty() {
                    entry.title = entry.id.clone();
                }
                Some(entry)
            }
        })
        .collect()
}

/// Resolve which entry boots by default (loader.conf `default`, glob-style
/// suffix match per BLS, otherwise the first entry).
pub fn default_entry<'a>(entries: &'a [BootEntry], default: &Option<String>) -> Option<&'a BootEntry> {
    let default_id = default.as_deref()?;
    entries
        .iter()
        .find(|e| e.id == default_id || e.id.ends_with(default_id) || e.id.starts_with(default_id))
        .or_else(|| entries.first())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_entries_and_loader_conf() {
        let tmp = std::env::temp_dir().join(format!("sbm-test-{}", std::process::id()));
        let entries_dir = tmp.join("loader").join("entries");
        std::fs::create_dir_all(&entries_dir).unwrap();
        std::fs::write(
            entries_dir.join("splashos-0.0.1.conf"),
            "title SplashOS\nlinux /7.1.8/linux\ninitrd /7.1.8/initrd\noptions root=UUID=x ro\n",
        )
        .unwrap();
        std::fs::write(
            entries_dir.join("rescue.conf"),
            "title Rescue\nlinux /rescue/linux\n",
        )
        .unwrap();
        std::fs::create_dir_all(tmp.join("loader")).unwrap();
        std::fs::write(
            tmp.join("loader").join("loader.conf"),
            "timeout 3\ndefault splashos-*\n",
        )
        .unwrap();

        let entries = scan_entries(&tmp);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].id, "rescue");
        assert_eq!(entries[1].title, "SplashOS");
        assert_eq!(entries[1].initrd, vec!["/7.1.8/initrd".to_string()]);

        let (default, timeout) = parse_loader_conf(&tmp);
        assert_eq!(timeout, 3);
        let chosen = default_entry(&entries, &default).unwrap();
        assert_eq!(chosen.id, "splashos-0.0.1");

        std::fs::remove_dir_all(&tmp).unwrap();
    }
}
