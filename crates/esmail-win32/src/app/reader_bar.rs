//! The action bar above the reading pane: the actions that apply to the open
//! message, and the remote-images banner.
//!
//! Unlike the main toolbar (`toolbar.rs`), these are native [`Button`]s so they
//! grey out and re-label in place: `update` sets each one's enabled state and
//! text from a [`ReaderState`], and only re-runs the layout when the
//! remote-images banner changes shape.

use std::cell::RefCell;

use xui_win32::prelude::*;

use esmail_win32::core_glue::Notice;
use esmail_win32::core_glue::compose::Kind;

use super::Msg;
use super::toolbar::ActionState;

/// Whether remote content is blocked for the message on screen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemoteState {
    /// No message is open: the banner is hidden.
    Absent,
    /// Blocked; `sender` names who "Always load from..." would trust, unless
    /// the message has no address to trust.
    Blocked { sender: String, trustable: bool },
    /// Loading because View > Load remote images is on.
    Allowed,
    /// Loading because the sender is on the always-load list.
    Trusted,
}

/// Everything the reading pane's bar shows: the action state the buttons derive
/// their availability from, and the remote-images state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReaderState {
    pub actions: ActionState,
    pub remote: RemoteState,
    /// An account that needs signing in again: shown in the banner's place.
    pub notice: Option<Notice>,
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
    pub fn new(ui: &mut Ui<Msg>) -> xui_win32::Result<ReaderBar> {
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
        let always = Button::new(ui, "Always load from...")?.on_click(|| Some(Msg::BannerAction));
        let stop = Button::new(ui, "Stop")?.on_click(|| Some(Msg::StopRemoteImages));

        for button in [&load, &always, &stop] {
            button.set_visible(false);
        }

        Ok(ReaderBar { reply, reply_all, forward, star, unread, archive, delete, export, banner, load, always, stop, last: RefCell::new(None) })
    }

    /// Applies `state` to the buttons: enabled, label and the remote-images
    /// banner's shape. Re-runs the layout only when the banner's shape changed.
    pub fn update(&self, ui: &Ui<Msg>, state: ReaderState) {
        let previous = self.last.replace(Some(state.clone()));
        if previous.as_ref() == Some(&state) {
            return;
        }
        let remote_changed = previous.as_ref().is_none_or(|old| old.remote != state.remote || old.notice != state.notice);
        let on = state.actions.message_enabled();
        for button in [&self.reply, &self.reply_all, &self.forward, &self.star, &self.unread, &self.archive, &self.delete, &self.export] {
            button.set_enabled(on);
        }
        self.star.set_text(state.actions.star_label());
        if let Some(notice) = &state.notice {
            let more = if notice.others > 0 { format!(" (and {} more account(s))", notice.others) } else { String::new() };
            self.banner.set_visible(true);
            self.banner.set_text(&format!("{}{more}", notice.text));
            self.always.set_visible(true);
            self.always.set_enabled(true);
            self.always.set_text(notice.action);
            self.always.set_tooltip("Open the account's sign-in form");
            self.load.set_visible(false);
            self.stop.set_visible(false);
            if remote_changed {
                ui.relayout();
            }
            return;
        }
        match &state.remote {
            RemoteState::Absent => {
                self.banner.set_visible(false);
                self.load.set_visible(false);
                self.always.set_visible(false);
                self.stop.set_visible(false);
            }
            RemoteState::Blocked { sender, trustable } => {
                self.banner.set_visible(true);
                self.banner.set_text("Remote images are blocked.");
                self.load.set_visible(true);
                self.always.set_visible(true);
                self.stop.set_visible(false);
                self.always.set_enabled(*trustable);
                self.always.set_tooltip(if *trustable { "Load images from this sender's messages without asking" } else { "This message has no sender address to trust" });
                self.always.set_text(&format!("Always load from {sender}"));
            }
            RemoteState::Allowed => {
                self.banner.set_visible(true);
                self.banner.set_text("Remote images load.");
                self.load.set_visible(false);
                self.always.set_visible(false);
                self.stop.set_visible(true);
                self.stop.set_text("Stop");
                self.stop.set_tooltip("Turn off View > Load remote images");
            }
            RemoteState::Trusted => {
                self.banner.set_visible(true);
                self.banner.set_text("Remote images load: you trust this sender.");
                self.load.set_visible(false);
                self.always.set_visible(false);
                self.stop.set_visible(true);
                self.stop.set_text("Untrust");
                self.stop.set_tooltip("Stop loading this sender's images automatically");
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
            .item(self.always.width(dip(210.0)))
            .item(self.stop.width(dip(68.0)))
            .height(dip(48.0))
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
