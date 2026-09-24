//! Window furniture: the menu bar and the theme choice behind View > Theme.

use win32ui::prelude::*;

use super::Msg;
use super::args::ThemeChoice;

/// The palette for `choice`. "System" reads the Windows "app mode" setting now;
/// it does not track later changes (win32ui has no system-theme notification).
pub fn palette(choice: ThemeChoice) -> Theme {
    match choice {
        ThemeChoice::Light => Theme::light(),
        ThemeChoice::Dark => Theme::dark(),
        ThemeChoice::System if system_uses_light_apps() => Theme::light(),
        ThemeChoice::System => Theme::dark(),
    }
}

fn system_uses_light_apps() -> bool {
    windows_registry::CURRENT_USER
        .open(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize")
        .and_then(|key| key.get_u32("AppsUseLightTheme"))
        .map_or(true, |value| value != 0)
}

/// The commands that act on the selected messages: the Message menu, and the
/// list's right-click menu.
pub fn message_menu() -> Menu<Msg> {
    Menu::new()
        .item("&Flag / unflag", None, || Msg::ToggleFlag)
        .item("Mark as &read", None, || Msg::SetSeen(true))
        .item("Mark as &unread", None, || Msg::SetSeen(false))
        .separator()
        .item("&Archive", None, || Msg::Archive)
        .item("&Delete", None, || Msg::Delete)
}

/// File, Message and View menus; `current` marks the active theme and
/// `original_colours` the state of View > Original colours.
pub fn menu_bar(current: ThemeChoice, original_colours: bool) -> Menu<Msg> {
    let file = Menu::new()
        .item("&Refresh folder", Shortcut::key(Key::F5), || Msg::Refresh)
        .separator()
        .item("&Quit", Shortcut::ctrl(Key::Q), || Msg::Quit);
    let theme = Menu::new()
        .radio_item("&Light", None, current == ThemeChoice::Light, || Msg::SetTheme(ThemeChoice::Light))
        .radio_item("&Dark", None, current == ThemeChoice::Dark, || Msg::SetTheme(ThemeChoice::Dark))
        .radio_item("&System", None, current == ThemeChoice::System, || Msg::SetTheme(ThemeChoice::System));
    let view = Menu::new()
        .submenu("&Theme", theme)
        .checked_item("&Original colours", None, original_colours, move || Msg::OriginalColours(!original_colours));
    Menu::new().submenu("&File", file).submenu("&Message", message_menu()).submenu("&View", view)
}
