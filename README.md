# SplashBootManager

SplashOS's LinuxBoot-style graphical boot manager: a tiny Linux system whose
only job is to present a GTK4/libadwaita boot UI and boot other
distros/operating systems. No systemd, no initramfs, no GRUB — the SplashOS
kernel (with `EFI_STUB`, built-in storage/filesystem drivers) *is* the
bootloader.

## Boot flow

```
firmware (UEFI) -> SplashOS kernel (EFI stub) -> read-only root
  -> seatd -> cage (kiosk Wayland compositor) -> splash-boot-manager
       |  default entry: auto-booted after `timeout` seconds
       |  hold Super key: boot-options menu
       -> Linux target:      kexec -l <kernel> --initrd <...> --command-line <...>; kexec -e
       -> foreign EFI (planned): efibootmgr --bootnext <entry>; reboot
```

## Configuration — Boot Loader Specification

Entries are read from `$BOOT/loader/entries/*.conf` (freedesktop Boot Loader
Specification, type 1) and `$BOOT/loader/loader.conf` (`default`, `timeout`).
This keeps the manager interoperable with systemd-boot/GRUB-style layouts
and makes booting other installed distros a pure data problem: their entries
point at their kernel/initrd/cmdline on disk.

## Phase 1 (now)

- BLS entry parsing (`src/bls.rs`, dependency-free)
- countdown to default boot; Super key opens the menu; picking an entry
  boots it via kexec (`src/boot.rs`)
- `--dry-run` prints the kexec commands instead of executing them
- `--boot-dir <path>` to point at any $BOOT tree (development/testing)

## Later phases

- diagnostics/rescue tooling inside the UI (read-only root)
- foreign EFI boot entries via efibootmgr `--bootnext`
- BuildScripts packaging: `rust` toolchain package + vendored crates
  (kept tiny: gtk4 + libadwaita only, hand-rolled BLS parser instead of
  serde), kiosk integration with cage/seatd

## Development

```sh
cargo test                                   # BLS parser tests (no GTK needed to run logic tests? needs gtk dev libs to link)
cargo run -- --dry-run --boot-dir ./example-boot
```

`example-boot/` provides a sample `$BOOT` tree with BLS entries for
development without a boot environment.

## Testing a built boot-manager partition

`install-test-entries.sh` (dev helper, machine-specific) prepares a partition
built by SplashBuilder's `bootloader` edition for a boot test:

```sh
sudo ./install-test-entries.sh [boot-dir] [splashos-root] [--skip-grub]
# defaults: /mnt/bootmanager /mnt/splashos
```

It snapshots this machine's Debian + SplashOS kernels into the boot tree,
writes the BLS `loader/` tree for them, and appends a
"Splash Boot Manager (dev)" entry to the host GRUB (`/etc/grub.d/40_custom` +
`update-grub`, idempotent). Re-run it after every rebuild — the builder wipes
the partition. Until the graphical manager is packaged, the target's
placeholder `/sbin/init` (edition config in the `configuration` repo) kexecs
the default BLS entry after the loader.conf timeout.
