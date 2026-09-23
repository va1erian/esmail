//! In-memory mailbox state shared between the IMAP and SMTP halves of the
//! mock server. An SMTP `DATA` delivery appends straight into the
//! recipient's mailbox here, so "send then fetch" round-trips work without
//! esmail ever issuing `APPEND` (it doesn't -- see smtp.rs's doc comment).

use std::collections::HashMap;
use std::sync::Mutex;

use tokio::sync::broadcast;

/// One stored message: the raw RFC822 bytes (source of truth, what
/// `UID FETCH ... RFC822` returns) plus the envelope fields extracted once
/// so `FETCH (UID ENVELOPE)` doesn't reparse on every request.
#[derive(Debug, Clone)]
pub struct StoredMessage {
    pub uid: u32,
    pub raw: Vec<u8>,
    pub envelope: Envelope,
    /// IMAP flags on this message (e.g. `"\Seen"`, `"\Flagged"`, `"\Deleted"`),
    /// mutated by `STORE`/`UID STORE` (B8) and consulted by `EXPUNGE`/`STATUS
    /// (UNSEEN)`. Empty for a freshly delivered message -- nothing marks
    /// anything `\Seen` on arrival, matching a real server.
    pub flags: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Envelope {
    pub date: String,
    pub subject: String,
    /// (display name, mailbox local-part, host) -- just the first address,
    /// same simplification `imap.rs::format_address` makes on the client
    /// side, which is all these tests need.
    pub from: Option<(String, String, String)>,
    pub to: Option<(String, String, String)>,
    pub message_id: String,
}

pub struct Mailbox {
    pub name: String,
    pub messages: Vec<StoredMessage>,
    pub uid_validity: u32,
    pub uid_next: u32,
}

impl Mailbox {
    fn new(name: &str, uid_validity: u32) -> Self {
        Mailbox { name: name.to_string(), messages: Vec::new(), uid_validity, uid_next: 1 }
    }

    pub fn append(&mut self, raw: Vec<u8>, envelope: Envelope) -> u32 {
        let uid = self.uid_next;
        self.uid_next += 1;
        self.messages.push(StoredMessage { uid, raw, envelope, flags: Vec::new() });
        uid
    }

    /// Messages with no `\Seen` flag -- what `STATUS (UNSEEN)` (B8) counts.
    pub fn unseen_count(&self) -> u32 {
        self.messages.iter().filter(|m| !m.flags.iter().any(|f| f == "\\Seen")).count() as u32
    }

    /// Applies `add`/`remove` to the flags of the message with the given
    /// `uid`, returning its resulting flag list. `None` if no message in
    /// this mailbox has that UID.
    pub fn store_flags(&mut self, uid: u32, add: &[String], remove: &[String]) -> Option<Vec<String>> {
        let msg = self.messages.iter_mut().find(|m| m.uid == uid)?;
        // Remove before add: a plain (replace-the-whole-set) STORE is
        // modeled by the caller as "remove every known system flag, then
        // add the new set" (imap_server.rs), so `remove` and `add` can
        // legitimately share members there. Removing first means a flag
        // that's in both ends up present, matching "replace with this set"
        // -- doing it the other way around (as this used to) would add the
        // new flags and then immediately strip them again via the wildcard
        // remove list, leaving every replace-mode STORE with an empty flag
        // set. The +FLAGS/-FLAGS cases (where `add`/`remove` are always
        // disjoint, one of them empty) are unaffected by the order.
        msg.flags.retain(|f| !remove.iter().any(|r| r.eq_ignore_ascii_case(f)));
        for flag in add {
            if !msg.flags.iter().any(|f| f.eq_ignore_ascii_case(flag)) {
                msg.flags.push(flag.clone());
            }
        }
        Some(msg.flags.clone())
    }

