//! Window furniture: the menu bar and the theme choice behind View > Theme.

use win32ui::prelude::*;

use super::Msg;
use esmail_win32::core_glue::compose::Kind;

use super::args::ThemeChoice;

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

/// File, Message and View menus; `current` marks the active theme, and the
/// flags the state of View > Original colours and View > Load remote images.
pub fn menu_bar(current: ThemeChoice, original_colours: bool, remote_images: bool) -> Menu<Msg> {
    let file = Menu::new()
        .item("&New message", Shortcut::ctrl(Key::N), || Msg::Compose(Kind::New))
        .separator()
        .item("&Refresh folder", Shortcut::key(Key::F5), || Msg::Refresh)
        .separator()
        .item("&Quit", Shortcut::ctrl(Key::Q), || Msg::Quit);
    let theme = Menu::new()
        .radio_item("&System (follow Windows)", None, current == ThemeChoice::System, || Msg::SetTheme(ThemeChoice::System))
        .radio_item("&Light", None, current == ThemeChoice::Light, || Msg::SetTheme(ThemeChoice::Light))
        .radio_item("&Dark", None, current == ThemeChoice::Dark, || Msg::SetTheme(ThemeChoice::Dark));
    let view = Menu::new()
        .submenu("&Theme", theme)
        .checked_item("&Original colours", None, original_colours, move || Msg::OriginalColours(!original_colours))
        .checked_item("Load remote &images", None, remote_images, move || Msg::RemoteImages(!remote_images));
    Menu::new().submenu("&File", file).submenu("&Message", message_menu()).submenu("&View", view)
}
