//! How a connection proves who it is: a password, or an OAuth2 access token
//! presented through SASL `XOAUTH2`.
//!
//! [`Auth`] is what `imap.rs`, `idle_watch.rs` and `smtp.rs` hold instead of a
//! bare password. It is asked for its secret *each time a connection is
//! opened* ([`Auth::secret`]) rather than once up front, because an OAuth
//! access token lives about an hour while esmail's connections are reopened
//! for as long as the process runs (reconnect after a drop, the IDLE watch's
//! backoff loop, the next send). A password just hands itself back.

use std::sync::Arc;

use secrecy::{ExposeSecret, SecretString};

use crate::oauth::TokenSource;

/// Cheap to clone -- an OAuth source is shared, so every connection of one
/// account (the actor's session, the body worker, IDLE, SMTP) reuses one
/// cached access token instead of each refreshing its own.
#[derive(Clone)]
pub enum Auth {
    Password(SecretString),
    OAuth(Arc<TokenSource>),
}

impl Auth {
    pub fn password(password: impl Into<SecretString>) -> Self {
        Self::Password(password.into())
    }

    pub fn is_oauth(&self) -> bool {
        matches!(self, Self::OAuth(_))
    }

    /// The password, or a currently valid access token (refreshed first if
    /// the cached one is about to expire).
    pub async fn secret(&self) -> anyhow::Result<SecretString> {
        match self {
            Self::Password(password) => Ok(password.clone()),
            Self::OAuth(source) => source.access_token().await,
        }
    }
}

/// The SASL `XOAUTH2` initial client response: `user=<user>^Aauth=Bearer
/// <token>^A^A` (`^A` is `\x01`). Google documents this format for both IMAP
/// and SMTP.
pub fn xoauth2_initial_response(user: &str, access_token: &SecretString) -> String {
    format!("user={user}\x01auth=Bearer {}\x01\x01", access_token.expose_secret())
}

/// `async_imap` authenticator for `AUTHENTICATE XOAUTH2`.
///
/// The server first sends an empty `+` continuation, which gets the initial
/// response. If the token is rejected it sends a second `+` carrying a
/// base64 JSON error description and waits for the client to acknowledge it
/// with an empty line before it answers `NO` -- so every later challenge is
/// answered with nothing, and the resulting `NO` becomes the login error.
pub struct XOAuth2 {
    response: Option<String>,
}

impl XOAuth2 {
    pub fn new(user: &str, access_token: &SecretString) -> Self {
        Self { response: Some(xoauth2_initial_response(user, access_token)) }
    }
}

impl async_imap::Authenticator for XOAuth2 {
    type Response = String;

    fn process(&mut self, _challenge: &[u8]) -> Self::Response {
        self.response.take().unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_imap::Authenticator;

    #[test]
    fn initial_response_matches_the_documented_xoauth2_layout() {
        let token = SecretString::from("ya29.token");
        assert_eq!(
            xoauth2_initial_response("alice@gmail.com", &token),
            "user=alice@gmail.com\x01auth=Bearer ya29.token\x01\x01"
        );
    }

    #[test]
    fn authenticator_answers_the_first_challenge_then_stays_silent() {
        let mut auth = XOAuth2::new("alice@gmail.com", &SecretString::from("t"));
        assert_eq!(auth.process(b""), "user=alice@gmail.com\x01auth=Bearer t\x01\x01");
        // The error-description challenge a server sends after rejecting the token.
        assert_eq!(auth.process(b"{\"status\":\"401\"}"), "");
    }

    #[tokio::test]
    async fn password_auth_hands_back_the_password() {
        let auth = Auth::password("hunter2");
        assert!(!auth.is_oauth());
        assert_eq!(auth.secret().await.unwrap().expose_secret(), "hunter2");
    }
}
