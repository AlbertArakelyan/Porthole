mod sockets;

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::{gdk, gio, glib};

use sockets::{KillError, Listener, Proto, Signal};

const APP_ID: &str = "io.github.albertarakelyan.Porthole";
const AUTO_REFRESH: Duration = Duration::from_secs(2);

const CSS: &str = "
.port {
    font-family: monospace;
    font-size: 1.3em;
    font-weight: bold;
}
.proto {
    font-size: 0.75em;
    font-weight: bold;
    padding: 2px 8px;
    border-radius: 999px;
}
.proto.tcp { color: var(--blue-4); background: color-mix(in srgb, var(--blue-3) 15%, transparent); }
.proto.udp { color: var(--orange-5); background: color-mix(in srgb, var(--orange-3) 18%, transparent); }
.cmdline { font-family: monospace; font-size: 0.85em; }
row.foreign { opacity: 0.55; }
";

struct Ui {
    window: adw::ApplicationWindow,
    title: adw::WindowTitle,
    toasts: adw::ToastOverlay,
    search: gtk::SearchEntry,
    filter: adw::ToggleGroup,
    stack: gtk::Stack,
    empty: adw::StatusPage,
    list: gtk::ListBox,
    auto_refresh: gtk::ToggleButton,
    listeners: RefCell<Vec<Listener>>,
}

fn main() -> glib::ExitCode {
    gio::resources_register_include!("porthole.gresource").expect("failed to register resources");

    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_startup(|_| {
        load_css();
        gtk::Window::set_default_icon_name(APP_ID);
    });
    app.connect_activate(build_ui);
    app.run()
}

fn load_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(CSS);
    gtk::style_context_add_provider_for_display(
        &gdk::Display::default().expect("no display"),
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}

