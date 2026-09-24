//! The state the two bars derive their buttons from, kept out of `mod.rs`.
//!
//! Both bars show the same selection/action state (`toolbar.rs`) plus, for the
//! reader bar, whether remote images are blocked for the message on screen. It
//! is derived from the window's own fields rather than cached, so there is one
//! source of truth; `sync_bars` runs after every user-driven message and is a
//! no-op when nothing changed.

use esmail::imap::MailHeader;
use win32ui::prelude::*;

use esmail::oauth::now_unix;
use esmail_win32::core_glue::{Notice, account_notice, button_name};

use super::reader_bar::{ReaderState, RemoteState};
use super::toolbar::ActionState;
use super::{App, Msg};

impl App {
    /// What is selected, what is open, and whether a command is still in flight.
    pub(super) fn action_state(&self) -> ActionState {
        ActionState {
            selected: !self.list.selection().is_empty(),
            busy: self.pending_actions > 0,
            flagged: self.selected.as_ref().is_some_and(MailHeader::is_flagged),
            message: self.reader.current_message().is_some(),
        }
    }

    /// Whether remote images are blocked for the message on screen, loading, or
    /// there is no message at all.
    pub(super) fn remote_state(&self) -> RemoteState {
        let Some((header, _)) = self.reader.current_message() else { return RemoteState::Absent };
        if self.current_sender_trusted() {
            RemoteState::Trusted
        } else if self.remote_images {
            RemoteState::Allowed
        } else {
            RemoteState::Blocked { sender: button_name(&header.sender_name()), trustable: header.sender_address().is_some() }
        }
    }

    /// The account that needs signing in again, if any.
    pub(super) fn attention(&self) -> Option<Notice> {
        account_notice(&self.config.accounts, &self.accounts.failures(), now_unix())
    }

    /// Pushes the derived state into both bars.
    pub(super) fn sync_bars(&self, ui: &Ui<Msg>) {
        let actions = self.action_state();
        self.toolbar.set_state(actions);
        self.reader_bar.update(ui, ReaderState { actions, remote: self.remote_state(), notice: self.attention() });
    }
}
