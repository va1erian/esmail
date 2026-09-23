//! Account configuration: a serde/TOML file in the platform config dir,
//! replacing the old 3-line `esmail_config.txt`. Passwords never live here —
//! see [`crate::secrets`] for those.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// Serialises `Config::save` calls. The theme toggle now writes from a
/// background thread (`config_saver`), and other preferences are still saved
/// from the UI thread; two `std::fs::write`s racing would truncate the file
/// mid-write and leave `config.toml` unparseable, which `Config::load` treats
/// as "no accounts".
static SAVE_LOCK: Mutex<()> = Mutex::new(());

/// How to secure a connection to a mail server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TlsMode {
    /// Implicit TLS from the first byte (IMAPS/SMTPS, ports 993/465).
    Ssl,
    /// Plaintext connection upgraded via `STARTTLS`.
    StartTls,
    /// No transport security. Only useful for local/test servers.
    None,
}

/// How an account signs in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthKind {
    /// Username + password (or an app password), sent with `LOGIN`/`AUTH PLAIN`.
    #[default]
    Password,
    /// "Sign in with Google": OAuth2 through the system browser, presented as
    /// SASL `XOAUTH2`. No password of any kind; the keyring holds a refresh
    /// token instead (see [`crate::oauth`]).
    GoogleOAuth,
}

/// A Google OAuth client to sign in with — see [`crate::oauth::google_client`]
/// for what it is and the other places it can come from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OAuthClientConfig {
    pub client_id: String,
    #[serde(default)]
    pub client_secret: Option<String>,
}

/// One configured mail account. Passwords are looked up separately, from the
/// OS keyring, keyed by `(id, "imap" | "smtp")` — see [`crate::secrets`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccountConfig {
    /// Stable identifier for this account, used as the keyring key and to
    /// match this entry across edits. Not shown in the UI.
    pub id: String,
    /// Human-readable label, e.g. "Work" or the email address.
    pub display_name: String,
    pub imap_host: String,
    pub imap_port: u16,
    pub imap_tls: TlsMode,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub smtp_tls: TlsMode,
    /// Used for both IMAP and SMTP auth.
    pub username: String,
    /// Absent in a `config.toml` written before OAuth2 existed, which is
    /// exactly the password case.
    #[serde(default)]
    pub auth: AuthKind,
    /// Mailbox watched for new mail (IDLE + polling, toasts). `None` means
    /// the default, `INBOX` -- see `session::DEFAULT_WATCH_MAILBOX`. Only
    /// settable by editing `config.toml` for now.
    #[serde(default)]
    pub watch_mailbox: Option<String>,
    /// Unix seconds a fresh "Sign in with Google" issued this account's
    /// refresh token, written so the app can warn before Google's Testing-mode
    /// 7-day expiry ([`crate::oauth::refresh_token_expiring_soon`]). `None`
    /// for a password account, or a token restored from the keyring whose
    /// issue time predates this field.
    #[serde(default)]
    pub oauth_token_issued_at: Option<i64>,
}

impl AccountConfig {
    /// A new account for `username`@`imap_host`, with the common IMAPS/SMTPS
    /// defaults (993/465, implicit TLS) and a guessed `smtp_host` (see
    /// [`derive_smtp_host`]) that the login screen lets the user override —
    /// there's no real provider-settings lookup (that's B9's first-run
    /// wizard, not done), just the common `imap.` → `smtp.` convention.
    pub fn new(display_name: String, imap_host: String, imap_port: u16, username: String) -> Self {
        Self {
            id: format!("{username}@{imap_host}"),
            display_name,
            smtp_host: derive_smtp_host(&imap_host),
            imap_host,
            imap_port,
            imap_tls: TlsMode::Ssl,
            smtp_port: 465,
            smtp_tls: TlsMode::Ssl,
            username,
            auth: AuthKind::Password,
            watch_mailbox: None,
            oauth_token_issued_at: None,
        }
    }
}

