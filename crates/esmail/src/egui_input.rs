//! egui adapters for the frontend-agnostic tables: key events to
//! [`esmail::shortcuts`] commands, and [`ThemeMode`] to egui's theme
//! preference.

use esmail::config::ThemeMode;
use esmail::shortcuts::{self, Command, Context, Key, Mods};

pub fn theme_preference(mode: ThemeMode) -> egui::ThemePreference {
    match mode {
        ThemeMode::Dark => egui::ThemePreference::Dark,
        ThemeMode::Light => egui::ThemePreference::Light,
        ThemeMode::System => egui::ThemePreference::System,
    }
}

fn egui_key(key: Key) -> egui::Key {
    match key {
        Key::A => egui::Key::A,
        Key::F => egui::Key::F,
        Key::J => egui::Key::J,
        Key::K => egui::Key::K,
        Key::N => egui::Key::N,
        Key::R => egui::Key::R,
        Key::Enter => egui::Key::Enter,
        Key::Delete => egui::Key::Delete,
        Key::Backspace => egui::Key::Backspace,
    }
}

/// The commands the keys pressed this frame stand for.
pub fn pressed_commands(input: &egui::InputState, context: Context) -> Vec<Command> {
    let mods = Mods { ctrl: input.modifiers.ctrl || input.modifiers.command };
    shortcuts::pressed_commands(mods, context, |key| input.key_pressed(egui_key(key)))
}
