//! Window furniture: the menu bar and the theme choice behind View > Theme.

use win32ui::prelude::*;

use super::Msg;
use esmail_win32::core_glue::compose::Kind;

use esmail_win32::core_glue::ThemeChoice;

/// The theme for `choice`. "System" is the Windows app mode and accent colour
/// as of now; a window following the system re-reads it when it changes.
pub fn theme(choice: ThemeChoice) -> Theme {
    match choice {
        ThemeChoice::Light => Theme::light(),
        ThemeChoice::Dark => Theme::dark(),
        ThemeChoice::System => Theme::system(),
    }
}

/// The commands that act on the selected messages: the Message menu, and the
/// list's right-click menu.
pub fn message_menu() -> Menu<Msg> {
    Menu::new()
        .item("&Reply", Shortcut::ctrl(Key::R), || Msg::Compose(Kind::Reply))
        .item("Reply &all", Shortcut::ctrl(Key::R).with_shift(), || Msg::Compose(Kind::ReplyAll))
        .item("&Forward", Shortcut::ctrl(Key::L), || Msg::Compose(Kind::Forward))
        .separator()
        .item("&Flag / unflag", None, || Msg::ToggleFlag)
        .item("Mark as &read", None, || Msg::SetSeen(true))
        .item("Mark as &unread", None, || Msg::SetSeen(false))
        .separator()
        .item("&Archive", None, || Msg::Archive)
        .item("&Delete", None, || Msg::Delete)
}

/// What the View menu's checks and radio buttons show.
#[derive(Clone, Copy)]
pub struct ViewState {
    pub theme: ThemeChoice,
    pub original_colours: bool,
    pub remote_images: bool,
    /// `None` without a tray icon: the item would do nothing.
    pub close_to_tray: Option<bool>,
}

/// File, Message and View menus in the state `view` describes.
pub fn menu_bar(view: ViewState) -> Menu<Msg> {
    let ViewState { theme: current, original_colours, remote_images, close_to_tray } = view;
    let file = Menu::new()
        .item("&New message", Shortcut::ctrl(Key::N), || Msg::Compose(Kind::New))
        .item("&Add account...", None, || Msg::AddAccount)
        .item("A&ccounts...", None, || Msg::ManageAccounts)
        .separator()
        .item("&Refresh folder", Shortcut::key(Key::F5), || Msg::Refresh)
        .separator()
        .item("&Quit", Shortcut::ctrl(Key::Q), || Msg::Quit);
    let theme = Menu::new()
        .radio_item("&System (follow Windows)", None, current == ThemeChoice::System, || Msg::SetTheme(ThemeChoice::System))
        .radio_item("&Light", None, current == ThemeChoice::Light, || Msg::SetTheme(ThemeChoice::Light))
        .radio_item("&Dark", None, current == ThemeChoice::Dark, || Msg::SetTheme(ThemeChoice::Dark));
    let mut view = Menu::new()
        .submenu("&Theme", theme)
        .checked_item("&Original colours", None, original_colours, move || Msg::OriginalColours(!original_colours))
        .checked_item("Load remote &images", None, remote_images, move || Msg::RemoteImages(!remote_images));
    if let Some(hide) = close_to_tray {
        view = view.separator().checked_item("&Close to tray", None, hide, move || Msg::CloseToTray(!hide));
    }
    Menu::new().submenu("&File", file).submenu("&Message", message_menu()).submenu("&View", view)
}

/// `--acrylic`: the window's title strip is an extended acrylic one that carries
/// the menu, as in Windows Terminal. Falls back to the normal frame by itself
/// where the material is unavailable.
pub fn acrylic(spec: WindowSpec, on: bool) -> WindowSpec {
    if on { spec.backdrop(Backdrop::Acrylic).title_bar(TitleBar::Extended).menu_in_strip(true) } else { spec }
}
