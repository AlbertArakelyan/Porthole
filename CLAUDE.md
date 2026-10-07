# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

Porthole is a native Linux GUI (Rust, GTK4 + libadwaita) that lists listening TCP/UDP ports with their owning process and lets you kill it.

## Commands

```sh
cargo build                 # debug build
cargo run --release         # run the app (binary: target/release/porthole)
cargo test                  # unit tests (socket parsing in src/sockets.rs)
cargo test parses_ipv6      # run a single test by name filter
cargo clippy                # lint; the tree is expected to be warning-free
```

System deps: `libgtk-4-dev` and `libadwaita-1-dev` (GTK 4.12+, libadwaita 1.7+, matching the `v4_12` / `v1_7` crate features).

## Architecture

Two modules, split between data and UI:

- `src/sockets.rs` has no GTK dependency. `scan()` reads `/proc/net/{tcp,tcp6,udp,udp6}` directly (no `ss`/`lsof`), keeps TCP sockets in LISTEN state and unconnected UDP sockets, then maps socket inodes to processes by walking `/proc/<pid>/fd/*` symlinks (`socket:[inode]`). Addresses in `/proc/net` are native-endian hex words; IPv4-mapped IPv6 addresses are folded to IPv4. Processes of other users are invisible without root, so `Listener.process` is `Option` and the UI shows those as dimmed "Unknown process" rows with no Kill button. `send_signal()` wraps `libc::kill` and reports `EPERM` separately.
- `src/main.rs` is the libadwaita UI. A single `Rc<Ui>` struct holds widgets plus the current `Vec<Listener>`. `rescan()` re-reads `/proc` and only re-renders when the result differs (keeps auto-refresh from disturbing scroll/focus); `render()` rebuilds the `boxed-list` of `AdwActionRow`s from the cached data applying the search text and `AdwToggleGroup` protocol filter. Kill flow is async (`glib::spawn_future_local`): `AdwAlertDialog` (Terminate = SIGTERM, Force Kill = SIGKILL), and on permission denied a second dialog offers `pkexec kill` via `gio::Subprocess`. Feedback goes through `AdwToast`.

The app icon lives at `data/icons/hicolor/scalable/apps/<APP_ID>.svg` (standard icon theme layout, ready for packaging). `build.rs` compiles it into the binary via `data/resources.gresource.xml`; GtkApplication picks it up from the `/io/github/albertarakelyan/Porthole/icons` resource path, and `main()` sets it as the default window icon. If the app ID changes, rename the SVG and update the gresource prefix to match.

## Conventions and gotchas

- Crates are renamed in `Cargo.toml`: use `gtk::` and `adw::`, not `gtk4::` / `libadwaita::`.
- `glib::clone!` (glib 0.20+) uses attribute syntax: `glib::clone!(#[weak] ui, move |_| ...)`, not the old `@weak` form.
- Startup prints many `Theme parser error: gtk.css...` warnings from the user's system GTK theme (Greybird). They are harmless; libadwaita ignores the system theme.
- `data/screenshot.png` is the README screenshot. There is no Xvfb or xdotool on this machine, and synthetic X input lands on the user's real desktop, so capture by temporarily adding code that renders the window via `gtk::WidgetPaintable` + `renderer().render_texture()` + `save_to_png()`, then revert it.
- When stopping test servers with `pkill -f`, use a bracketed pattern (e.g. `'http[.]server'`) and don't start the servers in the same shell command, or pkill matches and kills its own shell.
- README and other docs: keep them short and plain, no em dashes.
