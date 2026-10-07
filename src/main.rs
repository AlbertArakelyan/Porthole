mod sockets;

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use gtk::prelude::*;
use gtk::{gdk, gio, glib};

use sockets::{KillError, Listener, Proto, Signal};

const APP_ID: &str = "dev.albert.PortInspector";
const AUTO_REFRESH: Duration = Duration::from_secs(2);

const CSS: &str = "
.port {
    font-family: monospace;
    font-size: 1.35em;
    font-weight: bold;
}
.proto {
    font-size: 0.75em;
    font-weight: bold;
    padding: 1px 6px;
    border-radius: 999px;
    background: alpha(currentColor, 0.1);
}
.proto.tcp { color: #1c71d8; }
.proto.udp { color: #c64600; }
.process-name { font-weight: bold; }
.cmdline { font-family: monospace; font-size: 0.85em; }
list.listeners row { padding: 8px 12px; }
list.listeners row.foreign { opacity: 0.6; }
";

#[derive(Clone, Copy, PartialEq)]
enum ProtoFilter {
    All,
    Tcp,
    Udp,
}

struct Ui {
    window: gtk::ApplicationWindow,
    search: gtk::SearchEntry,
    list: gtk::ListBox,
    status: gtk::Label,
    auto_refresh: gtk::ToggleButton,
    listeners: RefCell<Vec<Listener>>,
    filter: Cell<ProtoFilter>,
}

fn main() -> glib::ExitCode {
    let app = gtk::Application::builder().application_id(APP_ID).build();
    app.connect_startup(|_| load_css());
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

fn build_ui(app: &gtk::Application) {
    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title("Port Inspector")
        .default_width(640)
        .default_height(620)
        .build();

    // Top bar: search + refresh
    let search = gtk::SearchEntry::builder()
        .placeholder_text("Filter by port, process, PID or user…")
        .hexpand(true)
        .build();
    let refresh = gtk::Button::with_label("Refresh");
    refresh.add_css_class("suggested-action");
    let top = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    top.append(&search);
    top.append(&refresh);

    // Listener list
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .show_separators(true)
        .css_classes(["listeners"])
        .build();
    list.set_placeholder(Some(&gtk::Label::new(Some("Nothing to show"))));
    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&list)
        .build();
    let frame = gtk::Frame::builder().child(&scroller).build();

    // Bottom bar: status, protocol filter, auto-refresh
    let status = gtk::Label::builder()
        .xalign(0.0)
        .hexpand(true)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .css_classes(["dim-label"])
        .build();
    let all = gtk::ToggleButton::builder().label("All").active(true).build();
    let tcp = gtk::ToggleButton::builder().label("TCP").group(&all).build();
    let udp = gtk::ToggleButton::builder().label("UDP").group(&all).build();
    let filters = gtk::Box::builder().css_classes(["linked"]).build();
    filters.append(&all);
    filters.append(&tcp);
    filters.append(&udp);
    let auto_refresh = gtk::ToggleButton::builder()
        .label("Auto-refresh")
        .active(true)
        .tooltip_text("Rescan every 2 seconds")
        .build();
    let bottom = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    bottom.append(&status);
    bottom.append(&filters);
    bottom.append(&auto_refresh);

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();
    content.append(&top);
    content.append(&frame);
    content.append(&bottom);
    window.set_child(Some(&content));

    let ui = Rc::new(Ui {
        window: window.clone(),
        search: search.clone(),
        list,
        status,
        auto_refresh,
        listeners: RefCell::new(Vec::new()),
        filter: Cell::new(ProtoFilter::All),
    });

    refresh.connect_clicked(glib::clone!(#[weak] ui, move |_| ui.rescan(true)));
    search.connect_search_changed(glib::clone!(#[weak] ui, move |_| ui.render()));
    for (button, filter) in [(&all, ProtoFilter::All), (&tcp, ProtoFilter::Tcp), (&udp, ProtoFilter::Udp)] {
        button.connect_toggled(glib::clone!(#[weak] ui, move |b| {
            if b.is_active() {
                ui.filter.set(filter);
                ui.render();
            }
        }));
    }

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
        let filter = self.filter.get();
        let listeners = self.listeners.borrow();

        let visible: Vec<&Listener> = listeners
            .iter()
            .filter(|l| match filter {
                ProtoFilter::All => true,
                ProtoFilter::Tcp => l.proto == Proto::Tcp,
                ProtoFilter::Udp => l.proto == Proto::Udp,
            })
            .filter(|l| query.is_empty() || matches_query(l, &query))
            .collect();

        for l in &visible {
            self.list.append(&self.build_row(l));
        }

        let hidden_owner = listeners.iter().filter(|l| l.process.is_none()).count();
        let mut text = if visible.len() == listeners.len() {
            format!("{} listening sockets", listeners.len())
        } else {
            format!("{} of {} listening sockets", visible.len(), listeners.len())
        };
        if hidden_owner > 0 {
            text.push_str(&format!(" · {hidden_owner} owned by other users"));
        }
        self.status.set_text(&text);
    }

    fn build_row(self: &Rc<Self>, l: &Listener) -> gtk::ListBoxRow {
        let port = gtk::Label::builder()
            .label(format!(":{}", l.port))
            .xalign(0.0)
            .width_chars(7)
            .css_classes(["port"])
            .selectable(true)
            .build();

        let proto = gtk::Label::builder()
            .label(l.proto.label())
            .valign(gtk::Align::Center)
            .css_classes(["proto", if l.proto == Proto::Tcp { "tcp" } else { "udp" }])
            .build();
        let name = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["process-name"])
            .build();
        let title = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        title.append(&name);
        title.append(&proto);

        let details = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["dim-label", "caption"])
            .build();
        let cmdline = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::Middle)
            .css_classes(["dim-label", "cmdline"])
            .build();

        let kill = gtk::Button::builder()
            .label("Kill")
            .valign(gtk::Align::Center)
            .css_classes(["destructive-action"])
            .build();

        match &l.process {
            Some(p) => {
                name.set_text(&p.name);
                details.set_text(&format!("PID {} · {} · {}", p.pid, l.user, l.address_label()));
                cmdline.set_text(&p.cmdline);
                cmdline.set_tooltip_text(Some(&p.cmdline));
                cmdline.set_visible(!p.cmdline.is_empty());

                let (ui, l) = (Rc::downgrade(self), l.clone());
                kill.connect_clicked(move |_| {
                    if let Some(ui) = ui.upgrade() {
                        glib::spawn_future_local(ui.confirm_kill(l.clone()));
                    }
                });
            }
            None => {
                name.set_text("Unknown process");
                details.set_text(&format!("{} · {}", l.user, l.address_label()));
                cmdline.set_text("Owned by another user — run with sudo to see details");
                kill.set_visible(false);
            }
        }

        let info = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .hexpand(true)
            .valign(gtk::Align::Center)
            .build();
        info.append(&title);
        info.append(&details);
        info.append(&cmdline);

        let hbox = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        hbox.append(&port);
        hbox.append(&info);
        hbox.append(&kill);

        let row = gtk::ListBoxRow::builder().child(&hbox).activatable(false).build();
        if l.process.is_none() {
            row.add_css_class("foreign");
        }
        row
    }

    async fn confirm_kill(self: Rc<Self>, l: Listener) {
        let Some(p) = &l.process else { return };
        let dialog = gtk::AlertDialog::builder()
            .modal(true)
            .message(format!("Stop “{}”?", p.name))
            .detail(format!(
                "PID {} is listening on {} port {}.\n\n\
                 Terminate asks the process to shut down cleanly. \
                 Force Kill stops it immediately and may lose unsaved data.",
                p.pid,
                l.proto.label(),
                l.port
            ))
            .buttons(["Cancel", "Force Kill", "Terminate"])
            .cancel_button(0)
            .default_button(2)
            .build();

        let signal = match dialog.choose_future(Some(&self.window)).await {
            Ok(1) => Signal::Kill,
            Ok(2) => Signal::Term,
            _ => return,
        };

        match sockets::send_signal(p.pid, signal) {
            Ok(()) => {
                self.flash(&format!("Sent SIG{} to {} (PID {})", signal.name(), p.name, p.pid));
                self.rescan_soon();
            }
            Err(KillError::PermissionDenied) => self.offer_elevated_kill(p, signal).await,
            Err(KillError::Other(e)) => self.flash(&format!("Couldn't signal PID {}: {e}", p.pid)),
        }
    }

    async fn offer_elevated_kill(self: &Rc<Self>, p: &sockets::Process, signal: Signal) {
        let dialog = gtk::AlertDialog::builder()
            .modal(true)
            .message("Permission Denied")
            .detail(format!(
                "“{}” (PID {}) belongs to another user. Stopping it requires administrator rights.",
                p.name, p.pid
            ))
            .buttons(["Cancel", "Authenticate…"])
            .cancel_button(0)
            .default_button(1)
            .build();
        if dialog.choose_future(Some(&self.window)).await != Ok(1) {
            return;
        }

        let pid = p.pid.to_string();
        let argv = ["pkexec", "kill", "-s", signal.name(), pid.as_str()].map(std::ffi::OsStr::new);
        let result = match gio::Subprocess::newv(&argv, gio::SubprocessFlags::STDERR_SILENCE) {
            Ok(proc) => proc.wait_check_future().await,
            Err(e) => Err(e),
        };
        match result {
            Ok(()) => {
                self.flash(&format!("Sent SIG{} to {} (PID {})", signal.name(), p.name, p.pid));
                self.rescan_soon();
            }
            Err(e) => self.flash(&format!("Administrator kill failed: {}", e.message())),
        }
    }

    /// Gives the process a moment to release its socket before rescanning.
    fn rescan_soon(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        glib::timeout_add_local_once(Duration::from_millis(400), move || {
            if let Some(ui) = weak.upgrade() {
                ui.rescan(true);
            }
        });
    }

    /// Shows a transient message in the status line.
    fn flash(self: &Rc<Self>, msg: &str) {
        self.status.set_text(msg);
        let weak = Rc::downgrade(self);
        glib::timeout_add_local_once(Duration::from_secs(4), move || {
            if let Some(ui) = weak.upgrade() {
                ui.render();
            }
        });
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
