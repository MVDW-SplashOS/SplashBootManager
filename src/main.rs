//! splash-boot-manager — SplashOS LinuxBoot-style graphical boot manager.
//!
//! Behaviour (phase 1):
//!   - reads Boot Loader Specification entries from $BOOT (default /boot)
//!   - boots the default entry automatically after `timeout` seconds
//!   - holding the Super key during the countdown opens the boot menu
//!   - picking an entry kexecs it (`--dry-run` only prints the commands)
//!
//! The diagnostics/rescue UI is a later phase and intentionally absent.

mod bls;
mod boot;

use gtk::glib;
use gtk::prelude::*;
use adw::prelude::*;

use std::cell::Cell;
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

const APP_ID: &str = "com.splashos.BootManager";

/// Everything the UI needs, shared across callbacks.
struct Manager {
    dry_run: bool,
    menu_mode: bool,
    entries: Vec<bls::BootEntry>,
    default: Option<bls::BootEntry>,
    timeout_secs: u32,
}

fn main() -> glib::ExitCode {
    // Minimal arg handling: --dry-run, --menu and --boot-dir <path>. Kept
    // dependency-free on purpose (no clap) to keep the vendored crate tree
    // small for offline from-source builds.
    let mut dry_run = false;
    let mut menu_mode = false;
    let mut boot_dir = PathBuf::from("/boot");
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--dry-run" => dry_run = true,
            // Menu mode: skip the countdown entirely and present the boot
            // options list right away. Used when the boot manager is
            // launched because the Super key was held at power-on (see the
            // /sbin/init boot flow) - in that case the default entry is
            // NOT booted automatically, the user picks from the UI.
            "--menu" => menu_mode = true,
            "--boot-dir" => {
                if let Some(dir) = args.next() {
                    boot_dir = PathBuf::from(dir);
                }
            }
            other => {
                eprintln!("splash-boot-manager: unknown argument '{other}'");
                eprintln!("usage: splash-boot-manager [--dry-run] [--menu] [--boot-dir <path>]");
                return glib::ExitCode::FAILURE;
            }
        }
    }

    let entries = bls::scan_entries(&boot_dir);
    let (default_id, timeout_secs) = bls::parse_loader_conf(&boot_dir);
    let default = bls::default_entry(&entries, &default_id).cloned();

    let manager = Rc::new(RefCell::new(Manager {
        dry_run,
        menu_mode,
        entries,
        default,
        timeout_secs,
    }));

    let app = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gtk::gio::ApplicationFlags::NON_UNIQUE)
        .build();

    let manager_for_activate = manager.clone();
    app.connect_activate(move |app| build_ui(app, manager_for_activate.clone()));

    // We parse our own args above; pass only argv[0] to GApplication so its
    // built-in option parser doesn't reject --dry-run / --boot-dir.
    app.run_with_args(&["splash-boot-manager"])
}

fn build_ui(app: &adw::Application, manager: Rc<RefCell<Manager>>) {
    let (timeout_secs, has_default) = {
        let m = manager.borrow();
        (m.timeout_secs, m.default.is_some())
    };

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Splash Boot Manager")
        .default_width(640)
        .default_height(480)
        .build();

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(24)
        .margin_bottom(24)
        .margin_start(24)
        .margin_end(24)
        .build();

    let status = gtk::Label::new(Some("Splash Boot Manager"));
    status.add_css_class("title-2");
    content.append(&status);

    // Countdown text (normal state) and the boot-options list (menu state).
    let countdown = gtk::Label::new(None);
    content.append(&countdown);

    let list = gtk::ListBox::new();
    list.add_css_class("boxed-list");
    list.set_visible(false);
    {
        let m = manager.borrow();
        for entry in &m.entries {
            let row = adw::ActionRow::builder()
                .title(&entry.title)
                .subtitle(&entry.id)
                .activatable(true)
                .build();
            list.append(&row);
        }
    }
    content.append(&list);

    window.set_content(Some(&content));

    // ---- menu mode: opened by holding Super during the countdown -----------
    let menu_opened = Rc::new(Cell::new(false));
    // The manager is launched with --menu when the Super key was already
    // held at power-on: present the boot options list immediately.
    if manager.borrow().menu_mode {
        menu_opened.set(true);
        list.set_visible(true);
        countdown.set_visible(false);
        status.set_text("Boot options");
    }
    let controller = gtk::EventControllerKey::new();
    controller.set_propagation_phase(gtk::PropagationPhase::Capture);
    {
        let menu_opened = menu_opened.clone();
        let countdown = countdown.clone();
        let list = list.clone();
        let status = status.clone();
        controller.connect_key_pressed(move |_ctrl, keyval, _keycode, state| {
            let is_super = keyval == gtk::gdk::Key::Super_L
                || keyval == gtk::gdk::Key::Super_R
                || state.contains(gtk::gdk::ModifierType::SUPER_MASK);
            if !is_super {
                return glib::signal::Propagation::Proceed;
            }
            if !menu_opened.get() {
                menu_opened.set(true);
                countdown.set_visible(false);
                list.set_visible(true);
                status.set_text("Boot options — hold Super");
            }
            glib::signal::Propagation::Stop
        });
    }
    window.add_controller(controller);

    // ---- countdown to the default boot -------------------------------------
    let remaining = Rc::new(Cell::new(timeout_secs));
    {
        let menu_opened = menu_opened.clone();
        let manager = manager.clone();
        let countdown = countdown.clone();
        let status = status.clone();
        glib::timeout_add_seconds_local(1, move || {
            if menu_opened.get() {
                return glib::ControlFlow::Break;
            }
            let left = remaining.get();
            if left == 0 {
                return glib::ControlFlow::Break;
            }
            remaining.set(left - 1);

            let (entry, dry_run) = {
                let m = manager.borrow();
                (m.default.clone(), m.dry_run)
            };
            let Some(entry) = entry else {
                countdown.set_text("No boot entries found");
                return glib::ControlFlow::Break;
            };

            countdown.set_text(&format!(
                "Booting \u{201c}{}\u{201d} in {}s — hold Super for options",
                entry.title,
                remaining.get()
            ));

            if remaining.get() == 0 {
                countdown.set_text(&format!("Booting \u{201c}{}\u{201d}\u{2026}", entry.title));
                match boot::boot(&entry, dry_run) {
                    Ok(()) if !dry_run => {
                        // A successful `kexec -e` never returns.
                        return glib::ControlFlow::Break;
                    }
                    Err(err) => {
                        status.set_text(&format!("Boot failed: {err}"));
                    }
                    _ => {}
                }
                return glib::ControlFlow::Break;
            }

            glib::ControlFlow::Continue
        });
    }

    // ---- picking an entry from the menu -------------------------------------
    {
        let manager = manager.clone();
        let status = status.clone();
        list.connect_row_activated(move |_list, row| {
            let index = row.index();
            if index < 0 {
                return;
            }
            let (entry, dry_run) = {
                let m = manager.borrow();
                (m.entries.get(index as usize).cloned(), m.dry_run)
            };
            if let Some(entry) = entry {
                status.set_text(&format!("Booting \u{201c}{}\u{201d}\u{2026}", entry.title));
                if let Err(err) = boot::boot(&entry, dry_run) {
                    status.set_text(&format!("Boot failed: {err}"));
                }
            }
        });
    }

    if !has_default {
        countdown.set_text("No default entry configured");
    }

    window.present();
}
