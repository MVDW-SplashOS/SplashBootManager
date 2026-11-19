//! splash-boot-manager — SplashOS LinuxBoot-style graphical boot manager.
//!
//! Boot flow (the dev init launches us under cage ONLY when the Super key
//! was held at power-on; without Super the dev init kexecs the default
//! entry without ever starting a UI):
//!
//!   --splash        play the animated boot logo fullscreen on a black
//!                   background (Apple-style), then transition into the
//!                   boot-options UI (see --menu)
//!   --menu          straight into the boot-options UI
//!   --dry-run       print the boot commands instead of executing them
//!   --list-devices  print the hotplug scanner's results and exit
//!
//! The boot-options UI is Apple-boot-picker style: libadwaita dark scheme
//! on a black window, one disc icon per BLS entry, front and center.
//! Selecting a disc (click, arrows + Enter) kexecs it. Bootable USB/optical
//! media plugged in while the menu is open appears next to the internal
//! entries (with a USB-stick or CD icon) and can be booted the same way.

mod bls;
mod boot;
mod devices;

use adw::prelude::*;
use gtk::gdk;
use gtk::glib;

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

const APP_ID: &str = "com.splashos.BootManager";
const LOGO_GIF: &str = "/usr/share/splashos/boot-logo.gif";
/// Seconds the splash animation plays before the menu opens. The GIF may
/// loop forever, so "finished" is a bounded play window; override it with
/// SPLASH_SECONDS for faster dev iterations.
const SPLASH_SECONDS: f64 = 3.0;
const DISC_SIZE: i32 = 112;

/// Everything the UI needs, shared across callbacks.
struct Manager {
    dry_run: bool,
    /// $BOOT tree the internal entries come from (also used by the hotplug
    /// watcher's duplicate detector).
    boot_dir: PathBuf,
    entries: Vec<bls::BootEntry>,
    /// Bootable removable media, refreshed by the hotplug watcher.
    devices: Vec<devices::BootDevice>,
}

fn main() -> glib::ExitCode {
    // Minimal arg handling: kept dependency-free on purpose (no clap) to
    // keep the vendored crate tree small for offline from-source builds.
    let mut dry_run = false;
    let mut with_splash = false;
    let mut list_devices = false;
    let mut boot_dir = PathBuf::from("/boot");
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--dry-run" => dry_run = true,
            "--splash" => with_splash = true,
            // Legacy alias for the menu-only mode; without --splash the
            // menu shows directly anyway.
            "--menu" => {}
            "--list-devices" => list_devices = true,
            "--boot-dir" => {
                if let Some(dir) = args.next() {
                    boot_dir = PathBuf::from(dir);
                }
            }
            other => {
                eprintln!("splash-boot-manager: unknown argument '{other}'");
                eprintln!("usage: splash-boot-manager [--dry-run] [--splash] [--menu] [--boot-dir <path>] [--list-devices]");
                return glib::ExitCode::FAILURE;
            }
        }
    }

    // Diagnostics helper (no GTK/display needed): show what the hotplug
    // scanner sees and how each device would boot.
    if list_devices {
        for device in devices::scan(&boot_dir) {
            println!(
                "{}  {:?}  \"{}\"  {}",
                device.devnode, device.kind, device.title, device.detail
            );
            match &device.boot {
                devices::BootKind::Kexec {
                    entries, default, ..
                } => {
                    println!(
                        "    kexec, {} entries, default {:?}",
                        entries.len(),
                        default
                    );
                    for entry in entries {
                        println!(
                            "    - {}  {}  [{}]",
                            entry.title, entry.linux, entry.options
                        );
                    }
                }
                devices::BootKind::Efi { loader, part } => {
                    println!("    uefi {loader} (partition {part})");
                }
                devices::BootKind::FirmwareEntry { number } => {
                    println!("    uefi entry Boot{number} (efibootmgr --bootnext + reboot)");
                }
                devices::BootKind::Firmware { model, .. } => {
                    println!("    firmware boot ({model})");
                }
            }
        }
        return glib::ExitCode::SUCCESS;
    }

    let entries = bls::scan_entries(&boot_dir);
    let manager = Rc::new(RefCell::new(Manager {
        dry_run,
        boot_dir: boot_dir.clone(),
        entries,
        devices: Vec::new(),
    }));

    let app = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gtk::gio::ApplicationFlags::NON_UNIQUE)
        .build();

    let manager_for_activate = manager.clone();
    app.connect_activate(move |app| build_ui(app, manager_for_activate.clone(), with_splash));

    // We parse our own args above; pass only argv[0] to GApplication so its
    // built-in option parser doesn't reject our flags.
    app.run_with_args(&["splash-boot-manager"])
}