/// IMAP/SMTP settings for one known provider, looked up by email domain
/// (B9's first-run wizard). Unlike [`derive_smtp_host`] — a mechanical
/// `imap.` → `smtp.` string transform applied to a host the user already
/// typed — this goes the other way: from just the domain half of an email
/// address to a complete guess at both hosts, ports and TLS modes, for
/// providers where the mechanical convention doesn't hold (`gmail.com`'s
/// mail lives at `imap.gmail.com`, not `imap.gmail.com` derived from
/// `gmail.com` by any string rule; Outlook's consumer and Yahoo's IMAP host
/// names aren't `imap.<domain>` either).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProviderSettings {
    pub imap_host: &'static str,
    pub imap_port: u16,
    pub smtp_host: &'static str,
    pub smtp_port: u16,
}

/// A small built-in table of well-known consumer providers, keyed by the
/// domain half of an email address (lowercased). Deliberately short — this
/// is a convenience for the common case, not an attempt at the exhaustive
/// provider databases Thunderbird/Outlook ship; anything not listed here
/// falls back to [`derive_smtp_host`]'s mechanical guess once the user types
/// an IMAP host directly. Signing in is a separate matter: Gmail can use
/// OAuth2 ([`AuthKind::GoogleOAuth`], see [`crate::oauth`]), but Outlook
/// accounts still need an app password even though their connection settings
/// are guessed correctly here.
const PROVIDERS: &[(&str, ProviderSettings)] = &[
    (
        "gmail.com",
        ProviderSettings { imap_host: "imap.gmail.com", imap_port: 993, smtp_host: "smtp.gmail.com", smtp_port: 465 },
    ),
    (
        "googlemail.com",
        ProviderSettings { imap_host: "imap.gmail.com", imap_port: 993, smtp_host: "smtp.gmail.com", smtp_port: 465 },
    ),
    (
        "outlook.com",
        ProviderSettings { imap_host: "outlook.office365.com", imap_port: 993, smtp_host: "smtp.office365.com", smtp_port: 587 },
    ),
    (
        "hotmail.com",
        ProviderSettings { imap_host: "outlook.office365.com", imap_port: 993, smtp_host: "smtp.office365.com", smtp_port: 587 },
    ),
    (
        "live.com",
        ProviderSettings { imap_host: "outlook.office365.com", imap_port: 993, smtp_host: "smtp.office365.com", smtp_port: 587 },
    ),
    (
        "yahoo.com",
        ProviderSettings { imap_host: "imap.mail.yahoo.com", imap_port: 993, smtp_host: "smtp.mail.yahoo.com", smtp_port: 465 },
    ),
    (
        "icloud.com",
        ProviderSettings { imap_host: "imap.mail.me.com", imap_port: 993, smtp_host: "smtp.mail.me.com", smtp_port: 587 },
    ),
    (
        "me.com",
        ProviderSettings { imap_host: "imap.mail.me.com", imap_port: 993, smtp_host: "smtp.mail.me.com", smtp_port: 587 },
    ),
    (
        "fastmail.com",
        ProviderSettings { imap_host: "imap.fastmail.com", imap_port: 993, smtp_host: "smtp.fastmail.com", smtp_port: 465 },
    ),
    (
        "gmx.com",
        ProviderSettings { imap_host: "imap.gmx.com", imap_port: 993, smtp_host: "smtp.gmx.com", smtp_port: 465 },
    ),
    (
        "zoho.com",
        ProviderSettings { imap_host: "imap.zoho.com", imap_port: 993, smtp_host: "smtp.zoho.com", smtp_port: 465 },
    ),
];

/// Look up known settings for `email`'s domain, case-insensitively. Returns
/// `None` for an address with no `@`, an empty domain, or a domain not in
/// [`PROVIDERS`] — the caller (the login screen) falls back to letting the
/// user type the IMAP host directly, from which [`derive_smtp_host`] takes
/// over.
pub fn provider_for_email(email: &str) -> Option<ProviderSettings> {
    let domain = email.rsplit_once('@')?.1.trim().to_ascii_lowercase();
    if domain.is_empty() {
        return None;
    }
    PROVIDERS
        .iter()
        .find(|(d, _)| *d == domain)
        .map(|(_, settings)| *settings)
}