fn build_ui(app: &adw::Application) {
    // Header bar
    let title = adw::WindowTitle::new("Porthole", "");
    let refresh = gtk::Button::builder()
        .icon_name("view-refresh-symbolic")
        .tooltip_text("Refresh (F5)")
        .build();
    let auto_refresh = gtk::ToggleButton::builder()
        .icon_name("media-playlist-repeat-symbolic")
        .tooltip_text("Auto-refresh every 2 seconds")
        .active(true)
        .build();
    let header = adw::HeaderBar::builder().title_widget(&title).build();
    header.pack_start(&refresh);
    header.pack_end(&auto_refresh);

    // Search + protocol filter
    let search = gtk::SearchEntry::builder()
        .placeholder_text("Filter by port, process, PID or user…")
        .hexpand(true)
        .build();
    let filter = adw::ToggleGroup::new();
    for (name, label) in [("all", "All"), ("tcp", "TCP"), ("udp", "UDP")] {
        filter.add(adw::Toggle::builder().name(name).label(label).build());
    }
    filter.set_active_name(Some("all"));
    let controls = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    controls.append(&search);
    controls.append(&filter);

    // Listener list, or a status page when nothing matches
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .valign(gtk::Align::Start)
        .css_classes(["boxed-list"])
        .build();
    let empty = adw::StatusPage::builder()
        .icon_name("network-wired-disconnected-symbolic")
        .vexpand(true)
        .build();
    let stack = gtk::Stack::new();
    stack.add_named(&list, Some("list"));
    stack.add_named(&empty, Some("empty"));

    let page = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(18)
        .margin_top(18)
        .margin_bottom(18)
        .margin_start(12)
        .margin_end(12)
        .build();
    page.append(&controls);
    page.append(&stack);

    let clamp = adw::Clamp::builder().maximum_size(860).child(&page).build();
    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&clamp)
        .build();
    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&scroller));

    let view = adw::ToolbarView::new();
    view.add_top_bar(&header);
    view.set_content(Some(&toasts));

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Porthole")
        .default_width(720)
        .default_height(680)
        .width_request(360)
        .height_request(300)
        .content(&view)
        .build();

    let ui = Rc::new(Ui {
        window: window.clone(),
        title,
        toasts,
        search: search.clone(),
        filter: filter.clone(),
        stack,
        empty,
        list,
        auto_refresh,
        listeners: RefCell::new(Vec::new()),
    });

    refresh.connect_clicked(glib::clone!(#[weak] ui, move |_| ui.rescan(true)));
    search.connect_search_changed(glib::clone!(#[weak] ui, move |_| ui.render()));
    filter.connect_active_name_notify(glib::clone!(#[weak] ui, move |_| ui.render()));

    // Ctrl+F focuses search, Ctrl+R / F5 refreshes
    let keys = gtk::EventControllerKey::new();
    keys.connect_key_pressed(glib::clone!(
        #[weak] ui,
        #[upgrade_or] glib::Propagation::Proceed,
        move |_, key, _, mods| {
            let ctrl = mods.contains(gdk::ModifierType::CONTROL_MASK);
            match key {
                gdk::Key::F5 => ui.rescan(true),
                gdk::Key::r if ctrl => ui.rescan(true),
                gdk::Key::f if ctrl => { ui.search.grab_focus(); }
                _ => return glib::Propagation::Proceed,
            }
            glib::Propagation::Stop
        }));
    window.add_controller(keys);

    let weak = Rc::downgrade(&ui);
    glib::timeout_add_local(AUTO_REFRESH, move || {
        let Some(ui) = weak.upgrade() else {
            return glib::ControlFlow::Break;
        };
        if ui.auto_refresh.is_active() {
            ui.rescan(false);
        }
        glib::ControlFlow::Continue
    });

    ui.rescan(true);
    window.present();
}

impl Ui {
    /// Re-reads sockets. Only rebuilds the list when something changed so
    /// auto-refresh doesn't disturb scrolling or focus.
    fn rescan(self: &Rc<Self>, force: bool) {
        let fresh = sockets::scan();
        if !force && *self.listeners.borrow() == fresh {
            return;
        }
        *self.listeners.borrow_mut() = fresh;
        self.render();
    }

    fn render(self: &Rc<Self>) {
        self.list.remove_all();
        let query = self.search.text().trim().to_lowercase();
        let proto = match self.filter.active_name().as_deref() {
            Some("tcp") => Some(Proto::Tcp),
            Some("udp") => Some(Proto::Udp),
            _ => None,
        };
        let listeners = self.listeners.borrow();

        let visible: Vec<&Listener> = listeners
            .iter()
            .filter(|l| proto.is_none_or(|p| l.proto == p))
            .filter(|l| query.is_empty() || matches_query(l, &query))
            .collect();

        for l in &visible {
            self.list.append(&self.build_row(l));
        }

        if visible.is_empty() {
            if listeners.is_empty() {
                self.empty.set_title("No Listening Ports");
                self.empty.set_description(Some("Nothing on this machine is accepting connections"));
            } else {
                self.empty.set_title("No Matches");
                self.empty.set_description(Some("Try a different port, process name or PID"));
            }
            self.stack.set_visible_child_name("empty");
        } else {
            self.stack.set_visible_child_name("list");
        }

        let mut subtitle = if visible.len() == listeners.len() {
            format!("{} listening sockets", listeners.len())
        } else {
            format!("{} of {} listening sockets", visible.len(), listeners.len())
        };
        let foreign = listeners.iter().filter(|l| l.process.is_none()).count();
        if foreign > 0 {
            subtitle.push_str(&format!(" · {foreign} owned by other users"));
        }
        self.title.set_subtitle(&subtitle);
    }

    fn build_row(self: &Rc<Self>, l: &Listener) -> adw::ActionRow {
        let row = adw::ActionRow::builder()
            .use_markup(false)
            .subtitle_lines(2)
            .build();

        let port = gtk::Label::builder()
            .label(format!(":{}", l.port))
            .xalign(0.0)
            .width_chars(7)
            .css_classes(["port"])
            .build();
        row.add_prefix(&port);

        let proto = gtk::Label::builder()
            .label(l.proto.label())
            .valign(gtk::Align::Center)
            .css_classes(["proto", if l.proto == Proto::Tcp { "tcp" } else { "udp" }])
            .build();
        row.add_suffix(&proto);

        match &l.process {
            Some(p) => {
                row.set_title(&p.name);
                let mut subtitle = format!("PID {} · {} · {}", p.pid, l.user, l.address_label());
                if !p.cmdline.is_empty() {
                    subtitle.push('\n');
                    subtitle.push_str(&p.cmdline);
                }
                row.set_subtitle(&subtitle);
                row.set_tooltip_text(Some(&p.cmdline));

                let kill = gtk::Button::builder()
                    .label("Kill")
                    .valign(gtk::Align::Center)
                    .css_classes(["destructive-action"])
                    .build();
                let (ui, l) = (Rc::downgrade(self), l.clone());
                kill.connect_clicked(move |_| {
                    if let Some(ui) = ui.upgrade() {
                        glib::spawn_future_local(ui.confirm_kill(l.clone()));
                    }
                });
                row.add_suffix(&kill);
            }
            None => {
                row.set_title("Unknown process");
                row.set_subtitle(&format!(
                    "{} · {}\nOwned by another user — run with sudo to see details",
                    l.user,
                    l.address_label()
                ));
                row.add_css_class("foreign");
            }
        }
        row
    }

    async fn confirm_kill(self: Rc<Self>, l: Listener) {
        let Some(p) = &l.process else { return };
        let dialog = adw::AlertDialog::builder()
            .heading(format!("Stop “{}”?", p.name))
            .body(format!(
                "PID {} is listening on {} port {}.\n\n\
                 Terminate asks the process to shut down cleanly. \
                 Force Kill stops it immediately and may lose unsaved data.",
                p.pid,
                l.proto.label(),
                l.port
            ))
            .close_response("cancel")
            .default_response("term")
            .build();
        dialog.add_responses(&[("cancel", "Cancel"), ("kill", "Force Kill"), ("term", "Terminate")]);
        dialog.set_response_appearance("kill", adw::ResponseAppearance::Destructive);
        dialog.set_response_appearance("term", adw::ResponseAppearance::Suggested);

        let signal = match dialog.choose_future(Some(&self.window)).await.as_str() {
            "kill" => Signal::Kill,
            "term" => Signal::Term,
            _ => return,
        };

        match sockets::send_signal(p.pid, signal) {
            Ok(()) => self.signal_sent(p, signal),
            Err(KillError::PermissionDenied) => self.offer_elevated_kill(p, signal).await,
            Err(KillError::Other(e)) => self.toast(&format!("Couldn't signal PID {}: {e}", p.pid)),
        }
    }

    async fn offer_elevated_kill(self: &Rc<Self>, p: &sockets::Process, signal: Signal) {
        let dialog = adw::AlertDialog::builder()
            .heading("Permission Denied")
            .body(format!(
                "“{}” (PID {}) belongs to another user. Stopping it requires administrator rights.",
                p.name, p.pid
            ))
            .close_response("cancel")
            .default_response("auth")
            .build();
        dialog.add_responses(&[("cancel", "Cancel"), ("auth", "Authenticate…")]);
        dialog.set_response_appearance("auth", adw::ResponseAppearance::Suggested);
        if dialog.choose_future(Some(&self.window)).await != "auth" {
            return;
        }

        let pid = p.pid.to_string();
        let argv = ["pkexec", "kill", "-s", signal.name(), pid.as_str()].map(std::ffi::OsStr::new);
        let result = match gio::Subprocess::newv(&argv, gio::SubprocessFlags::STDERR_SILENCE) {
            Ok(proc) => proc.wait_check_future().await,
            Err(e) => Err(e),
        };
        match result {
            Ok(()) => self.signal_sent(p, signal),
            Err(e) => self.toast(&format!("Administrator kill failed: {}", e.message())),
        }
    }

    fn signal_sent(self: &Rc<Self>, p: &sockets::Process, signal: Signal) {
        self.toast(&format!("Sent SIG{} to {} (PID {})", signal.name(), p.name, p.pid));
        // Give the process a moment to release its socket before rescanning.
        let weak = Rc::downgrade(self);
        glib::timeout_add_local_once(Duration::from_millis(400), move || {
            if let Some(ui) = weak.upgrade() {
                ui.rescan(true);
            }
        });
    }

    fn toast(&self, msg: &str) {
        let toast = adw::Toast::builder().title(msg).use_markup(false).build();
        self.toasts.add_toast(toast);
    }
}

fn matches_query(l: &Listener, q: &str) -> bool {
    let q = q.trim_start_matches(':');
    if l.port.to_string().contains(q) || l.user.to_lowercase().contains(q) {
        return true;
    }
    l.process.as_ref().is_some_and(|p| {
        p.pid.to_string() == q
            || p.name.to_lowercase().contains(q)
            || p.cmdline.to_lowercase().contains(q)
    })
}