    /// Removes every message flagged `\Deleted` from this mailbox (`EXPUNGE`,
    /// B8) -- the fallback path `imap.rs::move_message` uses when a server
    /// doesn't support `MOVE`. Returns the removed UIDs.
    pub fn expunge(&mut self) -> Vec<u32> {
        let mut removed = Vec::new();
        self.messages.retain(|m| {
            let deleted = m.flags.iter().any(|f| f == "\\Deleted");
            if deleted {
                removed.push(m.uid);
            }
            !deleted
        });
        removed
    }
}

pub struct Store {
    pub users: HashMap<String, String>,
    /// `XOAUTH2` access tokens by user -- see [`Store::add_oauth_token`].
    pub oauth_tokens: HashMap<String, String>,
    pub mailboxes: HashMap<String, Mailbox>,
    /// Broadcasts the name of any mailbox a delivery just landed in, so an
    /// `IDLE` connection selected on that mailbox can push an untagged
    /// `EXISTS` instead of the client having to poll (see `imap_server.rs`'s
    /// `IDLE` handling). A plain `Mutex`-guarded field rather than something
    /// fancier: `broadcast` already handles the "zero or many idling
    /// connections care about this" fan-out, and `send` on no subscribers is
    /// just a no-op `Err` every `deliver` call already ignores.
    pub notify: broadcast::Sender<String>,
    /// Test-only fault injection (issue #80): when set, the next
    /// `UID FETCH ... RFC822` -- the fetch `imap.rs::fetch_body`/`fetch_raw`
    /// issue -- makes the server drop the connection instead of answering,
    /// simulating the dead/aborted socket from the bug report. The flag
    /// clears on use, so the client's retry on a fresh connection succeeds.
    /// Never set by production code; the integration test that exercises the
    /// body worker's one-command retry flips it directly.
    pub drop_next_rfc822_fetch: bool,
}

impl Store {
    pub fn new() -> Self {
        let (notify, _) = broadcast::channel(32);
        Store {
            users: HashMap::new(),
            oauth_tokens: HashMap::new(),
            mailboxes: HashMap::new(),
            notify,
            drop_next_rfc822_fetch: false,
        }
    }

    pub fn add_user(&mut self, username: &str, password: &str) {
        self.users.insert(username.to_string(), password.to_string());
        // A fresh account gets the usual special-use mailboxes so LIST has
        // something realistic to return and SMTP delivery always has an
        // INBOX to land in.
        for name in ["INBOX", "Sent", "Drafts", "Trash"] {
            self.mailboxes.entry(name.to_string()).or_insert_with(|| Mailbox::new(name, 1));
        }
    }

    pub fn check_login(&self, username: &str, password: &str) -> bool {
        self.users.get(username).is_some_and(|p| p == password)
    }

    /// Accept `token` as `username`'s OAuth access token, for `XOAUTH2`.
    /// The tokens are independent of the password: a client that authenticated
    /// with one of these provably did not use `LOGIN`/`AUTH PLAIN`.
    pub fn add_oauth_token(&mut self, username: &str, token: &str) {
        self.oauth_tokens.insert(username.to_string(), token.to_string());
    }

    /// Check a decoded `XOAUTH2` initial client response
    /// (`user=<user>^Aauth=Bearer <token>^A^A`), returning the user it
    /// authenticated as.
    pub fn check_xoauth2(&self, payload: &[u8]) -> Option<String> {
        let text = std::str::from_utf8(payload).ok()?;
        let mut user = None;
        let mut token = None;
        for field in text.split('\x01') {
            if let Some(v) = field.strip_prefix("user=") {
                user = Some(v);
            } else if let Some(v) = field.strip_prefix("auth=Bearer ") {
                token = Some(v);
            }
        }
        let (user, token) = user.zip(token)?;
        (self.oauth_tokens.get(user).is_some_and(|t| t == token)).then(|| user.to_string())
    }

    pub fn mailbox_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.mailboxes.keys().cloned().collect();
        names.sort();
        names
    }

    pub fn mailbox(&self, name: &str) -> Option<&Mailbox> {
        self.mailboxes.get(name)
    }

    pub fn mailbox_mut(&mut self, name: &str) -> Option<&mut Mailbox> {
        self.mailboxes.get_mut(name)
    }

    pub fn deliver(&mut self, recipient_mailbox: &str, raw: Vec<u8>) -> u32 {
        let envelope = parse_envelope(&raw);
        let mailbox = self
            .mailboxes
            .entry(recipient_mailbox.to_string())
            .or_insert_with(|| Mailbox::new(recipient_mailbox, 1));
        let uid = mailbox.append(raw, envelope);
        let _ = self.notify.send(recipient_mailbox.to_string());
        uid
    }