/// Guess an SMTP host from an IMAP one, using the common `imap.` → `smtp.`
/// naming convention (e.g. `imap.gmail.com` → `smtp.gmail.com`). Falls back
/// to prefixing `smtp.` when the IMAP host doesn't start with `imap.` (e.g.
/// `mail.example.com` → `smtp.mail.example.com`) — not always right, but a
/// starting point the login screen lets the user edit rather than leaving
/// the field empty. There is no real per-provider settings lookup here
/// (that's B9's first-run wizard).
fn derive_smtp_host(imap_host: &str) -> String {
    match imap_host.strip_prefix("imap.") {
        Some(rest) => format!("smtp.{rest}"),
        None => format!("smtp.{imap_host}"),
    }
}

/// Light/dark theme preference (B9). Deliberately its own type rather than
/// reusing `egui::ThemePreference` — `config.rs` otherwise has no dependency
/// on egui, and keeping it that way means these variants (and their `serde`
/// round-trip, tested below) don't depend on egui's own `serde` feature flag
/// being enabled. `main.rs` converts to/from `egui::ThemePreference` at the
/// one call site that needs it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThemeMode {
    Dark,
    Light,
    #[default]
    System,
}

impl ThemeMode {
    /// Cycle Dark -> Light -> System -> Dark, for a single toggle button
    /// rather than a picker with three separate options.
    pub fn next(self) -> Self {
        match self {
            Self::Dark => Self::Light,
            Self::Light => Self::System,
            Self::System => Self::Dark,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Dark => "Dark",
            Self::Light => "Light",
            Self::System => "System",
        }
    }
}

/// Saved window position/size (B9), in the same "monitor space, ui points"
/// units `egui::ViewportInfo::outer_rect` reports them in. `f32` (not a
/// screen-pixel integer type) to match that directly with no conversion.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WindowGeometry {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// All configured accounts. Serialized as TOML to
/// `<config dir>/esmail/config.toml`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub accounts: Vec<AccountConfig>,
    #[serde(default)]
    pub theme: ThemeMode,
    /// `None` until a window has actually been closed once — the very first
    /// run uses eframe's own built-in default size instead of forcing one.
    #[serde(default)]
    pub window: Option<WindowGeometry>,
    /// The Google OAuth client to use for "Sign in with Google" (an
    /// environment variable, or a build-time one, can supply it instead).
    #[serde(default)]
    pub google_oauth: Option<OAuthClientConfig>,
    /// Sender addresses (lowercased) whose remote images load without asking:
    /// the "Always load from ..." button on the remote-images bar. A set, so
    /// trusting the same sender twice is a no-op and the file stays sorted.
    #[serde(default)]
    pub image_trusted_senders: BTreeSet<String>,
    /// Mailbox-tree nodes the user collapsed, as [`collapsed_folder_key`]s.
    #[serde(default)]
    pub collapsed_folders: BTreeSet<String>,
}

/// The `Config::collapsed_folders` entry for the tree node `folder_key`
/// (`imap::MailboxRow::key`) of account `account_id`. Keyed per account so
/// two accounts that both have an `INBOX/Work` don't share a fold state.
pub fn collapsed_folder_key(account_id: &str, folder_key: &str) -> String {
    format!("{account_id}\t{folder_key}")
}

/// The old plain-text config file this format replaces, so first-run
/// migration knows where to look.
fn legacy_config_path() -> PathBuf {
    let appdata = std::env::var("APPDATA").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(appdata).join("esmail_config.txt")
}

