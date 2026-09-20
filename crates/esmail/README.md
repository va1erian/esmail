# esmail

An IMAP/SMTP mail client built on `egui` and litehtml (via
`egui-litehtml-webview`, this workspace's other crate). See `../../PLAN.md` for
the design and open work, and `../../HANDOFF.md` for how to build, run, and
verify changes.

**Internal to this workspace.** Not published to crates.io.

## Signing in — Gmail and Outlook need an app password

esmail authenticates with plain IMAP/SMTP username+password login. It does
**not** support OAuth2 — that's explicitly out of scope for v1 (see PLAN.md
§B9).

Gmail and Outlook/Office 365 have both moved away from allowing plain
password auth for normal account passwords. To sign into either from esmail:

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
(and a handful of others — see `config::PROVIDERS`) automatically; it still
cannot authenticate for you without an app password, since that half is
exactly what OAuth2 would otherwise replace.

Any IMAP/SMTP server that accepts a plain username+password login (most
self-hosted and IMAP-friendly providers) works with your normal password, no
app password needed.
