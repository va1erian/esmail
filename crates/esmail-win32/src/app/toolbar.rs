//! The window's top bar: the main toolbar under the menu.
//!
//! Each button has a typed id, so [`Toolbar::set_enabled`] and
//! [`Toolbar::set_checked`] keep its state right without rebuilding anything:
//! buttons that do not apply dim (and emit nothing when clicked), and the star
//! button's checked state follows the open message's flag. The reader's own bar
//! (`reader_bar.rs`) uses native `Button`s, which also grey out in place.

use win32ui::prelude::*;

use esmail_win32::core_glue::compose::Kind;

use super::Msg;

/// The typed ids of the toolbar's buttons, so `set_state` can address them.
mod id {
    pub const NEW: u64 = 1;
    pub const REFRESH: u64 = 2;
    pub const REPLY: u64 = 3;
    pub const REPLY_ALL: u64 = 4;
    pub const FORWARD: u64 = 5;
    pub const MARK_READ: u64 = 6;
    pub const MARK_UNREAD: u64 = 7;
    pub const STAR: u64 = 8;
    pub const ARCHIVE: u64 = 9;
    pub const DELETE: u64 = 10;
    pub const THEME: u64 = 11;
    pub const SETTINGS: u64 = 12;
}

/// The selection/action state the main toolbar's buttons derive their
/// availability from. Pure, so it is unit-tested without a window.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ActionState {
    /// At least one message is selected in the list.
    pub selected: bool,
    /// A flag or move command is still on its way to the server.
    pub busy: bool,
    /// The open message is flagged (`\Flagged`).
    pub flagged: bool,
    /// A message is open in the reading pane.
    pub message: bool,
}

impl ActionState {
    /// Whether a bulk action on the selection (mark read/unread, star, archive,
    /// delete) can run: something is selected and no action is already running.
    pub fn bulk_enabled(self) -> bool {
        self.selected && !self.busy
    }

    /// Whether an action on the open message (reply, forward, export) can run:
    /// a message is open and no action is already running.
    pub fn message_enabled(self) -> bool {
        self.message && !self.busy
    }

    /// The star button's label: it follows the open message's flag.
    pub fn star_label(self) -> &'static str {
        if self.flagged { "Unstar" } else { "Star" }
    }
}

/// The main toolbar.
pub struct MainBar {
    toolbar: Toolbar<Msg>,
}

