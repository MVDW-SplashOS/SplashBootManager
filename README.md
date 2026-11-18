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
       -> USB (EFI app):     efibootmgr -c ...; efibootmgr --bootnext <entry>; reboot
       -> optical (EFI):     efibootmgr --bootnext <firmware entry for the drive>; reboot
```

## Removable media

While the menu is open the manager polls sysfs once a second and looks for
bootable USB/optical media. Newly plugged devices appear next to the internal
BLS entries, with a USB-stick or CD icon and the volume label (or drive
model) underneath; unplugging removes them again. Detection runs on a worker
thread because it read-only mounts each media to inspect it, and it caches
results by (device node, size, filesystem UUID) so nothing is mounted twice.

How a device is booted depends on what is on it:

- a BLS tree (`loader/entries/*.conf`) — kexec (the media is mounted, the
  kernel/initrd are loaded, the media is unmounted, then `kexec -e`)
- a GRUB or isolinux/syslinux config — the first/default menu entry is
  kexec'ed the same way (live Linux discs/installers)
- only `EFI/BOOT/BOOT*.EFI` (Windows/Ubuntu installers) — a one-shot firmware
  boot entry is created (`efibootmgr --bootnext`) and the machine reboots
  into it; optical media use the firmware's existing boot option for the
  drive
- anything else (exFAT/NTFS/UDF-only data media) is not bootable and is not
  shown

`--list-devices` prints the scanner's result (device, route, parsed boot
entries) without starting GTK; it is the easiest way to debug a stick that
does not show up.

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
- hotplug detection of bootable USB/optical media (`src/devices.rs`)
- `--dry-run` prints the boot commands instead of executing them
- `--boot-dir <path>` to point at any $BOOT tree (development/testing)
- `--list-devices` dumps the hotplug scanner's results

## Later phases

- diagnostics/rescue tooling inside the UI (read-only root)
- BuildScripts packaging: `rust` toolchain package + vendored crates
  (kept tiny: gtk4 + libadwaita only, hand-rolled BLS parser instead of
  serde), kiosk integration with cage/seatd

## Development

```sh
cargo test                                   # BLS parser + device/GRUB/isolinux parser tests
cargo run -- --dry-run --boot-dir ./example-boot
sudo ./target/debug/splash-boot-manager --list-devices   # what the hotplug scanner sees
```

`example-boot/` provides a sample `$BOOT` tree with BLS entries for
development without a boot environment.

## Boot logo

Two assets ship in `assets/`:

- `boot-logo.raw.xz` — the fbdev splash (`splash-logo.c`), a `SPLASHLOGO2`
  frame array: 28-byte header (`SPLASHLOGO2\0`, u32 LE width, height, frame
  count, frame delay in ms) followed by 560×531 BGR24 frames composited over
  black, 74 frames at 40 ms (25 fps). It is stored xz-compressed because the
  flat frame array is 66 MB; the build script decompresses it to
  `/usr/share/splashos/boot-logo.raw`. The player mmaps it and resamples with
  a box/tent kernel (area averaging when shrinking, bilinear when growing) —
  point sampling a 560×531 animation down to a ~200 px logo drops most source
  rows and visibly aliases the edges.
- `boot-logo.gif` — the GTK splash page (`--splash`). gdk-pixbuf cannot read
  APNG, so this stays a GIF.

Both represent the same source animation (the APNG export, 74 frames at
25 fps, 560×531 RGBA); the raw asset is generated with:

```sh
tools/apng2raw.py splashos_boot.apng -o boot-logo.raw   # ~12 s, needs zlib only
xz -9 -k boot-logo.raw                                  # -> assets/boot-logo.raw.xz
```

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
