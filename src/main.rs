//! splash-boot-manager — SplashOS LinuxBoot-style graphical boot manager.
//!
//! Boot flow (the dev init launches us under cage ONLY when the Super key
//! was held at power-on; without Super the dev init kexecs the default
//! entry without ever starting a UI):
//!
//!   --splash   play the animated boot logo fullscreen on a black
//!              background (Apple-style), then transition into the
//!              boot-options UI (see --menu)
//!   --menu     straight into the boot-options UI
//!   --dry-run  print the kexec commands instead of executing them
//!
//! The boot-options UI is Apple-boot-picker style: libadwaita dark scheme
//! on a black window, one disc icon per BLS entry, front and center.
//! Selecting a disc (click, arrows + Enter) kexecs it.

mod bls;
mod boot;

use gtk::gdk;
use gtk::glib;
use gtk::prelude::*;
use adw::prelude::*;

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

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
    entries: Vec<bls::BootEntry>,
}

fn main() -> glib::ExitCode {
    // Minimal arg handling: kept dependency-free on purpose (no clap) to
    // keep the vendored crate tree small for offline from-source builds.
    let mut dry_run = false;
    let mut with_splash = false;
    let mut boot_dir = PathBuf::from("/boot");
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--dry-run" => dry_run = true,
            "--splash" => with_splash = true,
            // Legacy alias for the menu-only mode; without --splash the
            // menu shows directly anyway.
            "--menu" => {}
            "--boot-dir" => {
                if let Some(dir) = args.next() {
                    boot_dir = PathBuf::from(dir);
                }
            }
            other => {
                eprintln!("splash-boot-manager: unknown argument '{other}'");
                eprintln!("usage: splash-boot-manager [--dry-run] [--splash] [--menu] [--boot-dir <path>]");
                return glib::ExitCode::FAILURE;
            }
        }
    }

    let entries = bls::scan_entries(&boot_dir);
    let manager = Rc::new(RefCell::new(Manager {
        dry_run,
        entries,
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
    css.load_from_data("
        window { background-color: #000000; }
        .bm-title { color: #d6d6d6; font-weight: 600; font-size: 20px; }
        .bm-caption { color: #e8e8e8; font-weight: 600; font-size: 15px; }
        .bm-status { color: #a8a8a8; }
        button.bm-disc { background: transparent; padding: 14px 10px 10px; }
        button.bm-disc:hover { background: rgba(255,255,255,0.08); border-radius: 18px; }
        button.bm-disc:focus { background: rgba(255,255,255,0.12); border-radius: 18px; }
    ");
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
    window.set_content(Some(&stack));

    build_splash_page(&stack);
    let discs_menu = build_menu_page(&window, &manager, &stack);

    if with_splash {
        stack.set_visible_child_name("splash");
        let splash_secs = std::env::var("SPLASH_SECONDS")
            .ok()
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(SPLASH_SECONDS);
        let stack = stack.clone();
        let discs_menu = discs_menu.clone();
        glib::timeout_add_local_once(
            std::time::Duration::from_millis((splash_secs * 1000.0) as u64),
            move || {
                stack.set_visible_child_name("menu");
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
        glib::timeout_add_local(
            std::time::Duration::from_millis(33),
            move || {
                let advanced =
                    iter_frame.borrow().advance(std::time::SystemTime::now());
                if advanced {
                    picture_frame.set_pixbuf(Some(&iter_frame.borrow().pixbuf()));
                }
                glib::ControlFlow::Continue
            },
        );
    }

    stack.add_named(&page, Some("splash"));
}

/// The boot-options UI: "Choose a boot volume" heading + centered disc
/// icons, one per BLS entry. Appends the page to the stack and returns the
/// disc row for focus management.
fn build_menu_page(
    window: &adw::ApplicationWindow,
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

    let row_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(28)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .vexpand(true)
        .build();

    let mut buttons = Vec::new();
    {
        let m = manager.borrow();
        for entry in &m.entries {
            let entry = entry.clone();
            let dry_run = m.dry_run;
            let status = status.clone();
            let button = disc_button(&entry);
            buttons.push(button.clone());
            row_box.append(&button);
            button.connect_clicked(move |_| {
                status.set_text(&format!("Booting \u{201c}{}\u{201d}\u{2026}", entry.title));
                if let Err(err) = boot::boot(&entry, dry_run) {
                    status.set_text(&format!("Boot failed: {err}"));
                }
            });
        }
    }

    page.append(&row_box);
    page.append(&status);

    // Keyboard selection: arrows move between discs, Enter boots.
    let discs = DiscRow::new(buttons);
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

/// A single Apple-style disc icon button.
fn disc_button(entry: &bls::BootEntry) -> gtk::Button {
    let button = gtk::Button::builder()
        .css_classes(vec!["bm-disc"])
        .build();

    let inner = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .build();

    let disc = gtk::DrawingArea::builder()
        .width_request(DISC_SIZE)
        .height_request(DISC_SIZE)
        .build();
    disc.set_draw_func(move |_area, cr, width, height| {
        draw_disc(cr, width as f64, height as f64);
    });

    let caption = gtk::Label::new(Some(&entry.title));
    caption.add_css_class("bm-caption");
    caption.set_max_width_chars(16);
    caption.set_ellipsize(gtk::pango::EllipsizeMode::End);

    inner.append(&disc);
    inner.append(&caption);
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

/// Focus/selection bookkeeping for the disc row.
#[derive(Clone)]
struct DiscRow {
    inner: RefCell<DiscRowInner>,
}

#[derive(Clone)]
struct DiscRowInner {
    buttons: Vec<gtk::Button>,
    focused: usize,
}

impl DiscRow {
    fn new(buttons: Vec<gtk::Button>) -> Self {
        Self {
            inner: RefCell::new(DiscRowInner {
                buttons,
                focused: 0,
            }),
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
        let n = inner.buttons.len() as i32;
        let next = (inner.focused as i32 + delta).rem_euclid(n);
        inner.focused = next as usize;
        inner.buttons[next as usize].grab_focus();
    }

    fn activate_focused(&self) {
        let inner = self.inner.borrow();
        if let Some(b) = inner.buttons.get(inner.focused) {
            b.emit_clicked();
        }
    }
}