fn build_ui(app: &adw::Application, manager: Rc<RefCell<Manager>>, with_splash: bool) {
    // Apple-picker style: force libadwaita dark, paint the window black.
    adw::StyleManager::default().set_color_scheme(adw::ColorScheme::ForceDark);
    let css = gtk::CssProvider::new();
    css.load_from_data(
        "
        window { background-color: #000000; }
        .bm-title { color: #d6d6d6; font-weight: 600; font-size: 20px; }
        .bm-caption { color: #e8e8e8; font-weight: 600; font-size: 15px; }
        .bm-detail { color: #9a9a9a; font-size: 12px; }
        .bm-status { color: #a8a8a8; }
        button.bm-disc { background: transparent; padding: 14px 10px 10px; }
        button.bm-disc:hover { background: rgba(255,255,255,0.08); border-radius: 18px; }
        button.bm-disc:focus { background: rgba(255,255,255,0.12); border-radius: 18px; }
    ",
    );
    gtk::style_context_add_provider_for_display(
        &gdk::Display::default().expect("no display"),
        &css,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Splash Boot Manager")
        // A concrete initial size: without it GTK 4.16 computes garbage
        // geometry for the first frame under cage's kiosk ("Unable to
        // create Cairo image surface: invalid value (typically too big)")
        // and the picker never renders. Cage fullscreens the toplevel.
        .default_width(640)
        .default_height(480)
        .build();
    // Do NOT call window.fullscreen(): cage's kiosk fullscreens the
    // surface itself - the client-initiated request races cage's configure
    // and GDK ends up with a garbage surface size (wl_shm pool abort).
    let stack = gtk::Stack::new();
    stack.set_transition_type(gtk::StackTransitionType::Crossfade);
    stack.set_transition_duration(500);

    // libadwaita toolbar: a slim header bar carries the app menu (firmware
    // tools and About). The splash phase hides it so the logo stands alone
    // on black, then it slides in with the picker.
    let toolbar = adw::ToolbarView::new();
    let header = adw::HeaderBar::new();
    header.set_show_start_title_buttons(false);
    header.set_show_end_title_buttons(false);
    header.set_title_widget(Some(&gtk::Label::new(Some("SplashOS"))));
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&stack));
    toolbar.set_reveal_top_bars(!with_splash);
    window.set_content(Some(&toolbar));

    build_splash_page(&stack);
    let discs_menu = build_menu_page(&window, &header, &manager, &stack);

    if with_splash {
        stack.set_visible_child_name("splash");
        let splash_secs = std::env::var("SPLASH_SECONDS")
            .ok()
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(SPLASH_SECONDS);
        let stack = stack.clone();
        let toolbar = toolbar.clone();
        let discs_menu = discs_menu.clone();
        glib::timeout_add_local_once(
            std::time::Duration::from_millis((splash_secs * 1000.0) as u64),
            move || {
                stack.set_visible_child_name("menu");
                toolbar.set_reveal_top_bars(true);
                discs_menu.first_focus();
            },
        );
    } else {
        stack.set_visible_child_name("menu");
        discs_menu.first_focus();
    }

    window.present();
}

/// Animated logo on a black background at Apple-logo proportions (~25% of
/// the screen height, centered). The GIF loops on its own; the fixed play
/// window (SPLASH_SECONDS) ends the phase.
fn build_splash_page(stack: &gtk::Stack) {
    let page = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .build();

    let picture = gtk::Picture::new();
    picture.set_halign(gtk::Align::Center);
    picture.set_valign(gtk::Align::Center);
    picture.set_hexpand(true);
    page.append(&picture);

    if let Ok(animation) = gtk::gdk_pixbuf::PixbufAnimation::from_file(PathBuf::from(LOGO_GIF)) {
        let iter = Rc::new(RefCell::new(
            animation.iter(Some(std::time::SystemTime::now())),
        ));
        // Apple-boot-logo proportions: keep the animation small and
        // centered (GtkPicture scales its paintable inside the requested
        // area while preserving the aspect ratio).
        let first = iter.borrow().pixbuf();
        let (pw, ph) = (first.width().max(1), first.height().max(1));
        let target_h = 200;
        let target_w = ((target_h as f64) * pw as f64 / ph as f64).round() as i32;
        picture.set_size_request(target_w, target_h);
        picture.set_pixbuf(Some(&first));
        let iter_frame = iter.clone();
        let picture_frame = picture.clone();
        glib::timeout_add_local(std::time::Duration::from_millis(33), move || {
            let advanced = iter_frame.borrow().advance(std::time::SystemTime::now());
            if advanced {
                picture_frame.set_pixbuf(Some(&iter_frame.borrow().pixbuf()));
            }
            glib::ControlFlow::Continue
        });
    }

    stack.add_named(&page, Some("splash"));
}

/// The boot-options UI: "Choose a boot volume" heading + centered disc
/// icons, one per BLS entry, followed by hot-plugged USB/optical media.
/// Appends the page to the stack and returns the disc row for focus
/// management.
fn build_menu_page(
    window: &adw::ApplicationWindow,
    header: &adw::HeaderBar,
    manager: &Rc<RefCell<Manager>>,
    stack: &gtk::Stack,
) -> DiscRow {
    let page = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(0)
        .build();

    let heading = gtk::Label::new(Some("Choose a boot volume"));
    heading.add_css_class("bm-title");
    heading.set_margin_top(48);
    heading.set_valign(gtk::Align::Start);
    page.append(&heading);

    let status = gtk::Label::new(None);
    status.add_css_class("bm-status");
    status.set_margin_bottom(36);
    status.set_margin_top(12);
    status.set_wrap(true);
    status.set_max_width_chars(60);

    let dry_run = manager.borrow().dry_run;
    build_app_menu(window, header, dry_run, &status);

    let row_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(28)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .vexpand(true)
        .build();

    // More discs than fit (internal entries + several plugged-in sticks)
    // scroll horizontally; the kiosk window itself never grows.
    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::External)
        .vscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        .vexpand(true)
        .build();
    scroller.set_child(Some(&row_box));
    page.append(&scroller);
    page.append(&status);

    let discs = DiscRow::new(row_box);
    rebuild_discs(manager, &discs, &status);

    // Hotplug watcher: the scan mounts devices, so it runs on a thread; this
    // timeout only drains results and rebuilds the row when something
    // changed. Focus is preserved by disc key across rebuilds.
    let (tx, rx) = std::sync::mpsc::channel();
    devices::spawn_watcher(tx, manager.borrow().boot_dir.clone());
    {
        let manager = manager.clone();
        let discs = discs.clone();
        let status = status.clone();
        glib::timeout_add_local(Duration::from_millis(300), move || {
            let mut latest = None;
            while let Ok(list) = rx.try_recv() {
                latest = Some(list);
            }
            if let Some(list) = latest {
                let changed = {
                    let mut m = manager.borrow_mut();
                    if m.devices != list {
                        m.devices = list;
                        true
                    } else {
                        false
                    }
                };
                if changed {
                    rebuild_discs(&manager, &discs, &status);
                }
            }
            glib::ControlFlow::Continue
        });
    }

    // Keyboard selection: arrows move between discs, Enter boots.
    let controller = gtk::EventControllerKey::new();
    {
        let discs = discs.clone();
        controller.connect_key_pressed(move |_ctrl, keyval, _keycode, _state| {
            match keyval {
                gtk::gdk::Key::Left => discs.move_focus(-1),
                gtk::gdk::Key::Right => discs.move_focus(1),
                gtk::gdk::Key::Return | gtk::gdk::Key::KP_Enter => discs.activate_focused(),
                _ => return glib::signal::Propagation::Proceed,
            }
            glib::signal::Propagation::Stop
        });
    }
    window.add_controller(controller);

    stack.add_named(&page, Some("menu"));

    discs
}

/// The app menu behind the header-bar hamburger: firmware-provided tools
/// and About.
///
/// Firmware setup, the firmware boot-menu UI and the UEFI shell point into
/// firmware volumes — they act on the machine, they are not bootable
/// volumes — so the picker hides them and they live here instead. Each one
/// still runs through the firmware route (`efibootmgr --bootnext` + reboot).
fn build_app_menu(
    window: &adw::ApplicationWindow,
    header: &adw::HeaderBar,
    dry_run: bool,
    status: &gtk::Label,
) {
    let menu = gtk::gio::Menu::new();
    let actions = gtk::gio::SimpleActionGroup::new();

    let utilities: Vec<devices::FirmwareEntry> = devices::firmware_entries()
        .unwrap_or_default()
        .into_iter()
        .filter(|entry| entry.is_firmware_utility())
        .collect();

    if !utilities.is_empty() {
        let section = gtk::gio::Menu::new();
        for (index, entry) in utilities.iter().enumerate() {
            let action_name = format!("firmware-{index}");
            section.append(Some(&entry.label), Some(&format!("app-menu.{action_name}")));
            let action = gtk::gio::SimpleAction::new(&action_name, None);
            let number = entry.number.clone();
            let label = entry.label.clone();
            let status = status.clone();
            action.connect_activate(move |_, _| {
                status.set_text(&format!("Restarting into \u{201c}{label}\u{201d}\u{2026}"));
                if let Err(err) = boot::firmware_entry_boot(&number, dry_run) {
                    status.set_text(&format!("Boot failed: {err}"));
                }
            });
            actions.add_action(&action);
        }
        menu.append_section(None, &section);
    }

    let about_section = gtk::gio::Menu::new();
    about_section.append(Some("About SplashOS Boot Manager"), Some("app-menu.about"));
    menu.append_section(None, &about_section);

    let about_action = gtk::gio::SimpleAction::new("about", None);
    let window_weak = window.downgrade();
    about_action.connect_activate(move |_, _| {
        let Some(window) = window_weak.upgrade() else {
            return;
        };
        let about = adw::AboutDialog::builder()
            .application_name("SplashOS Boot Manager")
            .developer_name("SplashOS")
            .version(env!("CARGO_PKG_VERSION"))
            .comments(
                "A graphical boot manager: picks a kernel to kexec, \
                 or hands foreign media to the firmware.",
            )
            .website("https://github.com/MVDW-SplashOS/SplashBootManager")
            .issue_url("https://github.com/MVDW-SplashOS/SplashBootManager/issues")
            .license_type(gtk::License::Gpl20)
            .developers(["SplashOS contributors"])
            .build();
        about.present(Some(&window));
    });
    actions.add_action(&about_action);

    window.insert_action_group("app-menu", Some(&actions));

    // No icon theme is installed in the boot environment, so the button
    // carries a glyph from the bundled font instead of an icon name.
    let button = gtk::MenuButton::builder()
        .label("\u{2261}")
        .tooltip_text("Menu")
        .css_classes(vec!["flat"])
        .build();
    button.set_menu_model(Some(&menu));
    header.pack_end(&button);
}

/// Build the full disc row (internal BLS entries first, then detected
/// devices). Called at startup and whenever the hotplug watcher reports a
/// change.
fn rebuild_discs(manager: &Rc<RefCell<Manager>>, discs: &DiscRow, status: &gtk::Label) {
    let m = manager.borrow();
    let mut items: Vec<(String, gtk::Button)> = Vec::new();

    for entry in &m.entries {
        items.push((
            format!("entry:{}", entry.id),
            entry_button(entry, m.dry_run, status.clone()),
        ));
    }
    for device in &m.devices {
        items.push((
            format!("device:{}", device.key),
            device_button(device, m.dry_run, status.clone()),
        ));
    }

    discs.rebuild(items);
}

fn entry_button(entry: &bls::BootEntry, dry_run: bool, status: gtk::Label) -> gtk::Button {
    // Internal entries are installed operating systems: hard-drive look,
    // distinct from the CD-ROM icon of optical media.
    let button = disc_button(&entry.title, None, DiscIcon::Hdd);
    let entry = entry.clone();
    button.connect_clicked(move |_| {
        status.set_text(&format!("Booting \u{201c}{}\u{201d}\u{2026}", entry.title));
        if let Err(err) = boot::boot(&entry, dry_run) {
            status.set_text(&format!("Boot failed: {err}"));
        }
    });
    button
}

fn device_button(device: &devices::BootDevice, dry_run: bool, status: gtk::Label) -> gtk::Button {
    let icon = match device.kind {
        devices::DeviceKind::Usb => DiscIcon::Usb,
        devices::DeviceKind::Optical => DiscIcon::Optical,
        devices::DeviceKind::Internal => internal_icon(&device.title),
    };
    let button = disc_button(&device.title, Some(&device.detail), icon);
    let device = device.clone();
    button.connect_clicked(move |_| {
        status.set_text(&format!("Booting \u{201c}{}\u{201d}\u{2026}", device.title));
        if let Err(err) = boot::boot_device(&device, dry_run) {
            status.set_text(&format!("Boot failed: {err}"));
        }
    });
    button
}

/// Pick the icon for an installed-OS disc from its title (vendor glyphs
/// where we know them, a hard drive otherwise).
fn internal_icon(title: &str) -> DiscIcon {
    let title = title.to_lowercase();
    if title.contains("windows") || title.contains("bootmgfw") {
        DiscIcon::Windows
    } else {
        DiscIcon::Hdd
    }
}

#[derive(Clone, Copy)]
enum DiscIcon {
    Optical,
    Usb,
    Windows,
    Hdd,
}

/// A single Apple-style disc icon button.
fn disc_button(title: &str, detail: Option<&str>, icon: DiscIcon) -> gtk::Button {
    let button = gtk::Button::builder().css_classes(vec!["bm-disc"]).build();

    let inner = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .build();

    let disc = gtk::DrawingArea::builder()
        .width_request(DISC_SIZE)
        .height_request(DISC_SIZE)
        .build();
    disc.set_draw_func(move |_area, cr, width, height| match icon {
        DiscIcon::Optical => draw_disc(cr, width as f64, height as f64),
        DiscIcon::Usb => draw_usb(cr, width as f64, height as f64),
        DiscIcon::Windows => draw_windows(cr, width as f64, height as f64),
        DiscIcon::Hdd => draw_hdd(cr, width as f64, height as f64),
    });

    let caption = gtk::Label::new(Some(title));
    caption.add_css_class("bm-caption");
    caption.set_max_width_chars(16);
    caption.set_ellipsize(gtk::pango::EllipsizeMode::End);

    inner.append(&disc);
    inner.append(&caption);
    if let Some(detail) = detail {
        let detail = gtk::Label::new(Some(detail));
        detail.add_css_class("bm-detail");
        detail.set_max_width_chars(18);
        detail.set_ellipsize(gtk::pango::EllipsizeMode::End);
        inner.append(&detail);
    }

    button.set_child(Some(&inner));
    button
}

/// Draw a CD/disc icon with Cairo: dark body, data rings, hub and a
/// specular highlight — readable on a black background.
#[allow(unused_must_use)]
fn draw_disc(cr: &gtk::cairo::Context, width: f64, height: f64) {
    let size = width.min(height) * 0.86;
    let r = size / 2.0;
    let cx = width / 2.0;
    let cy = height / 2.0;

    // Body with a soft radial gradient (lighter rim than core).
    let body = gtk::cairo::RadialGradient::new(cx, cy, r * 0.30, cx, cy, r);
    body.add_color_stop_rgba(0.0, 0.16, 0.16, 0.17, 1.0);
    body.add_color_stop_rgba(0.75, 0.22, 0.22, 0.24, 1.0);
    body.add_color_stop_rgba(1.0, 0.34, 0.34, 0.36, 1.0);
    cr.set_source(&body);
    cr.arc(cx, cy, r, 0.0, std::f64::consts::TAU);
    cr.fill();

    // Faint data rings.
    for (ring, alpha) in [(0.52, 0.10), (0.64, 0.08), (0.76, 0.07)] {
        cr.set_source_rgba(1.0, 1.0, 1.0, alpha);
        cr.set_line_width(1.4);
        cr.arc(cx, cy, r * ring, 0.0, std::f64::consts::TAU);
        cr.stroke();
    }

    // Specular highlight arc (top-left).
    cr.set_source_rgba(1.0, 1.0, 1.0, 0.30);
    cr.set_line_width(r * 0.055);
    cr.arc(cx, cy, r * 0.88, -2.4, -1.2);
    cr.stroke();

    // Hub: light ring around the dark center hole.
    cr.set_source_rgba(0.72, 0.72, 0.74, 1.0);
    cr.set_line_width(r * 0.045);
    cr.arc(cx, cy, r * 0.30, 0.0, std::f64::consts::TAU);
    cr.stroke();
    cr.set_source_rgb(0.0, 0.0, 0.0);
    cr.arc(cx, cy, r * 0.24, 0.0, std::f64::consts::TAU);
    cr.fill();
}

/// Draw a USB flash drive: metal connector with two contacts, dark body
/// with a gradient, an activity LED and a lanyard hole.
#[allow(unused_must_use)]
fn draw_usb(cr: &gtk::cairo::Context, width: f64, height: f64) {
    let s = width.min(height) * 0.80;
    let cx = width / 2.0;
    let cy = height / 2.0;

    let body_w = s * 0.46;
    let body_h = s * 0.62;
    let body_x = cx - body_w / 2.0;
    let body_y = cy - body_h / 2.0 + s * 0.09;
    let conn_w = body_w * 0.58;
    let conn_h = s * 0.16;
    let conn_x = cx - conn_w / 2.0;
    let conn_y = body_y - conn_h + 2.0;

    // Metal connector.
    let metal = gtk::cairo::LinearGradient::new(conn_x, conn_y, conn_x, conn_y + conn_h);
    metal.add_color_stop_rgb(0.0, 0.85, 0.86, 0.88);
    metal.add_color_stop_rgb(0.5, 0.62, 0.63, 0.66);
    metal.add_color_stop_rgb(1.0, 0.78, 0.79, 0.81);
    cr.set_source(&metal);
    rounded_rect(cr, conn_x, conn_y, conn_w, conn_h, s * 0.03);
    cr.fill_preserve();
    cr.set_source_rgba(0.0, 0.0, 0.0, 0.35);
    cr.set_line_width(1.0);
    cr.stroke();

    // Two connector contacts.
    cr.set_source_rgba(0.28, 0.28, 0.30, 1.0);
    let contact_w = conn_w * 0.18;
    let contact_h = conn_h * 0.42;
    for i in 0..2 {
        let x = conn_x + conn_w * (0.18 + i as f64 * 0.36) - contact_w / 2.0;
        rounded_rect(
            cr,
            x,
            conn_y + conn_h * 0.30,
            contact_w,
            contact_h,
            contact_w * 0.2,
        );
        cr.fill();
    }

    // Body.
    let body = gtk::cairo::LinearGradient::new(body_x, body_y, body_x, body_y + body_h);
    body.add_color_stop_rgb(0.0, 0.30, 0.30, 0.33);
    body.add_color_stop_rgb(0.45, 0.20, 0.20, 0.22);
    body.add_color_stop_rgb(1.0, 0.14, 0.14, 0.16);
    cr.set_source(&body);
    rounded_rect(cr, body_x, body_y, body_w, body_h, s * 0.08);
    cr.fill_preserve();
    cr.set_source_rgba(1.0, 1.0, 1.0, 0.18);
    cr.set_line_width(1.2);
    cr.stroke();

    // Highlight stripe.
    cr.set_source_rgba(1.0, 1.0, 1.0, 0.10);
    rounded_rect(
        cr,
        body_x + body_w * 0.12,
        body_y + body_h * 0.10,
        body_w * 0.10,
        body_h * 0.72,
        body_w * 0.05,
    );
    cr.fill();

    // Activity LED and lanyard hole.
    cr.set_source_rgba(0.55, 0.75, 0.95, 0.85);
    cr.arc(
        cx + body_w * 0.22,
        body_y + body_h * 0.14,
        s * 0.018,
        0.0,
        std::f64::consts::TAU,
    );
    cr.fill();

    cr.set_source_rgba(0.0, 0.0, 0.0, 0.55);
    cr.arc(
        cx,
        body_y + body_h * 0.82,
        s * 0.030,
        0.0,
        std::f64::consts::TAU,
    );
    cr.fill();
    cr.set_source_rgba(0.45, 0.45, 0.48, 0.9);
    cr.set_line_width(1.2);
    cr.arc(
        cx,
        body_y + body_h * 0.82,
        s * 0.030,
        0.0,
        std::f64::consts::TAU,
    );
    cr.stroke();
}

#[allow(unused_must_use)]
/// Draw the Windows logo: four tilted panes in the brand blue, sized and
/// centered like the other disc icons.
#[allow(unused_must_use)]
fn draw_windows(cr: &gtk::cairo::Context, width: f64, height: f64) {
    let s = width.min(height) * 0.62;
    let cx = width / 2.0;
    let cy = height / 2.0;

    let pane = s * 0.46;
    let gap = s * 0.07;
    let tilt = -0.04; // subtle perspective like the official flag

    // One gradient for all four panes, slightly brighter at the top.
    let paint = gtk::cairo::LinearGradient::new(cx - s, cy - s, cx + s, cy + s);
    paint.add_color_stop_rgb(0.0, 0.22, 0.55, 0.92);
    paint.add_color_stop_rgb(1.0, 0.12, 0.38, 0.78);

    // Four panes; the left column is taller for the perspective tilt.
    for (x, y, w, h) in [
        (
            cx - pane - gap / 2.0,
            cy - pane * (1.0 + tilt) - gap / 2.0,
            pane,
            pane * (1.0 + tilt) * 2.0,
        ),
        (
            cx - pane - gap / 2.0,
            cy + gap / 2.0,
            pane,
            pane * (1.0 - tilt) * 2.0,
        ),
        (
            cx + gap / 2.0,
            cy - pane * (1.0 + tilt) - gap / 2.0,
            pane,
            pane * (1.0 + tilt) * 2.0,
        ),
        (
            cx + gap / 2.0,
            cy + gap / 2.0,
            pane,
            pane * (1.0 - tilt) * 2.0,
        ),
    ] {
        cr.set_source(&paint);
        rounded_rect(cr, x, y, w, h, pane * 0.10);
        cr.fill();
    }
}

/// Draw an internal hard drive: a rounded enclosure with a visible platter
/// and actuator arm, plus a small activity LED.
#[allow(unused_must_use)]
fn draw_hdd(cr: &gtk::cairo::Context, width: f64, height: f64) {
    let s = width.min(height) * 0.82;
    let cx = width / 2.0;
    let cy = height / 2.0;

    let body_w = s * 0.78;
    let body_h = s * 0.52;
    let body_x = cx - body_w / 2.0;
    let body_y = cy - body_h / 2.0;

    // Enclosure.
    let body = gtk::cairo::LinearGradient::new(body_x, body_y, body_x, body_y + body_h);
    body.add_color_stop_rgb(0.0, 0.32, 0.32, 0.35);
    body.add_color_stop_rgb(0.5, 0.20, 0.20, 0.23);
    body.add_color_stop_rgb(1.0, 0.13, 0.13, 0.15);
    cr.set_source(&body);
    rounded_rect(cr, body_x, body_y, body_w, body_h, s * 0.06);
    cr.fill_preserve();
    cr.set_source_rgba(1.0, 1.0, 1.0, 0.16);
    cr.set_line_width(1.2);
    cr.stroke();

    // Platter recess and platter.
    let platter_r = body_h * 0.30;
    let platter_cx = body_x + body_w * 0.34;
    let platter_cy = body_y + body_h * 0.52;
    cr.set_source_rgba(0.0, 0.0, 0.0, 0.45);
    cr.arc(
        platter_cx,
        platter_cy,
        platter_r * 1.14,
        0.0,
        std::f64::consts::TAU,
    );
    cr.fill();

    let platter = gtk::cairo::RadialGradient::new(
        platter_cx,
        platter_cy,
        platter_r * 0.2,
        platter_cx,
        platter_cy,
        platter_r,
    );
    platter.add_color_stop_rgb(0.0, 0.62, 0.64, 0.68);
    platter.add_color_stop_rgb(0.85, 0.45, 0.47, 0.52);
    platter.add_color_stop_rgb(1.0, 0.58, 0.60, 0.66);
    cr.set_source(&platter);
    cr.arc(
        platter_cx,
        platter_cy,
        platter_r,
        0.0,
        std::f64::consts::TAU,
    );
    cr.fill();

    // Data rings.
    cr.set_source_rgba(1.0, 1.0, 1.0, 0.12);
    cr.set_line_width(0.8);
    for ring in [0.45, 0.65, 0.82] {
        cr.arc(
            platter_cx,
            platter_cy,
            platter_r * ring,
            0.0,
            std::f64::consts::TAU,
        );
        cr.stroke();
    }

    // Actuator arm reaching from the corner onto the platter.
    cr.set_source_rgba(0.75, 0.76, 0.80, 0.9);
    cr.set_line_width(s * 0.022);
    cr.move_to(body_x + body_w * 0.88, body_y + body_h * 0.14);
    cr.line_to(platter_cx + platter_r * 0.55, platter_cy - platter_r * 0.55);
    cr.stroke();
    cr.set_source_rgba(0.85, 0.86, 0.90, 1.0);
    cr.arc(
        platter_cx + platter_r * 0.55,
        platter_cy - platter_r * 0.55,
        s * 0.020,
        0.0,
        std::f64::consts::TAU,
    );
    cr.fill();

    // Spindle hub.
    cr.set_source_rgba(0.30, 0.31, 0.34, 1.0);
    cr.arc(
        platter_cx,
        platter_cy,
        platter_r * 0.22,
        0.0,
        std::f64::consts::TAU,
    );
    cr.fill();

    // Activity LED.
    cr.set_source_rgba(0.55, 0.75, 0.95, 0.9);
    cr.arc(
        body_x + body_w * 0.88,
        body_y + body_h * 0.82,
        s * 0.016,
        0.0,
        std::f64::consts::TAU,
    );
    cr.fill();
}

fn rounded_rect(cr: &gtk::cairo::Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    let pi = std::f64::consts::PI;
    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -pi / 2.0, 0.0);
    cr.arc(x + w - r, y + h - r, r, 0.0, pi / 2.0);
    cr.arc(x + r, y + h - r, r, pi / 2.0, pi);
    cr.arc(x + r, y + r, r, pi, 3.0 * pi / 2.0);
    cr.close_path();
}

/// Focus/selection bookkeeping for the disc row. The row is rebuilt when a
/// device is plugged in/unplugged, so the focused disc is tracked by key,
/// not by index.
#[derive(Clone)]
struct DiscRow {
    inner: Rc<RefCell<DiscRowInner>>,
}

struct DiscRowInner {
    row: gtk::Box,
    buttons: Vec<gtk::Button>,
    keys: Vec<String>,
    focused: usize,
}

impl DiscRow {
    fn new(row: gtk::Box) -> Self {
        Self {
            inner: Rc::new(RefCell::new(DiscRowInner {
                row,
                buttons: Vec::new(),
                keys: Vec::new(),
                focused: 0,
            })),
        }
    }

    fn rebuild(&self, items: Vec<(String, gtk::Button)>) {
        let mut inner = self.inner.borrow_mut();
        let focused_key = inner.keys.get(inner.focused).cloned();
        let had_focus = inner.buttons.iter().any(|b| b.has_focus());

        while let Some(child) = inner.row.first_child() {
            inner.row.remove(&child);
        }
        inner.buttons.clear();
        inner.keys.clear();

        for (key, button) in items {
            let shared = self.inner.clone();
            let focus_key = key.clone();
            let focus = gtk::EventControllerFocus::new();
            focus.connect_enter(move |_| {
                if let Ok(mut inner) = shared.try_borrow_mut() {
                    if let Some(pos) = inner.keys.iter().position(|k| *k == focus_key) {
                        inner.focused = pos;
                    }
                }
            });
            button.add_controller(focus);
            inner.row.append(&button);
            inner.keys.push(key);
            inner.buttons.push(button);
        }

        if inner.buttons.is_empty() {
            inner.focused = 0;
            return;
        }
        let next = focused_key
            .and_then(|key| inner.keys.iter().position(|k| *k == key))
            .unwrap_or_else(|| inner.focused.min(inner.buttons.len() - 1));
        inner.focused = next;
        if had_focus {
            inner.buttons[next].grab_focus();
        }
    }

    fn first_focus(&self) {
        let inner = self.inner.borrow();
        if let Some(first) = inner.buttons.first() {
            first.grab_focus();
        }
    }

    fn move_focus(&self, delta: i32) {
        let mut inner = self.inner.borrow_mut();
        if inner.buttons.is_empty() {
            return;
        }
        let current = inner
            .buttons
            .iter()
            .position(|b| b.has_focus())
            .unwrap_or(inner.focused);
        let n = inner.buttons.len() as i32;
        let next = (current as i32 + delta).rem_euclid(n) as usize;
        inner.focused = next;
        inner.buttons[next].grab_focus();
    }

    fn activate_focused(&self) {
        let inner = self.inner.borrow();
        let current = inner
            .buttons
            .iter()
            .position(|b| b.has_focus())
            .unwrap_or(inner.focused);
        if let Some(button) = inner.buttons.get(current) {
            button.emit_clicked();
        }
    }
}
