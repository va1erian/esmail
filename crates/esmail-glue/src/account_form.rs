//! The account form as plain data: what the user typed, the server settings
//! guessed from the address, and the check that turns it into an
//! [`AccountConfig`]. It names no widget, so the rules are unit-tested here.

use esmail::config::{AccountConfig, AuthKind, TlsMode, provider_for_email};

/// The one host Google sign-in applies to.
pub const GMAIL_IMAP_HOST: &str = "imap.gmail.com";

/// The form's fields, as typed. Ports stay text until [`AccountForm::to_account`].
#[derive(Debug, Clone, PartialEq)]
pub struct AccountForm {
    /// The name shown for the account; the address when left empty.
    pub display_name: String,
    /// The address, which is also the login name.
    pub email: String,
    /// How to sign in.
    pub auth: AuthKind,
    /// The password; unused for Google sign-in.
    pub password: String,
    /// The IMAP server (always implicit TLS: the mail core has no other mode).
    pub imap_host: String,
    /// The IMAP port, as typed.
    pub imap_port: String,
    /// The SMTP server.
    pub smtp_host: String,
    /// The SMTP port, as typed.
    pub smtp_port: String,
    /// How the SMTP connection is secured.
    pub smtp_tls: TlsMode,
}

/// A field a [`FormError`] points at, so the window can focus it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    /// The email address.
    Email,
    /// The password.
    Password,
    /// The IMAP host.
    ImapHost,
    /// The IMAP port.
    ImapPort,
    /// The SMTP host.
    SmtpHost,
    /// The SMTP port.
    SmtpPort,
}

/// Why the form cannot become an account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormError {
    /// The field to fix.
    pub field: Field,
    /// What is wrong with it.
    pub message: &'static str,
}

/// Server settings guessed from an email address.
#[derive(Debug, Clone, PartialEq)]
pub struct Preset {
    /// The IMAP host.
    pub imap_host: String,
    /// The IMAP port.
    pub imap_port: u16,
    /// The SMTP host.
    pub smtp_host: String,
    /// The SMTP port.
    pub smtp_port: u16,
    /// The SMTP security.
    pub smtp_tls: TlsMode,
    /// The sign-in method to start with.
    pub auth: AuthKind,
}

/// A [`Preset`] and how far to trust it.
#[derive(Debug, Clone, PartialEq)]
pub struct Detection {
    /// The settings.
    pub preset: Preset,
    /// The domain is a provider esMail knows. Otherwise the hosts are the
    /// `imap.` / `smtp.` convention applied to the domain, and the user should
    /// see (and probably fix) them.
    pub known: bool,
}

/// The settings for `email`'s domain, or `None` while it has no usable domain.
pub fn detect(email: &str) -> Option<Detection> {
    let domain = email.rsplit_once('@')?.1.trim().to_ascii_lowercase();
    if !domain.contains('.') || domain.contains(char::is_whitespace) || domain.starts_with('.') || domain.ends_with('.') {
        return None;
    }
    let (preset, known) = match provider_for_email(email) {
        Some(provider) => {
            let auth = if provider.imap_host == GMAIL_IMAP_HOST { AuthKind::GoogleOAuth } else { AuthKind::Password };
            let preset = Preset {
                imap_host: provider.imap_host.to_string(),
                imap_port: provider.imap_port,
                smtp_host: provider.smtp_host.to_string(),
                smtp_port: provider.smtp_port,
                smtp_tls: smtp_security_for(provider.smtp_port),
                auth,
            };
            (preset, true)
        }
        None => {
            let preset = Preset {
                imap_host: format!("imap.{domain}"),
                imap_port: 993,
                smtp_host: format!("smtp.{domain}"),
                smtp_port: 465,
                smtp_tls: TlsMode::Ssl,
                auth: AuthKind::Password,
            };
            (preset, false)
        }
    };
    Some(Detection { preset, known })
}

/// Port 587 is the STARTTLS submission port; every other port the presets use
/// is implicit TLS.
fn smtp_security_for(port: u16) -> TlsMode {
    if port == 587 { TlsMode::StartTls } else { TlsMode::Ssl }
}

/// The security a user who only changes the SMTP port most likely wants.
pub fn security_for_smtp_port(port: u16) -> Option<TlsMode> {
    match port {
        465 => Some(TlsMode::Ssl),
        587 => Some(TlsMode::StartTls),
        _ => None,
    }
}

impl Default for AccountForm {
    fn default() -> Self {
        AccountForm {
            display_name: String::new(),
            email: String::new(),
            auth: AuthKind::Password,
            password: String::new(),
            imap_host: String::new(),
            imap_port: "993".to_string(),
            smtp_host: String::new(),
            smtp_port: "465".to_string(),
            smtp_tls: TlsMode::Ssl,
        }
    }
}