    /// `COPY`/`UID COPY` (B8): duplicates the message `uid` from `src` into
    /// `dest` (creating `dest` if it doesn't exist yet, same as `deliver`),
    /// with a fresh UID and its flags carried over. Used by
    /// `imap.rs::move_message`'s COPY+STORE+EXPUNGE fallback for servers
    /// without `MOVE`. Returns the new UID, or `None` if `src`/`uid` doesn't
    /// exist.
    pub fn copy_message(&mut self, src: &str, uid: u32, dest: &str) -> Option<u32> {
        let msg = self.mailboxes.get(src)?.messages.iter().find(|m| m.uid == uid)?.clone();
        let dest_mailbox = self.mailboxes.entry(dest.to_string()).or_insert_with(|| Mailbox::new(dest, 1));
        let new_uid = dest_mailbox.uid_next;
        dest_mailbox.uid_next += 1;
        dest_mailbox.messages.push(StoredMessage { uid: new_uid, flags: msg.flags.clone(), ..msg });
        let _ = self.notify.send(dest.to_string());
        Some(new_uid)
    }
}

/// Extracts the handful of headers `FETCH ENVELOPE` needs from a raw
/// message, for mail delivered live over SMTP (fixture messages build their
/// `Envelope` directly instead -- see fixtures.rs).
pub fn parse_envelope(raw: &[u8]) -> Envelope {
    let parsed = match mailparse::parse_mail(raw) {
        Ok(p) => p,
        Err(_) => return Envelope::default(),
    };
    let header = |name: &str| -> String {
        parsed.headers.iter().find(|h| h.get_key_ref().eq_ignore_ascii_case(name)).map(|h| h.get_value()).unwrap_or_default()
    };
    let address = |name: &str| -> Option<(String, String, String)> {
        let raw = header(name);
        if raw.is_empty() {
            return None;
        }
        let addrs = mailparse::addrparse(&raw).ok()?;
        match addrs.first()? {
            mailparse::MailAddr::Single(info) => {
                let (mailbox, host) = info.addr.split_once('@').unwrap_or((info.addr.as_str(), ""));
                Some((info.display_name.clone().unwrap_or_default(), mailbox.to_string(), host.to_string()))
            }
            mailparse::MailAddr::Group(_) => None,
        }
    };

    Envelope {
        date: header("Date"),
        subject: header("Subject"),
        from: address("From"),
        to: address("To"),
        message_id: header("Message-ID"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_flags_replace_mode_keeps_the_new_set_instead_of_ending_up_empty() {
        // Regression test for a real bug: imap_server.rs models a plain
        // (replace-the-whole-set) STORE as "remove every known system flag,
        // then add the new set" -- calling store_flags with the new set as
        // both members of `add` and (via the wildcard) `remove`. Applying
        // `add` before `remove` (the original order) stripped the just-added
        // flags right back out, so every replace-mode STORE ended up empty.
        let mut mailbox = Mailbox::new("INBOX", 1);
        let uid = mailbox.append(b"raw".to_vec(), Envelope::default());

        let all_system_flags: Vec<String> =
            ["\\Seen", "\\Flagged", "\\Deleted", "\\Answered", "\\Draft"].iter().map(|s| s.to_string()).collect();
        let result = mailbox.store_flags(uid, &["\\Seen".to_string()], &all_system_flags);

        assert_eq!(result, Some(vec!["\\Seen".to_string()]));
    }

    #[test]
    fn store_flags_add_then_remove_are_unaffected_by_the_reordering() {
        // The +FLAGS/-FLAGS cases always pass a disjoint, one-sided
        // add/remove pair -- confirm the fix (remove-then-add) doesn't
        // change their behavior.
        let mut mailbox = Mailbox::new("INBOX", 1);
        let uid = mailbox.append(b"raw".to_vec(), Envelope::default());

        assert_eq!(mailbox.store_flags(uid, &["\\Seen".to_string()], &[]), Some(vec!["\\Seen".to_string()]));
        assert_eq!(mailbox.store_flags(uid, &[], &["\\Seen".to_string()]), Some(vec![]));
    }
}

pub type SharedStore = std::sync::Arc<Mutex<Store>>;
