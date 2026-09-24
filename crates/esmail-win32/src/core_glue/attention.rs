//! Which account needs the user's attention about its sign-in, and what to say:
//! an account that cannot sign in, or a Google refresh token that is about to
//! stop working (Google expires them after seven days while an app is in
//! "Testing"). Plain data, so the wording and the priorities are unit tested.

use esmail::config::{AccountConfig, AuthKind};
use esmail::oauth::refresh_token_expiring_soon;

/// What the notice bar shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notice {
    /// The account's index.
    pub account: usize,
    /// The line to show.
    pub text: String,
    /// The button's label: it opens the account's form.
    pub action: &'static str,
    /// How many further accounts also need attention.
    pub others: usize,
}

/// The first account that needs attention, if any. `failures` holds, for each
/// account, why it cannot sign in (`None` while it is fine); a failure outranks
/// a token that is merely about to expire. `now` is unix seconds.
pub fn notice(accounts: &[AccountConfig], failures: &[Option<String>], now: i64) -> Option<Notice> {
    let mut found: Vec<(usize, String, &'static str)> = Vec::new();
    for (index, account) in accounts.iter().enumerate() {
        let google = account.auth == AuthKind::GoogleOAuth;
        let failure = failures.get(index).and_then(Option::as_deref);
        if let Some(error) = failure {
            let first_line = error.lines().next().unwrap_or_default();
            found.push((index, format!("{}: could not sign in: {first_line}", account.display_name), if google { "Sign in again..." } else { "Reconnect..." }));
        } else if google && refresh_token_expiring_soon(account.oauth_token_issued_at, now) {
            found.push((index, format!("{}: Google sign-in expires soon.", account.display_name), "Sign in again..."));
        }
    }
    // Failures were pushed in account order alongside warnings; show a failure first.
    let first = found.iter().position(|(index, ..)| failures.get(*index).is_some_and(Option::is_some)).unwrap_or(0);
    let others = found.len().checked_sub(1)?;
    let (account, text, action) = found.swap_remove(first);
    Some(Notice { account, text, action, others })
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 24 * 60 * 60;

    fn account(name: &str, google: bool, issued: Option<i64>) -> AccountConfig {
        let mut account = AccountConfig::new(name.into(), "imap.example.com".into(), 993, format!("{name}@example.com"));
        if google {
            account.auth = AuthKind::GoogleOAuth;
            account.oauth_token_issued_at = issued;
        }
        account
    }

    #[test]
    fn nothing_to_say_when_every_account_is_fine() {
        let accounts = [account("Work", false, None), account("Me", true, Some(0))];
        assert_eq!(notice(&accounts, &[None, None], 2 * DAY), None);
        assert_eq!(notice(&[], &[], 0), None);
    }

    #[test]
    fn a_google_token_near_its_seven_day_expiry_is_flagged_before_it_fails() {
        let accounts = [account("Me", true, Some(0))];
        let flagged = notice(&accounts, &[None], 6 * DAY).unwrap();
        assert_eq!((flagged.text.as_str(), flagged.action, flagged.others), ("Me: Google sign-in expires soon.", "Sign in again...", 0));
    }

    #[test]
    fn a_password_account_that_cannot_sign_in_is_offered_a_reconnect() {
        let accounts = [account("Work", false, None)];
        let flagged = notice(&accounts, &[Some("LOGIN failed\nmore".into())], 0).unwrap();
        assert_eq!((flagged.text.as_str(), flagged.action), ("Work: could not sign in: LOGIN failed", "Reconnect..."));
    }

    #[test]
    fn a_failure_outranks_an_expiry_and_the_rest_are_counted() {
        let accounts = [account("Me", true, Some(0)), account("Work", false, None), account("Home", true, Some(0))];
        let flagged = notice(&accounts, &[None, Some("bad password".into()), None], 6 * DAY).unwrap();
        assert_eq!((flagged.account, flagged.others), (1, 2));
    }
}