impl AccountForm {
    /// The form for editing `account`; the password comes from the keyring.
    pub fn from_account(account: &AccountConfig, password: String) -> AccountForm {
        AccountForm {
            display_name: account.display_name.clone(),
            email: account.username.clone(),
            auth: account.auth,
            password,
            imap_host: account.imap_host.clone(),
            imap_port: account.imap_port.to_string(),
            smtp_host: account.smtp_host.clone(),
            smtp_port: account.smtp_port.to_string(),
            smtp_tls: account.smtp_tls,
        }
    }

    /// Fills the server fields and the sign-in method from `preset`.
    pub fn apply(&mut self, preset: &Preset) {
        self.imap_host = preset.imap_host.clone();
        self.imap_port = preset.imap_port.to_string();
        self.smtp_host = preset.smtp_host.clone();
        self.smtp_port = preset.smtp_port.to_string();
        self.smtp_tls = preset.smtp_tls;
        self.auth = preset.auth;
    }

    /// The id a new account made from this form gets, the one the keyring and
    /// the cache know it by.
    pub fn id(&self) -> String {
        format!("{}@{}", self.email.trim(), self.imap_host.trim())
    }

    /// The account this form describes. `base` is the saved account being
    /// edited or reconnected: it keeps its id (so its keyring entries and cached
    /// mail stay its own even when the address changes) and the settings the
    /// form has no field for.
    pub fn to_account(&self, base: Option<&AccountConfig>) -> Result<AccountConfig, FormError> {
        let error = |field, message| Err(FormError { field, message });
        let email = self.email.trim();
        let (local, domain) = email.rsplit_once('@').unwrap_or_default();
        if local.is_empty() || !domain.contains('.') || email.contains(char::is_whitespace) {
            return error(Field::Email, "Enter your full email address, such as you@example.com.");
        }
        let imap_host = self.imap_host.trim();
        if !is_host(imap_host) {
            return error(Field::ImapHost, "Enter the IMAP server's host name, such as imap.example.com.");
        }
        let Some(imap_port) = parse_port(&self.imap_port) else {
            return error(Field::ImapPort, "The IMAP port must be a number from 1 to 65535.");
        };
        let smtp_host = self.smtp_host.trim();
        if !is_host(smtp_host) {
            return error(Field::SmtpHost, "Enter the SMTP server's host name, such as smtp.example.com.");
        }
        let Some(smtp_port) = parse_port(&self.smtp_port) else {
            return error(Field::SmtpPort, "The SMTP port must be a number from 1 to 65535.");
        };
        match self.auth {
            AuthKind::Password if self.password.is_empty() => return error(Field::Password, "Enter the account's password."),
            AuthKind::GoogleOAuth if imap_host != GMAIL_IMAP_HOST => {
                return error(Field::ImapHost, "Sign in with Google only works for Gmail (imap.gmail.com). Choose Password for other servers.");
            }
            _ => {}
        }
        let display_name = match self.display_name.trim() {
            "" => email,
            name => name,
        };
        let mut account = base
            .cloned()
            .unwrap_or_else(|| AccountConfig::new(display_name.to_string(), imap_host.to_string(), imap_port, email.to_string()));
        account.display_name = display_name.to_string();
        account.username = email.to_string();
        account.imap_host = imap_host.to_string();
        account.imap_port = imap_port;
        account.imap_tls = TlsMode::Ssl;
        account.smtp_host = smtp_host.to_string();
        account.smtp_port = smtp_port;
        account.smtp_tls = self.smtp_tls;
        account.auth = self.auth;
        Ok(account)
    }
}

fn parse_port(text: &str) -> Option<u16> {
    text.trim().parse().ok().filter(|port| *port != 0)
}

