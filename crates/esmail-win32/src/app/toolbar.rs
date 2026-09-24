//! The window's top bar: the main toolbar under the menu, plus the theme-cycle
//! and Settings buttons at its right end.
//!
//! win32ui's [`Toolbar`] maps a click to an app message but has no per-button
//! enabled flag, so a button that does not apply — nothing selected, or an
//! action already on its way to the server — emits nothing: its `on_click`
//! closure reads the shared [`ActionState`]. The reading pane's bar
//! (`reader_bar.rs`) uses native `Button`s, which do grey out, for the actions
//! that need a visible disabled state.

use std::cell::Cell;
use std::rc::Rc;

use win32ui::prelude::*;

use esmail_win32::core_glue::compose::Kind;

use super::Msg;
use super::args::ThemeChoice;

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

/// The main toolbar and the two buttons at its right end.
pub struct MainBar {
    toolbar: Toolbar<Msg>,
    theme: Button<Msg>,
    settings: Button<Msg>,
    state: Rc<Cell<ActionState>>,
    /// The theme button's last label, so a message that does not change the
    /// theme does not re-set the button text.
    theme_label: Cell<Option<ThemeChoice>>,
}

impl MainBar {
    /// Builds the toolbar. All buttons are created once; their availability is
    /// read from the shared state at click time, so nothing here is rebuilt.
    pub fn new(ui: &mut Ui<Msg>, theme: ThemeChoice) -> win32ui::Result<MainBar> {
        let state = Rc::new(Cell::new(ActionState::default()));
        // A bulk button builds its message afresh on each click, so the closure
        // stays `Fn` and can grey itself out by returning `None` when no action
        // applies.
        let bulk = |msg: fn() -> Msg| {
            let state = Rc::clone(&state);
            move || state.get().bulk_enabled().then(|| msg())
        };

        let toolbar = Toolbar::new(
            ui,
            vec![
                ToolbarItem::new("New message")
                    .with_icon(ToolbarIcon::Circle)
                    .shortcut(Shortcut::ctrl(Key::N))
                    .tooltip("Write a new message")
                    .on_click(|| Some(Msg::Compose(Kind::New))),
                ToolbarItem::new("Refresh")
                    .with_icon(ToolbarIcon::Arrow)
                    .shortcut(Shortcut::key(Key::F5))
                    .tooltip("Fetch this folder again")
                    .on_click(|| Some(Msg::Refresh)),
                ToolbarItem::new("Reply")
                    .with_icon(ToolbarIcon::Arrow)
                    .shortcut(Shortcut::ctrl(Key::R))
                    .tooltip("Reply to the selected message")
                    .on_click(bulk(|| Msg::Compose(Kind::Reply))),
                ToolbarItem::new("Reply all")
                    .with_icon(ToolbarIcon::Arrow)
                    .shortcut(Shortcut::ctrl(Key::R).with_shift())
                    .tooltip("Reply to everyone")
                    .on_click(bulk(|| Msg::Compose(Kind::ReplyAll))),
                ToolbarItem::new("Forward")
                    .with_icon(ToolbarIcon::Arrow)
                    .shortcut(Shortcut::ctrl(Key::L))
                    .tooltip("Forward the selected message")
                    .on_click(bulk(|| Msg::Compose(Kind::Forward))),
                ToolbarItem::new("Mark read")
                    .with_icon(ToolbarIcon::Check)
                    .tooltip("Mark the selection as read")
                    .on_click(bulk(|| Msg::SetSeen(true))),
                ToolbarItem::new("Mark unread")
                    .with_icon(ToolbarIcon::Circle)
                    .tooltip("Mark the selection as unread")
                    .on_click(bulk(|| Msg::SetSeen(false))),
                ToolbarItem::new("Star")
                    .with_icon(ToolbarIcon::Check)
                    .tooltip("Flag or unflag the selection")
                    .on_click(bulk(|| Msg::ToggleFlag)),
                ToolbarItem::new("Archive")
                    .with_icon(ToolbarIcon::Arrow)
                    .tooltip("Move the selection to the archive folder")
                    .on_click(bulk(|| Msg::Archive)),
                ToolbarItem::new("Delete")
                    .with_icon(ToolbarIcon::Close)
                    .tooltip("Move the selection to the trash folder")
                    .on_click(bulk(|| Msg::Delete)),
            ],
        )?;

        let theme_button = Button::new(ui, "Theme")?
            .on_click(|| Some(Msg::CycleTheme));
        let theme_label = Cell::new(None);
        theme_button.set_tooltip("Cycle Dark / Light / System");
        let settings = Button::new(ui, "Settings")?
            .on_click(|| Some(Msg::ManageAccounts));
        settings.set_tooltip("Manage accounts (File > Accounts...)");

        let bar = MainBar { toolbar, theme: theme_button, settings, state, theme_label };
        bar.set_theme_label(theme);
        Ok(bar)
    }

    /// Updates the availability the toolbar buttons read.
    pub fn set_state(&self, state: ActionState) {
        self.state.set(state);
    }

    /// Labels the theme button with the current choice, once per change.
    pub fn set_theme_label(&self, theme: ThemeChoice) {
        if self.theme_label.get() == Some(theme) {
            return;
        }
        self.theme_label.set(Some(theme));
        self.theme.set_text(&format!("Theme: {}", theme.label()));
    }

    /// The toolbar row, sized to the toolbar's own height so the parent column
    /// gives it only the strip it needs. The theme/settings widths are fixed:
    /// the theme button's label changes, and an auto-width button would clip it.
    pub fn bar_layout(&self, dpi: u32) -> LayoutItem {
        let row_height = Px(self.toolbar.height()).to_dip(dpi) + dip(6.0);
        Layout::row()
            .spacing(dip(4.0))
            .margins(Insets::symmetric(dip(4.0), dip(3.0)))
            .item(self.toolbar.fill(1))
            .item(self.theme.width(dip(104.0)))
            .item(self.settings.width(dip(76.0)))
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
