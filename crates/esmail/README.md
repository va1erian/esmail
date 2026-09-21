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

**You have to register your own OAuth client.** Google only issues tokens to
registered applications, so esmail can't ship a working client id:

1. In [Google Cloud Console](https://console.cloud.google.com/), create a
   project and configure its OAuth consent screen (no API needs enabling —
   IMAP and SMTP are not APIs in that sense).
2. Create credentials → **OAuth client ID** → application type **Desktop app**.
   Note the client ID and client secret. (A desktop client's secret is not
   confidential; Google's token endpoint just requires it.)
3. Give them to esmail by any one of:
   - the **Settings** button in the top bar: paste the client ID and secret and
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
