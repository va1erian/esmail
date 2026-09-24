//! [`AppCore::pump`]: everything the accounts' IMAP sessions produced since
//! the frontend last looked, applied to the core state.

use crate::db::DbCommand;
use crate::imap::{self, ImapCommand, ImapEvent};
use crate::progress::{Progress, ProgressKind};
use crate::session::AccountId;

use super::{AppCore, Changes, ConnState};

impl AppCore {
    /// Route what every account's session produced since the last call.
    /// Events that change per-account state (mailbox tree, unread counts,
    /// connection state, cache bookkeeping) apply to the account they name;
    /// events about the message list and the reading pane only apply while
    /// that account is the `active` one, since those show one account's
    /// mailbox at a time.
    ///
    /// Returns what the frontend has to do besides redrawing from the state.
    pub fn pump(&mut self) -> Changes {
        while let Ok((account, evt)) = self.events_rx.try_recv() {
            self.handle_imap_event(account, evt);
        }
        self.take_changes()
    }

    fn handle_imap_event(&mut self, account: AccountId, evt: ImapEvent) {
        // The account was removed while this was still queued.
        if self.view(&account).is_none() {
            return;
        }
        let is_active = self.active.as_deref() == Some(account.as_str());
        match evt {
            ImapEvent::Connected => {
                let from_form = self.view_mut(&account).is_some_and(|view| {
                    view.state = ConnState::Connected;
                    view.pending_persist.is_some()
                });
                // Only an account added through the form is saved (and
                // announced to the listener, and dismisses the Add account
                // form). One connecting in the background -- a saved one at
                // startup, or a reconnect -- must not.
                if from_form {
                    self.changes.persist_accounts.push(account.clone());
                }
                self.status = format!("Connected: {}", self.account_label(&account));
                self.send_imap_to(&account, ImapCommand::FetchMailboxes);
                if self.active.is_none() {
                    self.activate(&account, "INBOX".to_string());
                } else if is_active {
                    self.fetch_headers(self.selected_mailbox.clone(), 1);
                }
            }
            ImapEvent::Disconnected => {
                if let Some(view) = self.view_mut(&account) {
                    view.state = ConnState::Disconnected;
                }
                if is_active {
                    self.status = "Connection lost, reconnecting...".to_string();
                }
            }
            ImapEvent::Error(e) => {
                // A failed `BulkDownload` reports through this generic
                // variant (it has no per-uid/req_id to attribute to), so
                // clear its bar rather than leaving it stuck at whatever
                // it last showed.
                self.clear_progress(ProgressKind::Index);
                if let Some(view) = self.view_mut(&account) {
                    // A first connect that failed, or a reconnect that gave
                    // up (a revoked Google sign-in, say): either way the
                    // account is not going to recover by itself.
                    if matches!(view.state, ConnState::Connecting | ConnState::Disconnected) {
                        view.state = ConnState::Failed(e.clone());
                    }
                }
                self.push_account_banner(&account, format!("IMAP error: {e}"));
            }
            ImapEvent::Mailboxes(mbs) => {
                // B8: render as a tree (name split on the server's
                // delimiter, special-use folders first) instead of a
                // flat alphabetical list.
                let names: Vec<String> = mbs.iter().map(|m| m.name.clone()).collect();
                if let Some(view) = self.view_mut(&account) {
                    view.mailbox_rows = imap::flatten_tree(&imap::mailbox_tree(&mbs));
                }
                self.send_imap_to(&account, ImapCommand::FetchUnreadCounts { mailboxes: names });
            }
            ImapEvent::Headers { mailbox, headers, page, total_pages, req_id, mailbox_state } => {
                // Only the most recently issued FetchHeaders' reply is
                // applied; an older one arriving late (e.g. the mailbox
                // was changed again before it came back) is dropped.
                if is_active && req_id == self.current_headers_req && mailbox == self.selected_mailbox {
                    self.headers = headers;
                    self.current_page = page;
                    self.total_pages = total_pages;
                    self.status = format!("Page {} of {}", page, total_pages);
                }
                // Rides along on every header fetch regardless of
                // req_id/mailbox staleness — db.rs's cache bookkeeping for
                // `mailbox` should stay current even if this particular
                // reply is no longer the one the UI is showing.
                let _ = self.db_tx.try_send(DbCommand::ReportMailboxState {
                    account_id: account,
                    mailbox,
                    uid_validity: mailbox_state.uid_validity,
                    uid_next: mailbox_state.uid_next,
                });
            }
            ImapEvent::Body { uid, html, attachments, req_id } => {
                // Matching `req_id`+`uid` is sufficient on its own now
                // that `open_message` bumps `current_body_req` on every
                // open (including a cache-served search result) -- a
                // stale live reply from before a search-result open can
                // no longer slip through just because `uid` happens to
                // coincide, since its `req_id` is guaranteed stale too.
                // (This used to also require `self.search_results.is_none()`,
                // which incidentally also blocked the *legitimate* case
                // fixed here: a live fallback fetch issued while the
                // search results list is still showing, from a DB cache
                // miss -- see the `DbEvent::MailFetchFailed` arm below.)
                if is_active && req_id == self.current_body_req && self.selected_uid == Some(uid) {
                    self.current_message_html = html.clone();
                    self.show_in_reading_pane(html);
                    self.current_attachments = attachments;
                }
            }
            ImapEvent::BodyFailed { uid, req_id, error } => {
                // Without this, a failed body fetch (dropped connection,
                // exhausted reconnect retries, message no longer on the
                // server) left `open_message`'s "Loading message..."
                // placeholder on screen forever: the generic `Error`
                // variant this used to arrive as carries no uid/req_id,
                // so nothing could tell it apart from an unrelated error
                // and resolve the pending fetch. This is the actual fix
                // for the "stuck on Loading message..." bug (issue #13).
                if is_active && req_id == self.current_body_req && self.selected_uid == Some(uid) {
                    let msg = format!("<i>Could not load message: {}</i>", ammonia::clean_text(&error));
                    self.current_message_html = msg.clone();
                    self.show_in_reading_pane(msg);
                }
                self.push_account_banner(&account, format!("Could not load message: {error}"));
            }
            ImapEvent::Exported { path } => {
                self.clear_progress(ProgressKind::Export);
                self.status = format!("Exported message to {}", path.display());
            }
            ImapEvent::ExportFailed { error } => {
                self.clear_progress(ProgressKind::Export);
                self.push_account_banner(&account, format!("Could not export message: {error}"));
            }
            ImapEvent::Progress { kind, progress: update } => {
                // Indexing is the active account's selected mailbox, so a
                // report for another account's is ignored; an append or an
                // export belongs to whichever account it was issued for,
                // so those show regardless.
                let indexing_done = kind == ProgressKind::Index
                    && matches!(update, Progress::Counted { current, total } if current == total);
                if kind != ProgressKind::Index || is_active {
                    if indexing_done {
                        self.clear_progress(ProgressKind::Index);
                        self.status = "Download complete".to_string();
                    } else {
                        self.set_progress(kind, update);
                    }
                }
            }
            ImapEvent::MailData { mailbox, header, body, attachments } => {
                let _ = self.db_tx.try_send(DbCommand::IndexMail {
                    account_id: account,
                    mailbox,
                    header,
                    body,
                    attachments,
                });
            }
            ImapEvent::MailboxPolled { .. } => {
                // B10's new-mail signal: already consumed by the
                // account's forwarder (session.rs) before this event
                // reached the UI channel at all (it decides whether to
                // poll again / fetch new envelopes / show a toast).
                // Nothing left here for the UI to do.
            }
            ImapEvent::NewHeaders { .. } => {
                // Same: the forwarder already turned it into a toast.
                // What the UI still owes is the unread counts, so an
                // account that is not on screen (with several accounts
                // most are not) shows its new mail in the folder pane.
                let names = self.mailbox_names(&account);
                if !names.is_empty() {
                    self.send_imap_to(&account, ImapCommand::FetchUnreadCounts { mailboxes: names });
                }
            }
            ImapEvent::Appended { mailbox } => {
                // B7: confirmation that the just-sent message was saved
                // to `mailbox` (see the `Append` sent from
                // `handle_smtp_events`'s `Sent` arm). Nothing else for the
                // UI to update -- the compose window and "Message sent"
                // status already reflect the send itself, which
                // succeeded independently of this.
                self.clear_progress(ProgressKind::Append);
                log::debug!("appended sent message to {mailbox}");
            }
            ImapEvent::AppendFailed { mailbox, error } => {
                // Deliberately not `self.status` -- see the variant's
                // doc in imap.rs: the send itself already succeeded and
                // is already reflected there, and this is a background,
                // best-effort step the user never explicitly asked to
                // watch. A banner (B9), unlike the old single `status`
                // string this replaces, can say so *alongside* "Message
                // sent" instead of only being able to overwrite it --
                // which is exactly the problem that made this event a
                // log-only affair up to now.
                self.clear_progress(ProgressKind::Append);
                log::warn!("could not save sent message to {mailbox}: {error}");
                self.push_account_banner(&account, format!("Sent, but could not save a copy to {mailbox}: {error}"));
            }
            ImapEvent::HeadersFrom { mailbox, headers } => {
                // B3: reply to the `FetchHeadersFrom` sent in
                // `handle_db_events`'s `SyncPlan::FetchFrom`/`Resync`
                // arm -- index into the cache now that these envelopes
                // are in hand. See `ImapCommand::FetchHeadersFrom`'s doc
                // for why this is a separate event from `NewHeaders`
                // rather than reusing it.
                let _ = self.db_tx.try_send(DbCommand::IndexHeaders {
                    account_id: account,
                    mailbox,
                    headers,
                });
            }
            ImapEvent::PollFailed(e) => {
                // Deliberately not `self.status` -- see the variant's
                // doc in imap.rs: a background poll failing every 60s
                // shouldn't overwrite whatever the user is looking at.
                log::warn!("background new-mail poll for {account} failed: {e}");
            }
            ImapEvent::FlagsUpdated { mailbox, uid, flags, req_id: _ } => {
                self.advance_bulk_action(ProgressKind::Flags, uid);
                // B8: reflect the server-confirmed flags back into the
                // visible header list, any active search-results list,
                // and the local cache. Only touches self.headers/
                // self.search_results when `mailbox` of the active
                // account is what's actually on screen -- both lists
                // only ever hold messages from `self.selected_mailbox`
                // (search is itself scoped to it, see the `Search`
                // send-site below), so an event for a different mailbox
                // (or account) finding a same-numbered UID in either
                // list would otherwise patch the wrong message's row.
                // The unread count belongs to the event's own account
                // and mailbox, and the DB write is keyed by both, so
                // both are correct regardless of what's displayed.
                let mut was_seen = None;
                if is_active && mailbox == self.selected_mailbox {
                    was_seen = self.headers.iter().find(|h| h.uid == uid).map(|h| h.is_seen());
                    if let Some(header) = self.headers.iter_mut().find(|h| h.uid == uid) {
                        header.flags = flags.clone();
                    }
                }
                // Search results can come from any account and mailbox, so
                // they are matched on their own origin rather than on what
                // is open.
                if let Some(results) = self.search_results.as_mut() {
                    for (i, header) in results.iter_mut().enumerate() {
                        let origin = self.search_origins.get(i);
                        if header.uid == uid && origin.is_some_and(|(a, m)| *a == account && *m == mailbox) {
                            header.flags = flags.clone();
                        }
                    }
                }
                if let Some(was_seen) = was_seen {
                    if let Some(count) = self.view_mut(&account).and_then(|v| v.unread_counts.get_mut(&mailbox)) {
                        let now_seen = flags.iter().any(|f| f.eq_ignore_ascii_case(imap::FLAG_SEEN));
                        if was_seen && !now_seen {
                            *count += 1;
                        } else if !was_seen && now_seen {
                            *count = count.saturating_sub(1);
                        }
                    }
                }
                let _ = self.db_tx.try_send(DbCommand::UpdateFlags {
                    account_id: account,
                    mailbox,
                    uid,
                    flags,
                });
            }
            ImapEvent::FlagsUpdateFailed { mailbox: _, uid, error, req_id: _ } => {
                self.advance_bulk_action(ProgressKind::Flags, uid);
                self.push_account_banner(&account, format!("Could not update flags on message {uid}: {error}"));
            }
            ImapEvent::Moved { mailbox, uid, dest, req_id: _ } => {
                self.advance_bulk_action(ProgressKind::Move, uid);
                // B8: delete-to-Trash/archive succeeded -- drop the
                // message from the visible list, any active
                // search-results list, the cache, and any selection it
                // was part of. See FlagsUpdated above for why the
                // header/search-results mutations are guarded on this
                // being the active account's selected mailbox.
                let on_screen = is_active && mailbox == self.selected_mailbox;
                let was_unread = on_screen
                    && self.headers.iter().find(|h| h.uid == uid).map(|h| !h.is_seen()).unwrap_or(false);
                if on_screen {
                    self.headers.retain(|h| h.uid != uid);
                }
                if let Some(results) = self.search_results.as_mut() {
                    let origins = std::mem::take(&mut self.search_origins);
                    let (kept, kept_origins): (Vec<_>, Vec<_>) = results
                        .drain(..)
                        .zip(origins)
                        .filter(|(h, (a, m))| !(h.uid == uid && *a == account && *m == mailbox))
                        .unzip();
                    *results = kept;
                    self.search_origins = kept_origins;
                }
                if is_active {
                    self.selected_uids.remove(&uid);
                    if self.selected_uid == Some(uid) {
                        self.selected_uid = None;
                        self.show_in_reading_pane("<i>Message moved.</i>".to_string());
                    }
                    self.status = format!("Moved to {dest}");
                }
                if was_unread {
                    if let Some(view) = self.view_mut(&account) {
                        if let Some(count) = view.unread_counts.get_mut(&mailbox) {
                            *count = count.saturating_sub(1);
                        }
                        // The message just landed in `dest` unread --
                        // bump its count too if we're already tracking
                        // it (it may not be yet if FetchUnreadCounts
                        // hasn't completed), so the sidebar doesn't
                        // read "no new mail in Archive/Trash" for a
                        // message that just arrived there.
                        if let Some(count) = view.unread_counts.get_mut(&dest) {
                            *count += 1;
                        }
                    }
                }
                let _ = self.db_tx.try_send(DbCommand::RemoveMessage {
                    account_id: account,
                    mailbox,
                    uid,
                });
            }
            ImapEvent::MoveFailed { mailbox: _, uid, error, req_id: _ } => {
                self.advance_bulk_action(ProgressKind::Move, uid);
                self.push_account_banner(&account, format!("Could not move message {uid}: {error}"));
            }
            ImapEvent::UnreadCounts(counts) => {
                if let Some(view) = self.view_mut(&account) {
                    view.unread_counts = counts;
                }
            }
        }
    }
}