impl Config {
    /// Load `config.toml`, or an empty config if it does not exist yet or
    /// fails to parse (rather than refusing to start).
    pub fn load() -> Self {
        let Some(path) = crate::paths::config_file() else {
            return Self::default();
        };
        match std::fs::read_to_string(&path) {
            Ok(content) => toml::from_str(&content).unwrap_or_else(|e| {
                log::warn!("could not parse {}: {e}; starting with no accounts", path.display());
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    /// Write `config.toml`, creating the config directory if needed. Serialised
    /// against every other save, including the background `config_saver`'s.
    pub fn save(&self) -> anyhow::Result<()> {
        let _guard = SAVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let path = crate::paths::config_file().ok_or_else(|| anyhow::anyhow!("no config directory available"))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let toml = toml::to_string_pretty(self)?;
        std::fs::write(path, toml)?;
        Ok(())
    }

    /// If there are no accounts yet, read the legacy `esmail_config.txt`
    /// (host/port/username — it never held a password) and turn it into a
    /// single account. Returns `true` if a legacy file was found and
    /// migrated, so the caller knows to persist the result.
    ///
    /// The legacy file is left in place: this only reads it, so a user who
    /// has not yet accepted this branch's config format is not surprised by
    /// a deleted file.
    pub fn migrate_legacy(&mut self) -> bool {
        let Ok(content) = std::fs::read_to_string(legacy_config_path()) else {
            return false;
        };
        self.migrate_legacy_content(&content)
    }

    /// The parsing half of [`Config::migrate_legacy`], split out so it can be
    /// tested without touching the real `%APPDATA%`.
    fn migrate_legacy_content(&mut self, content: &str) -> bool {
        if !self.accounts.is_empty() {
            return false;
        }
        let lines: Vec<&str> = content.lines().collect();
        if lines.len() < 3 {
            return false;
        }
        let host = lines[0].trim().to_string();
        let port: u16 = lines[1].trim().parse().unwrap_or(993);
        let username = lines[2].trim().to_string();
        if host.is_empty() || username.is_empty() {
            return false;
        }
        log::info!("migrating legacy config for {username}@{host} into config.toml");
        self.accounts.push(AccountConfig::new(username.clone(), host, port, username));
        true
    }

    /// Insert `account`, or replace the existing entry with the same `id`.
    ///
    /// A `watch_mailbox` set in `config.toml` by hand survives being
    /// re-added through the login form, which has no field for it.
    pub fn upsert_account(&mut self, mut account: AccountConfig) {
        if let Some(existing) = self.accounts.iter_mut().find(|a| a.id == account.id) {
            if account.watch_mailbox.is_none() {
                account.watch_mailbox = existing.watch_mailbox.take();
            }
            *existing = account;
        } else {
            self.accounts.push(account);
        }
    }

    /// Remove the account with this `id`, if any.
    pub fn remove_account(&mut self, id: &str) {
        self.accounts.retain(|a| a.id != id);
    }

    /// Whether remote images load automatically for mail from `address`
    /// (compared case-insensitively).
    pub fn is_image_trusted(&self, address: &str) -> bool {
        self.image_trusted_senders.contains(&address.to_ascii_lowercase())
    }

    /// Start (`trusted`) or stop trusting `address`'s remote images. Returns
    /// whether the set changed, i.e. whether the config needs saving.
    pub fn set_image_trusted(&mut self, address: &str, trusted: bool) -> bool {
        let address = address.to_ascii_lowercase();
        if trusted {
            self.image_trusted_senders.insert(address)
        } else {
            self.image_trusted_senders.remove(&address)
        }
    }

    /// Fold or unfold a mailbox-tree node. Returns whether anything changed.
    pub fn set_folder_collapsed(&mut self, account_id: &str, folder_key: &str, collapsed: bool) -> bool {
        let key = collapsed_folder_key(account_id, folder_key);
        if collapsed {
            self.collapsed_folders.insert(key)
        } else {
            self.collapsed_folders.remove(&key)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_keeps_a_hand_edited_watch_mailbox_and_an_old_config_still_parses() {
        let mut config = Config::default();
        let mut edited = AccountConfig::new("A".into(), "imap.example.com".into(), 993, "alice".into());
        edited.watch_mailbox = Some("Work".into());
        config.upsert_account(edited);
        config.upsert_account(AccountConfig::new("A".into(), "imap.example.com".into(), 993, "alice".into()));
        assert_eq!(config.accounts.len(), 1);
        assert_eq!(config.accounts[0].watch_mailbox.as_deref(), Some("Work"));

        // A config.toml written before the field existed has no such key.
        let old = "[[accounts]]
id = \"a@h\"
display_name = \"a\"
imap_host = \"h\"
imap_port = 993
imap_tls = \"Ssl\"
smtp_host = \"s\"
smtp_port = 465
smtp_tls = \"Ssl\"
username = \"a\"
";
        let parsed: Config = toml::from_str(old).expect("old config parses");
        assert_eq!(parsed.accounts[0].watch_mailbox, None);
    }

    #[test]
    fn account_id_combines_username_and_host_so_two_accounts_on_one_host_differ() {
        let a = AccountConfig::new("A".into(), "imap.example.com".into(), 993, "alice".into());
        let b = AccountConfig::new("B".into(), "imap.example.com".into(), 993, "bob".into());
        assert_ne!(a.id, b.id);
    }

    #[test]
    fn derive_smtp_host_swaps_the_imap_prefix() {
        assert_eq!(derive_smtp_host("imap.gmail.com"), "smtp.gmail.com");
    }

    #[test]
    fn derive_smtp_host_prefixes_when_there_is_no_imap_prefix_to_swap() {
        assert_eq!(derive_smtp_host("mail.example.com"), "smtp.mail.example.com");
    }

    #[test]
    fn new_account_guesses_its_smtp_host() {
        let account = AccountConfig::new("Home".into(), "imap.example.com".into(), 993, "alice".into());
        assert_eq!(account.smtp_host, "smtp.example.com");
    }

    #[test]
    fn config_round_trips_through_toml() {
        let mut config = Config::default();
        config.accounts.push(AccountConfig::new(
            "Home".into(),
            "imap.example.com".into(),
            993,
            "alice".into(),
        ));

        let toml = toml::to_string_pretty(&config).unwrap();
        let parsed: Config = toml::from_str(&toml).unwrap();
        assert_eq!(parsed, config);
    }

    #[test]
    fn upsert_replaces_the_matching_account_rather_than_duplicating_it() {
        let mut config = Config::default();
        let mut account = AccountConfig::new("Home".into(), "imap.example.com".into(), 993, "alice".into());
        config.upsert_account(account.clone());

        account.display_name = "Home (renamed)".into();
        config.upsert_account(account.clone());

        assert_eq!(config.accounts.len(), 1);
        assert_eq!(config.accounts[0].display_name, "Home (renamed)");
    }

    #[test]
    fn upsert_appends_when_the_id_is_new() {
        let mut config = Config::default();
        config.upsert_account(AccountConfig::new("Home".into(), "a.example.com".into(), 993, "alice".into()));
        config.upsert_account(AccountConfig::new("Work".into(), "b.example.com".into(), 993, "alice".into()));
        assert_eq!(config.accounts.len(), 2);
    }

    #[test]
    fn remove_account_drops_only_the_matching_id() {
        let mut config = Config::default();
        config.upsert_account(AccountConfig::new("Home".into(), "a.example.com".into(), 993, "alice".into()));
        let keep = AccountConfig::new("Work".into(), "b.example.com".into(), 993, "alice".into());
        config.upsert_account(keep.clone());

        config.remove_account(&AccountConfig::new("Home".into(), "a.example.com".into(), 993, "alice".into()).id);

        assert_eq!(config.accounts, vec![keep]);
    }

    #[test]
    fn migrate_legacy_parses_the_three_line_format() {
        let mut config = Config::default();
        let migrated = config.migrate_legacy_content("imap.gmail.com\n993\nalice@gmail.com\n");
        assert!(migrated);
        assert_eq!(config.accounts.len(), 1);
        assert_eq!(config.accounts[0].imap_host, "imap.gmail.com");
        assert_eq!(config.accounts[0].imap_port, 993);
        assert_eq!(config.accounts[0].username, "alice@gmail.com");
    }

    #[test]
    fn migrate_legacy_does_nothing_if_accounts_already_exist() {
        // Otherwise a real config would be silently clobbered by a stale
        // esmail_config.txt left over from before this format existed.
        let mut config = Config::default();
        config.upsert_account(AccountConfig::new("Home".into(), "imap.example.com".into(), 993, "alice".into()));
        let before = config.clone();

        let migrated = config.migrate_legacy_content("imap.gmail.com\n993\nalice@gmail.com\n");

        assert!(!migrated);
        assert_eq!(config, before);
    }

    #[test]
    fn migrate_legacy_rejects_malformed_or_empty_input() {
        let mut config = Config::default();
        assert!(!config.migrate_legacy_content(""));
        assert!(!config.migrate_legacy_content("only one line"));
        assert!(!config.migrate_legacy_content("\n993\nalice")); // empty host
        assert_eq!(config.accounts.len(), 0);
    }

    // ── theme ─────────────────────────────────────────────────────────────

    #[test]
    fn theme_mode_defaults_to_system() {
        assert_eq!(ThemeMode::default(), ThemeMode::System);
    }

    #[test]
    fn theme_mode_cycles_dark_light_system() {
        assert_eq!(ThemeMode::Dark.next(), ThemeMode::Light);
        assert_eq!(ThemeMode::Light.next(), ThemeMode::System);
        assert_eq!(ThemeMode::System.next(), ThemeMode::Dark);
    }

    #[test]
    fn config_with_theme_and_window_round_trips_through_toml() {
        let config = Config {
            accounts: vec![],
            theme: ThemeMode::Dark,
            window: Some(WindowGeometry { x: 10.0, y: 20.0, width: 800.0, height: 600.0 }),
            ..Config::default()
        };
        let toml = toml::to_string_pretty(&config).unwrap();
        let parsed: Config = toml::from_str(&toml).unwrap();
        assert_eq!(parsed, config);
    }

    #[test]
    fn config_with_trusted_senders_and_collapsed_folders_round_trips_through_toml() {
        let mut config = Config::default();
        config.upsert_account(AccountConfig::new("A".into(), "imap.example.com".into(), 993, "alice".into()));
        config.set_image_trusted("news@example.com", true);
        config.set_folder_collapsed("alice@imap.example.com", "Work", true);
        let toml = toml::to_string_pretty(&config).unwrap();
        let parsed: Config = toml::from_str(&toml).unwrap();
        assert_eq!(parsed, config);
    }

    #[test]
    fn config_without_trusted_senders_or_collapsed_folders_still_parses() {
        let parsed: Config = toml::from_str("accounts = []\n").unwrap();
        assert!(parsed.image_trusted_senders.is_empty());
        assert!(parsed.collapsed_folders.is_empty());
    }

    #[test]
    fn image_trust_is_case_insensitive_and_reports_whether_it_changed() {
        let mut config = Config::default();
        assert!(!config.is_image_trusted("News@Example.com"));
        assert!(config.set_image_trusted("News@Example.com", true));
        assert!(config.is_image_trusted("news@example.COM"));
        assert!(!config.set_image_trusted("news@example.com", true), "already trusted");
        assert!(config.set_image_trusted("NEWS@example.com", false));
        assert!(!config.is_image_trusted("news@example.com"));
        assert!(!config.set_image_trusted("news@example.com", false), "already untrusted");
    }

    #[test]
    fn folder_collapse_state_is_kept_per_account() {
        let mut config = Config::default();
        assert!(config.set_folder_collapsed("a", "Work", true));
        assert!(config.collapsed_folders.contains(&collapsed_folder_key("a", "Work")));
        assert!(!config.collapsed_folders.contains(&collapsed_folder_key("b", "Work")));
        assert!(config.set_folder_collapsed("a", "Work", false));
        assert!(config.collapsed_folders.is_empty());
    }

    /// An old `config.toml` written before B9 added `theme`/`window` has
    /// neither field; `#[serde(default)]` must still parse it rather than
    /// erroring the whole file out (which would silently drop every saved
    /// account, per `Config::load`'s fallback).
    #[test]
    fn config_without_theme_or_window_fields_still_parses() {
        let toml = "accounts = []\n";
        let parsed: Config = toml::from_str(toml).unwrap();
        assert_eq!(parsed.theme, ThemeMode::System);
        assert_eq!(parsed.window, None);
    }

    // ── auth kind / OAuth client ─────────────────────────────────────────────

    /// An account saved before OAuth2 existed has no `auth` key; it must load
    /// as a password account, not fail the whole file (which `Config::load`
    /// would turn into "no accounts").
    #[test]
    fn account_without_an_auth_key_is_a_password_account() {
        let toml = r#"
            [[accounts]]
            id = "alice@imap.example.com"
            display_name = "alice"
            imap_host = "imap.example.com"
            imap_port = 993
            imap_tls = "Ssl"
            smtp_host = "smtp.example.com"
            smtp_port = 465
            smtp_tls = "Ssl"
            username = "alice"
        "#;
        let parsed: Config = toml::from_str(toml).unwrap();
        assert_eq!(parsed.accounts[0].auth, AuthKind::Password);
        assert_eq!(parsed.accounts[0].oauth_token_issued_at, None);
        assert_eq!(parsed.google_oauth, None);
    }

    #[test]
    fn oauth_account_and_client_round_trip_through_toml() {
        let mut account = AccountConfig::new("Me".into(), "imap.gmail.com".into(), 993, "me@gmail.com".into());
        account.auth = AuthKind::GoogleOAuth;
        account.oauth_token_issued_at = Some(1_700_000_000);
        let config = Config {
            accounts: vec![account],
            google_oauth: Some(OAuthClientConfig { client_id: "id".into(), client_secret: Some("secret".into()) }),
            ..Config::default()
        };
        let toml = toml::to_string_pretty(&config).unwrap();
        let parsed: Config = toml::from_str(&toml).unwrap();
        assert_eq!(parsed, config);
    }

    #[test]
    fn oauth_client_secret_is_optional() {
        let parsed: Config = toml::from_str("[google_oauth]\nclient_id = \"id\"\n").unwrap();
        assert_eq!(parsed.google_oauth, Some(OAuthClientConfig { client_id: "id".into(), client_secret: None }));
    }

    // ── provider_for_email ──────────────────────────────────────────────────

    #[test]
    fn provider_for_email_finds_gmail() {
        let settings = provider_for_email("alice@gmail.com").unwrap();
        assert_eq!(settings.imap_host, "imap.gmail.com");
        assert_eq!(settings.smtp_host, "smtp.gmail.com");
        assert_eq!(settings.smtp_port, 465);
    }

    #[test]
    fn provider_for_email_is_case_insensitive_on_the_domain() {
        let settings = provider_for_email("Alice@GMAIL.COM").unwrap();
        assert_eq!(settings.imap_host, "imap.gmail.com");
    }

    #[test]
    fn provider_for_email_finds_outlook_with_starttls() {
        let settings = provider_for_email("bob@outlook.com").unwrap();
        assert_eq!(settings.imap_host, "outlook.office365.com");
        assert_eq!(settings.smtp_port, 587);
    }

    #[test]
    fn provider_for_email_returns_none_for_unknown_domains() {
        assert_eq!(provider_for_email("alice@my-own-mail-server.example"), None);
    }

    #[test]
    fn provider_for_email_returns_none_without_an_at_sign() {
        assert_eq!(provider_for_email("not-an-email"), None);
    }

    #[test]
    fn provider_for_email_returns_none_for_an_empty_domain() {
        assert_eq!(provider_for_email("alice@"), None);
    }
}
