# A tiny resident listener, and a GUI that exits when its window closes

Status: **phases 0, 1 and 2a (the `ipc` module) done, the rest planned.** Nothing here changes the app's behaviour yet.

## Why

Closing the window only hides it (`ViewportCommand::Visible(false)`). The GL
context and driver, the egui context and font atlas, the litehtml worker and
its system-font database, and the whole `EsMailApp` all stay resident. Measured
with `scripts/measure-idle.ps1` on a release build, no accounts, isolated profile:

| | working set | private (commit) | threads |
|---|---|---|---|
| window open | 78 MB | 88 MB | 35 |
| hidden to tray (+20 s) | 86 MB | 97 MB | 35 |

Memory goes *up* after hiding. The NVIDIA OpenGL driver alone is ~150 MB of
mapped image. A real session with mail is larger.

## Goal

- The resident process holds only the tray, one IMAP watch per account and
  toasts. No egui, no GL, no webview, no fonts.
- Budget (an estimate, to be confirmed in phase 2): **<= 15 MB private, <= ~8
  threads, ~0 CPU when idle**, and no GL driver module loaded.
- Closing the GUI window ends the GUI *process*, so the OS reclaims everything.

## Architecture

- **Listener**: `esmail.exe --background`. One instance, holds `esmail.lock`.
  Owns the tray, the per-account `AccountSession`s (via `watcher::Watcher`),
  all toasts and the unread total for the tray tooltip. Current-thread tokio
  runtime. The tray needs a Win32 message pump: a winit event loop with no
  windows provides it, blocking with no polling and needing no `unsafe`.
- **GUI**: plain `esmail.exe`. Takes `ui.lock` (one GUI at a time). No tray;
  closing the window exits. Starts the listener first if it is not running.
  Its own sessions stay (it needs live mailbox data) but its toast hook is off
  while a listener is connected.
- **Same binary**: untouched code pages are not resident and the GL driver only
  loads when the GUI creates a context. A second exe is only worth it if
  phase 2 misses the budget.
- **Fallback**: on non-Windows, with `ESMAIL_NO_LISTENER=1`, or when the
  listener cannot start, the app keeps today's single-process behaviour
  (hide-to-tray, in-process toasts).

## IPC

An `ipc` module with one job: a local, authenticated byte stream between the
two processes. Same pattern as `platform/`: a small interface, one file per OS,
plus an in-memory transport for tests.

- `ipc::Transport`: `bind(endpoint)`, `accept(listener)`, `connect(endpoint)`,
  with a `Stream` that is `AsyncRead + AsyncWrite + Unpin + Send`. A transport
  must keep other users out by itself. Implementations: `LocalSocket` (below)
  and `Memory` (in-process, for tests). Anything else -- another OS, another
  mechanism -- is one more `impl Transport`.
- **`LocalSocket` uses the `interprocess` crate** (`local_socket`, tokio
  flavour). One dependency, safe on both OSes.
  - Windows: a named pipe. No port, no network stack. The first instance is
    created exclusively (a second listener on the name is refused; tested),
    remote clients are rejected, and the ACL is replaced with an **owner-only**
    one built from an SDDL string with `SecurityDescriptor::deserialize` (a
    safe function). Checked on a live pipe: protected DACL, a single `OWNER
    RIGHTS` entry, nobody else. tokio's default ACL would give Everyone and
    Anonymous read access.
  - Unix: a socket file in a directory created mode 0700 under
    `$XDG_RUNTIME_DIR` (else the temp dir). Type-checked for Linux locally and
    exercised by CI's Linux job; not run on a Unix machine by the author.
- Endpoint name = hash of the data directory, so two users on one machine do
  not collide and an isolated `ESMAIL_DATA_DIR` test profile gets its own.
- Defence in depth on top of the transport: a random token in a file in the
  data directory must be in the first message, and the listener sends nothing
  until it has read a valid hello.
- Protocol: JSON lines (bounded line length), generic over the stream, so the
  same tests run over `Memory` and over a real `LocalSocket`. Listener -> GUI:
  `Welcome`, `Show`, `OpenAccount(id)`, `Quit`. GUI -> listener: `Hello{token,
  version}`, `ConfigChanged`. A hello with the wrong token, another protocol
  version, garbage, an over-long line, or nothing within 3 s is dropped without
  a byte written back. An open connection means "a GUI is running"; its close
  means it exited.
- The token is 256 random bits from the OS, written to `ipc.token` in the data
  directory each time the listener starts (and removed by `--purge-data`).

## Phases

0. **Baseline and budget (done).** `scripts/measure-idle.ps1` (Gui and
   Background modes, optional `-MaxPrivateMB` gate) and this document.
1. **Headless core (done).** `auth::saved_auth` moves into the library, and
   `watcher::Watcher` owns one session per account with no UI: applies an
   account list (start / stop / replace), drains session events, tracks
   connection state and the watched mailbox's unread count, and fires the same
   notify hook the GUI uses. Tested against mail-mock-server.
2a. **The `ipc` module (done).** `Transport`, `LocalSocket`, `Memory`, the
   handshake, tokens; tested over both transports.
2b. **Listener process.** `--background` mode: lock, tray on a window-less
   winit loop, IPC server, config reload, persisting rotated OAuth refresh
   tokens to the keyring (a known gap today). Measure against the budget.
3. **GUI changes.** Remove `TrayState`, hide-to-tray and `exit_requested`;
   IPC client; spawn the listener; `--open-account`; fallback mode;
   `ConfigChanged` after Settings edits; second launch raises the running GUI.
4. **Lifecycle and installer.** `--quit` / tray Quit end the listener, which
   asks the GUI to exit first. Optional "Start esMail at sign-in" installer
   task and Settings toggle (`HKCU\...\Run` through `windows-registry`, opt-in).
5. **Polish and CI.** e2e script covers both processes and the memory budget;
   toast click opens the account through `--open-account`.

## Risks

- **Cold start**: opening the window is now a new process plus IMAP connect and
  header fetch. The SQLite cache can show cached lists first (follow-up).
- **Connection count**: GUI and listener each hold sessions per account. Fine for
  Gmail; if stricter servers object, the fix is a lean single-connection
  watcher (IDLE, then fetch on the same connection).
- **Foreground focus**: a GUI launched from a toast click should get foreground
  rights; needs testing.
