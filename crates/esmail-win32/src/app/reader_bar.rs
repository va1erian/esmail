//! The action bar above the reading pane: the actions that apply to the open
//! message, and the remote-images banner.
//!
//! Unlike the main toolbar (`toolbar.rs`), these are native [`Button`]s so they
//! grey out and re-label in place: `update` sets each one's enabled state and
//! text from a [`ReaderState`], and only re-runs the layout when the
//! remote-images banner changes shape.

use std::cell::RefCell;

use win32ui::prelude::*;

use esmail_win32::core_glue::compose::Kind;

use super::Msg;
use super::toolbar::ActionState;

/// Whether remote content is blocked for the message on screen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemoteState {
    /// No message is open: the banner is hidden.
    Absent,
    /// Blocked; the sender names who "Always load from..." would trust.
    Blocked { sender: Option<String> },
    /// Loading for this message.
    Allowed,
}

/// Everything the reading pane's bar shows: the action state the buttons derive
/// their availability from, and the remote-images state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReaderState {
    pub actions: ActionState,
    pub remote: RemoteState,
}

/// The reading pane's action bar and remote-images banner.
pub struct ReaderBar {
    reply: Button<Msg>,
    reply_all: Button<Msg>,
    forward: Button<Msg>,
    star: Button<Msg>,
    unread: Button<Msg>,
    archive: Button<Msg>,
    delete: Button<Msg>,
    export: Button<Msg>,
    banner: Label,
    load: Button<Msg>,
    always: Button<Msg>,
    stop: Button<Msg>,
    /// The state applied last, so an unchanged message costs nothing.
    last: RefCell<Option<ReaderState>>,
}

impl ReaderBar {
    pub fn new(ui: &mut Ui<Msg>) -> win32ui::Result<ReaderBar> {
        let reply = Button::new(ui, "Reply")?.on_click(|| Some(Msg::Compose(Kind::Reply)));
        let reply_all = Button::new(ui, "Reply All")?.on_click(|| Some(Msg::Compose(Kind::ReplyAll)));
        let forward = Button::new(ui, "Forward")?.on_click(|| Some(Msg::Compose(Kind::Forward)));
        let star = Button::new(ui, "Star")?.on_click(|| Some(Msg::ToggleFlag));
        let unread = Button::new(ui, "Mark unread")?.on_click(|| Some(Msg::SetSeen(false)));
        let archive = Button::new(ui, "Archive")?.on_click(|| Some(Msg::Archive));
        let delete = Button::new(ui, "Delete")?.on_click(|| Some(Msg::Delete));
        let export = Button::new(ui, "Export...")?.on_click(|| Some(Msg::Export));

        reply.set_tooltip("Reply to this message (Ctrl+R)");
        reply_all.set_tooltip("Reply to everyone (Ctrl+Shift+R)");
        forward.set_tooltip("Forward this message (Ctrl+L)");
        star.set_tooltip("Flag or unflag this message");
        unread.set_tooltip("Mark this message unread");
        archive.set_tooltip("Move this message to the archive folder");
        delete.set_tooltip("Move this message to the trash folder");
        export.set_tooltip("Save this message's raw source as an .eml file");

        let banner = Label::new(ui, Rect::new(0, 0, 0, 0), "")?;
        let load = Button::new(ui, "Load remote images")?.on_click(|| Some(Msg::RemoteImages(true)));
        let always = Button::new(ui, "Always load from...")?;
        always.set_enabled(false);
        always.set_tooltip("Trusting a sender is not wired up in the win32 build yet; use View > Load remote images per message");
        let stop = Button::new(ui, "Stop")?.on_click(|| Some(Msg::RemoteImages(false)));

        Ok(ReaderBar { reply, reply_all, forward, star, unread, archive, delete, export, banner, load, always, stop, last: RefCell::new(None) })
    }

    /// Applies `state` to the buttons: enabled, label and the remote-images
    /// banner's shape. Re-runs the layout only when the banner's shape changed.
    pub fn update(&self, ui: &Ui<Msg>, state: ReaderState) {
        let previous = self.last.replace(Some(state.clone()));
        if previous.as_ref() == Some(&state) {
            return;
        }
        let remote_changed = previous.as_ref().is_none_or(|old| old.remote != state.remote);
        let on = state.actions.message_enabled();
        for button in [&self.reply, &self.reply_all, &self.forward, &self.star, &self.unread, &self.archive, &self.delete, &self.export] {
            button.set_enabled(on);
        }
        self.star.set_text(state.actions.star_label());
        match &state.remote {
            RemoteState::Absent => {
                self.banner.set_visible(false);
                self.load.set_visible(false);
                self.always.set_visible(false);
                self.stop.set_visible(false);
            }
            RemoteState::Blocked { sender } => {
                self.banner.set_visible(true);
                self.banner.set_text("Remote images are blocked.");
                self.load.set_visible(true);
                self.always.set_visible(true);
                self.stop.set_visible(false);
                let who = sender.as_deref().unwrap_or("this sender");
                self.always.set_text(&format!("Always load from {who}"));
            }
            RemoteState::Allowed => {
                self.banner.set_visible(true);
                self.banner.set_text("Remote images load.");
                self.load.set_visible(false);
                self.always.set_visible(false);
                self.stop.set_visible(true);
            }
        }
        if remote_changed {
            ui.relayout();
        }
    }

    /// The remote-images banner row: a slim strip above the reading pane, hidden
    /// entirely when no message is open. The two action buttons keep a fixed
    /// width: their labels name the sender and would otherwise clip.
    pub fn banner_layout(&self) -> LayoutItem {
        Layout::row()
            .margins(Insets::symmetric(dip(6.0), dip(2.0)))
            .spacing(dip(4.0))
            .item(self.banner.fill(1))
            .item(self.load.width(dip(130.0)))
            .item(self.always.width(dip(190.0)))
            .item(self.stop.width(dip(60.0)))
            .height(dip(40.0))
    }

    /// The action-buttons row.
    pub fn actions_layout(&self) -> LayoutItem {
        Layout::row()
            .spacing(dip(4.0))
            .margins(Insets::symmetric(dip(6.0), dip(3.0)))
            .item(self.reply.layout_item())
            .item(self.reply_all.layout_item())
            .item(self.forward.layout_item())
            .item(self.star.layout_item())
            .item(self.unread.layout_item())
            .item(self.archive.layout_item())
            .item(self.delete.layout_item())
            .item(self.export.layout_item())
            .height(dip(36.0))
    }
}
