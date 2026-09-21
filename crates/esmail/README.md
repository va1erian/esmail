# esmail

An IMAP/SMTP mail client built on `egui` and litehtml (via
`egui-litehtml-webview`, this workspace's other crate). See `../../PLAN.md` for
the design and open work, and `../../HANDOFF.md` for how to build, run, and
verify changes.

**Internal to this workspace.** Not published to crates.io.

## Signing in

esmail signs in with a plain IMAP/SMTP username and password, or — for Gmail
only — with **Sign in with Google** (OAuth2), which needs no password of any
kind. Gmail and Outlook/Office 365 have both moved away from allowing plain
password auth for normal account passwords, so each needs one of the
following.

### Gmail: Sign in with Google (no app password)

Tick **Sign in with Google (no app password)** on the login screen (it
appears when the IMAP host is `imap.gmail.com`), enter your Gmail address as
the username, and click **Connect**. Your browser opens on Google's consent
page; approve, and esmail connects. The refresh token Google returns is kept in
your OS keyring, so later launches connect without the browser. If Google stops
accepting it (you revoked esmail, or it expired), Connect reports that and
**Sign in again** repeats the browser step.

esmail sends the resulting access token with SASL `XOAUTH2` over IMAP, the
IDLE connection and SMTP, and refreshes it automatically when it expires.

**Several Google accounts** work side by side, and next to password accounts:
the OAuth client below identifies esmail to Google and is shared, but every
account signs in on its own and has its own refresh token in the keyring. All
saved accounts connect at startup; one whose Google sign-in has lapsed shows
**Sign in again** in the folder pane and under Settings > Accounts, without
affecting the others.

**You have to register your own OAuth client.** Google only issues tokens to
registered applications, so esmail can't ship a working client id:

1. In [Google Cloud Console](https://console.cloud.google.com/), create a
   project and configure its OAuth consent screen (no API needs enabling —
   IMAP and SMTP are not APIs in that sense).
2. Create credentials → **OAuth client ID** → application type **Desktop app**.
   Note the client ID and client secret. (A desktop client's secret is not
   confidential; Google's token endpoint just requires it.)
3. Give them to esmail by any one of:
   - **Settings > Google**: paste the client ID and secret and
     click Save (this writes the `config.toml` entry below);
   - environment variables `ESMAIL_GOOGLE_CLIENT_ID` and
     `ESMAIL_GOOGLE_CLIENT_SECRET` (these take precedence over saved settings;
     the Settings window says so when they are set);
   - `config.toml` (next to the saved accounts):
     ```toml
     [google_oauth]
     client_id = "1234567890-abc.apps.googleusercontent.com"
     client_secret = "GOCSPX-..."
     ```
   - the same two variables set when *building* esmail, to bake them in.

Things to know about Google's side:

- `https://mail.google.com/` is a *restricted* scope. While your consent
  screen is in **Testing** mode, only the test users you list can sign in, and
  Google expires their refresh tokens after **7 days** (esmail then asks you to
  sign in again). Publishing the app removes that, but requires Google's
  verification for a restricted scope.
- Forgetting an account in esmail deletes the token locally only. To revoke it
  at Google, remove esmail under Google Account → Security → Third-party access.

### Gmail (app password) and Outlook

- **Gmail**: turn on 2-Step Verification, then create an
  [App Password](https://myaccount.google.com/apppasswords) and use that in
  esmail's Password field instead of your normal Google password.
- **Outlook/Office 365**: create an
  [app password](https://support.microsoft.com/en-us/account-billing/using-app-passwords-with-apps-that-don-t-support-two-step-verification-5896ed9b-4263-e681-128a-a6f2979a7944)
  under your Microsoft account's security settings and use that instead of
  your normal password.

The login screen's first-run wizard (type your email address, e.g.
`alice@gmail.com`, into the "Email address" field before any account is
saved) fills in the correct IMAP/SMTP hosts and ports for both providers
(and a handful of others — see `config::PROVIDERS`) automatically, and for
Gmail ticks **Sign in with Google** when an OAuth client is configured. For
Outlook, or Gmail without one, you still need an app password.

Any IMAP/SMTP server that accepts a plain username+password login (most
self-hosted and IMAP-friendly providers) works with your normal password, no
app password needed.

## Settings

The **Settings** button in the top bar opens a window with three tabs:

- **General**: the theme, a summary of connected accounts, and the keyboard
  shortcuts.
- **Accounts**: every saved account with its connection state, and Connect /
  Disconnect / Sign in again. **Edit…** opens that account's own dialog:
  display name, ports, SMTP host and security, the mailbox watched for new
  mail, and how it signs in (a new password, or Google). **Remove account…**
  there disconnects it and deletes its saved passwords and token. Username and
  IMAP host are the account's identity and are not editable; add the account
  again to change them.
- **Google**: the OAuth client ID and secret.

## Windows: installing, where things live, uninstalling

**Installer.** Releases include `esmail-<version>-setup.exe` (built from
`installer/esmail.iss` with Inno Setup, see `.github/workflows/build.yml`) as
well as a portable zip. It installs per user, with no administrator prompt, to
`%LOCALAPPDATA%\Programs\esMail`, and adds a Start-menu entry (and optionally a
desktop shortcut) and an "Apps & features" entry. A portable copy behaves the
same way at run time; it just has no shortcuts or uninstaller.

**Where esmail keeps things.** (`paths.rs`)

| What | Where |
| --- | --- |
| Settings and account list (`config.toml`) | `%APPDATA%\esmail\config` |
| Mail cache (`mails.db`), notification icon | `%LOCALAPPDATA%\esmail\data` |
| Passwords and OAuth refresh tokens | Windows Credential Manager (entries `<account>:imap`/`smtp`/`oauth.esmail`) |
| Attachments opened with "Open" | `%TEMP%\esmail-attachments` (emptied at start-up) |
| Toast notification name and icon | `HKCU\Software\Classes\AppUserModelId\io.github.va1erian.esmail` |

`ESMAIL_CONFIG_DIR` and `ESMAIL_DATA_DIR` relocate the first two, which is
handy for trying a build without touching your real profile.

**Shell integration.** esmail is a GUI-subsystem program (no console window),
has a real icon in the executable, window and tray (the tray glyph is drawn in
white on a dark taskbar and follows the system theme), and shows toasts under
its own name. Starting it while it is already running (for example from the
Start menu while it sits in the tray) brings the existing window forward
instead of starting a second copy.

**Uninstalling.** The uninstaller asks whether to remove esmail's data as well
(settings, cached mail, saved passwords). It does that by running
`esmail.exe --purge-data`, which you can also run yourself. A silent uninstall
keeps the data unless told otherwise:

```
unins000.exe /VERYSILENT /PURGE
```

To try the installer locally: `cargo build --release`, then
`iscc /DAppVersion=0.0.0 installer\esmail.iss` (Inno Setup 6); the result is in
`dist\`. That folder is git-ignored.