fn is_host(text: &str) -> bool {
    !text.is_empty() && !text.contains(char::is_whitespace) && !text.contains('/')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filled() -> AccountForm {
        AccountForm {
            display_name: "Work".into(),
            email: " alice@example.com ".into(),
            password: "hunter2".into(),
            imap_host: " imap.example.com ".into(),
            smtp_host: "smtp.example.com".into(),
            ..AccountForm::default()
        }
    }

    #[test]
    fn a_filled_form_becomes_a_trimmed_account() {
        let account = filled().to_account(None).unwrap();
        assert_eq!(account.id, "alice@example.com@imap.example.com");
        assert_eq!(account.display_name, "Work");
        assert_eq!((account.username.as_str(), account.imap_host.as_str(), account.imap_port), ("alice@example.com", "imap.example.com", 993));
        assert_eq!((account.smtp_host.as_str(), account.smtp_port, account.smtp_tls), ("smtp.example.com", 465, TlsMode::Ssl));
        assert_eq!(account.auth, AuthKind::Password);
        assert_eq!(filled().id(), account.id);
    }

    #[test]
    fn a_blank_name_falls_back_to_the_address() {
        let form = AccountForm { display_name: "  ".into(), ..filled() };
        assert_eq!(form.to_account(None).unwrap().display_name, "alice@example.com");
    }

    #[test]
    fn editing_keeps_the_id_and_what_the_form_has_no_field_for() {
        let mut saved = filled().to_account(None).unwrap();
        saved.watch_mailbox = Some("Alerts".into());
        let form = AccountForm { email: "alice@corp.example.com".into(), smtp_tls: TlsMode::StartTls, smtp_port: "587".into(), ..filled() };
        let edited = form.to_account(Some(&saved)).unwrap();
        assert_eq!(edited.id, saved.id, "the keyring entries stay reachable");
        assert_eq!(edited.username, "alice@corp.example.com");
        assert_eq!((edited.smtp_port, edited.smtp_tls), (587, TlsMode::StartTls));
        assert_eq!(edited.watch_mailbox.as_deref(), Some("Alerts"));
    }

    #[test]
    fn each_problem_points_at_its_field() {
        let field = |form: AccountForm| form.to_account(None).unwrap_err().field;
        assert_eq!(field(AccountForm { email: "alice".into(), ..filled() }), Field::Email);
        assert_eq!(field(AccountForm { email: "@example.com".into(), ..filled() }), Field::Email);
        assert_eq!(field(AccountForm { email: "alice@localhost".into(), ..filled() }), Field::Email);
        assert_eq!(field(AccountForm { imap_host: " ".into(), ..filled() }), Field::ImapHost);
        assert_eq!(field(AccountForm { imap_port: "0".into(), ..filled() }), Field::ImapPort);
        assert_eq!(field(AccountForm { imap_port: "70000".into(), ..filled() }), Field::ImapPort);
        assert_eq!(field(AccountForm { smtp_host: "".into(), ..filled() }), Field::SmtpHost);
        assert_eq!(field(AccountForm { smtp_port: "x".into(), ..filled() }), Field::SmtpPort);
        assert_eq!(field(AccountForm { password: String::new(), ..filled() }), Field::Password);
    }

    #[test]
    fn google_needs_no_password_but_needs_gmail() {
        let gmail = AccountForm {
            email: "me@gmail.com".into(),
            auth: AuthKind::GoogleOAuth,
            password: String::new(),
            imap_host: GMAIL_IMAP_HOST.into(),
            ..filled()
        };
        assert_eq!(gmail.to_account(None).unwrap().auth, AuthKind::GoogleOAuth);
        let elsewhere = AccountForm { imap_host: "imap.example.com".into(), ..gmail };
        assert_eq!(elsewhere.to_account(None).unwrap_err().field, Field::ImapHost);
    }

    #[test]
    fn gmail_defaults_to_google_sign_in() {
        let detected = detect("Me@GMail.com").unwrap();
        assert!(detected.known);
        assert_eq!(detected.preset.auth, AuthKind::GoogleOAuth);
        assert_eq!((detected.preset.imap_host.as_str(), detected.preset.imap_port), ("imap.gmail.com", 993));
        assert_eq!((detected.preset.smtp_host.as_str(), detected.preset.smtp_port, detected.preset.smtp_tls), ("smtp.gmail.com", 465, TlsMode::Ssl));
    }

    #[test]
    fn known_providers_use_their_own_ports_and_passwords() {
        let outlook = detect("me@outlook.com").unwrap();
        assert!(outlook.known);
        assert_eq!((outlook.preset.smtp_port, outlook.preset.smtp_tls, outlook.preset.auth), (587, TlsMode::StartTls, AuthKind::Password));
        assert_eq!(detect("me@icloud.com").unwrap().preset.imap_host, "imap.mail.me.com");
    }

    #[test]
    fn an_unknown_domain_gets_the_conventional_hosts_marked_as_a_guess() {
        let detected = detect("alice@example.com").unwrap();
        assert!(!detected.known);
        assert_eq!((detected.preset.imap_host.as_str(), detected.preset.smtp_host.as_str()), ("imap.example.com", "smtp.example.com"));
    }

    #[test]
    fn nothing_is_detected_before_a_domain_is_typed() {
        for email in ["", "alice", "alice@", "alice@exa", "alice@.com", "alice@example.", "alice@ex ample.com"] {
            assert!(detect(email).is_none(), "{email:?}");
        }
    }

    #[test]
    fn applying_a_preset_fills_every_server_field() {
        let mut form = AccountForm::default();
        form.apply(&detect("me@outlook.com").unwrap().preset);
        assert_eq!((form.imap_host.as_str(), form.smtp_port.as_str(), form.smtp_tls), ("outlook.office365.com", "587", TlsMode::StartTls));
    }

    #[test]
    fn the_smtp_port_suggests_its_security() {
        assert_eq!(security_for_smtp_port(465), Some(TlsMode::Ssl));
        assert_eq!(security_for_smtp_port(587), Some(TlsMode::StartTls));
        assert_eq!(security_for_smtp_port(25), None);
    }
}