impl MainBar {
    /// Builds the toolbar once. Button availability is set from the shared
    /// state in [`MainBar::set_state`], never by rebuilding.
    pub fn new(ui: &mut Ui<Msg>) -> win32ui::Result<MainBar> {
        let toolbar = Toolbar::new(
            ui,
            vec![
                ToolbarItem::new("New message")
                    .id(id::NEW)
                    .with_icon(ToolbarIcon::Compose)
                    .shortcut(Shortcut::ctrl(Key::N))
                    .tooltip("Write a new message")
                    .on_click(|| Some(Msg::Compose(Kind::New))),
                ToolbarItem::new("Refresh")
                    .id(id::REFRESH)
                    .with_icon(ToolbarIcon::Refresh)
                    .shortcut(Shortcut::key(Key::F5))
                    .tooltip("Fetch this folder again")
                    .on_click(|| Some(Msg::Refresh)),
                ToolbarItem::separator(),
                ToolbarItem::new("Reply")
                    .id(id::REPLY)
                    .with_icon(ToolbarIcon::Reply)
                    .shortcut(Shortcut::ctrl(Key::R))
                    .tooltip("Reply to the selected message")
                    .enabled(false)
                    .on_click(|| Some(Msg::Compose(Kind::Reply))),
                ToolbarItem::new("Reply all")
                    .id(id::REPLY_ALL)
                    .with_icon(ToolbarIcon::Reply)
                    .shortcut(Shortcut::ctrl(Key::R).with_shift())
                    .tooltip("Reply to everyone")
                    .enabled(false)
                    .on_click(|| Some(Msg::Compose(Kind::ReplyAll))),
                ToolbarItem::new("Forward")
                    .id(id::FORWARD)
                    .with_icon(ToolbarIcon::Forward)
                    .shortcut(Shortcut::ctrl(Key::L))
                    .tooltip("Forward the selected message")
                    .enabled(false)
                    .on_click(|| Some(Msg::Compose(Kind::Forward))),
                ToolbarItem::separator(),
                ToolbarItem::new("Mark read")
                    .id(id::MARK_READ)
                    .with_icon(ToolbarIcon::MarkRead)
                    .tooltip("Mark the selection as read")
                    .enabled(false)
                    .on_click(|| Some(Msg::SetSeen(true))),
                ToolbarItem::new("Mark unread")
                    .id(id::MARK_UNREAD)
                    .with_icon(ToolbarIcon::MarkUnread)
                    .tooltip("Mark the selection as unread")
                    .enabled(false)
                    .on_click(|| Some(Msg::SetSeen(false))),
                ToolbarItem::separator(),
                ToolbarItem::new("Star")
                    .id(id::STAR)
                    .with_icon(ToolbarIcon::Star)
                    .tooltip("Flag or unflag the selection")
                    .toggle()
                    .enabled(false)
                    .on_toggle(|_| Some(Msg::ToggleFlag)),
                ToolbarItem::new("Archive")
                    .id(id::ARCHIVE)
                    .with_icon(ToolbarIcon::Archive)
                    .tooltip("Move the selection to the archive folder")
                    .enabled(false)
                    .on_click(|| Some(Msg::Archive)),
                ToolbarItem::new("Delete")
                    .id(id::DELETE)
                    .with_icon(ToolbarIcon::Delete)
                    .tooltip("Move the selection to the trash folder")
                    .enabled(false)
                    .on_click(|| Some(Msg::Delete)),
                ToolbarItem::flexible_spacer(),
                ToolbarItem::new("Theme")
                    .id(id::THEME)
                    .with_icon(ToolbarIcon::glyph('\u{E706}'))
                    .tooltip("Cycle Dark / Light / System (View > Theme)")
                    .on_click(|| Some(Msg::CycleTheme)),
                ToolbarItem::new("Settings")
                    .id(id::SETTINGS)
                    .with_icon(ToolbarIcon::Settings)
                    .tooltip("Manage accounts (File > Accounts...)")
                    .on_click(|| Some(Msg::ManageAccounts)),
            ],
        )?;
        Ok(MainBar { toolbar })
    }

    /// Updates which buttons apply and whether the star is on.
    pub fn set_state(&self, state: ActionState) {
        let actions = state.bulk_enabled();
        for id in [id::REPLY, id::REPLY_ALL, id::FORWARD, id::MARK_READ, id::MARK_UNREAD, id::STAR, id::ARCHIVE, id::DELETE] {
            self.toolbar.set_enabled(id, actions);
        }
        self.toolbar.set_checked(id::STAR, state.flagged);
    }

    /// The toolbar row, sized to the toolbar's own height so the parent column
    /// gives it only the strip it needs.
    pub fn bar_layout(&self, dpi: u32) -> LayoutItem {
        let row_height = Px(self.toolbar.height()).to_dip(dpi) + dip(6.0);
        Layout::row()
            .margins(Insets::symmetric(dip(4.0), dip(3.0)))
            .item(self.toolbar.fill(1))
            .height(row_height)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bulk_actions_need_a_selection_and_no_running_action() {
        let state = ActionState { selected: false, ..ActionState::default() };
        assert!(!state.bulk_enabled(), "nothing is selected");
        assert!(!ActionState { selected: true, busy: true, ..state }.bulk_enabled(), "an action is running");
        assert!(ActionState { selected: true, busy: false, ..state }.bulk_enabled());
    }

    #[test]
    fn message_actions_need_an_open_message_and_no_running_action() {
        assert!(!ActionState::default().message_enabled());
        assert!(ActionState { message: true, ..ActionState::default() }.message_enabled());
        assert!(!ActionState { message: true, busy: true, ..ActionState::default() }.message_enabled());
    }

    #[test]
    fn the_star_label_follows_the_message_flag() {
        assert_eq!(ActionState::default().star_label(), "Star");
        assert_eq!(ActionState { flagged: true, ..ActionState::default() }.star_label(), "Unstar");
    }
}
