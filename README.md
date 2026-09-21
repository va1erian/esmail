# esMail

A small, fast desktop and 100% vibe-coded mail client for IMAP and SMTP, written in Rust.

- Reads and sends mail over IMAP/SMTP, with several accounts side by side.
- Renders HTML mail with [litehtml](https://github.com/litehtml/litehtml) instead of an embedded browser: no JavaScript engine, a small binary, and message HTML is sanitised before it is shown.
- Signs in with a password or, for Gmail, with Sign in with Google (OAuth2, no app password).
- On Windows, lives in the system tray, shows new-mail notifications and comes with an installer that uninstalls cleanly. A Linux build is also provided.

Prebuilt Windows and Linux binaries are attached to each
[release](https://github.com/va1erian/esmail/releases). How to sign in, where
esMail keeps its files and how to uninstall are described in
[crates/esmail/README.md](crates/esmail/README.md).

## Building

You need a stable [Rust toolchain](https://rustup.rs) and a C/C++ compiler,
because litehtml is compiled from source.

- **Windows:** the "Desktop development with C++" workload of the Visual Studio
  Build Tools (MSVC).
- **Linux (Debian/Ubuntu):**
  ```
  sudo apt-get install build-essential pkg-config libssl-dev libdbus-1-dev
  ```

Then:

```
cargo build --release     # target/release/esmail (esmail.exe on Windows)
cargo run --release       # build and start it
cargo test --workspace    # unit tests
```

The IMAP/SMTP tests run esMail's real network code against an in-process
server. They are ignored by default; see
[crates/mail-mock-server/README.md](crates/mail-mock-server/README.md) for how
to run them.

To build the Windows installer, install [Inno Setup 6](https://jrsoftware.org/isinfo.php)
and run, after a release build:

```
iscc /DAppVersion=0.1.0 installer\esmail.iss
```

The result is in `dist\`. [PLAN.md](PLAN.md) and [HANDOFF.md](HANDOFF.md) describe
the design and how the code is organised.

## License

esMail is free software, licensed under the
[GNU General Public License version 3](LICENSE) (GPL-3.0-only).

## Credits

esMail stands on the work of many others. Every Rust dependency keeps its own
license, listed in its crate; the ones that ask for a mention are:

- **[Twemoji](https://github.com/twitter/twemoji)** graphics, used to draw emoji,
  are copyright 2020 Twitter, Inc and other contributors, licensed under
  [CC-BY 4.0](https://creativecommons.org/licenses/by/4.0/). They come through
  the [`twemoji-assets`](https://github.com/cptpiepmatz/twemoji-assets) crate
  by Tim "Piepmatz" Hesse.
- **[litehtml](https://github.com/litehtml/litehtml)**, the HTML/CSS layout
  engine, is copyright 2013 Yuri Kobets (tordex), BSD 3-Clause. It is used
  through the Rust bindings [`litehtml-rs`](https://github.com/franzos/litehtml-rs)
  by franzos, in [a fork](https://github.com/va1erian/litehtml-rs) with a
  Windows build fix and a few additions.
- **[egui](https://github.com/emilk/egui)** and **eframe** by Emil Ernerfeldt
  and contributors (MIT or Apache-2.0) provide the user interface. The fonts
  bundled with egui (Ubuntu-Light, Hack, Noto Emoji and emoji-icon-font, under
  the Ubuntu Font Licence, OFL-1.1 and MIT/Apache-2.0) are used as-is.
- **[SQLite](https://sqlite.org)** (public domain), through
  [`rusqlite`](https://github.com/rusqlite/rusqlite), caches mail on disk.
