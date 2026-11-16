//! Boot execution: kexec into Linux targets.
//!
//! Foreign EFI entries (Windows, macOS, other firmware-booted systems) come
//! later via `efibootmgr --bootnext`; Linux targets use kexec, which needs
//! no reboot of the firmware and keeps the whole flow in-process.

use crate::bls::BootEntry;
use std::process::Command;

/// Build the argv vectors for the two-step kexec boot:
/// `kexec -l <kernel> [--initrd <f>]... --command-line <options>` then
/// `kexec -e`.
pub fn kexec_argv(entry: &BootEntry) -> Vec<Vec<String>> {
    let mut load = vec![
        "kexec".to_string(),
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
    vec![load, vec!["kexec".to_string(), "-e".to_string()]]
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
