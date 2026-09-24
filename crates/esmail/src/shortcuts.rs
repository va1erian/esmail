//! The keyboard shortcut table (issue #107), frontend-agnostic.
//!
//! A frontend translates its own key events into a [`Shortcut`] and asks
//! [`lookup`] which [`Command`] it means, or feeds a whole frame's key state
//! to [`pressed_commands`]. The egui adapter lives in `egui_input.rs`; a
//! Win32 accelerator table can be built straight from [`BINDINGS`].

/// The keys that carry a binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    A,
    F,
    J,
    K,
    N,
    R,
    Enter,
    Delete,
    Backspace,
}

/// Modifier state. `ctrl` covers Ctrl and the platform command key alike;
/// Shift and Alt never change which command a key means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mods {
    pub ctrl: bool,
}

impl Mods {
    pub const NONE: Self = Self { ctrl: false };
    pub const CTRL: Self = Self { ctrl: true };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shortcut {
    pub key: Key,
    pub mods: Mods,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    FocusSearch,
    Compose,
    NextMessage,
    PreviousMessage,
    /// Re-opens the current selection; `j`/`k` already open as they move.
    OpenMessage,
    Reply,
    Archive,
    ToggleStar,
    Delete,
}

/// What the frontend knows that decides whether shortcuts apply at all.
#[derive(Debug, Clone, Copy, Default)]
pub struct Context {
    /// While the search box has focus, typing "j" or "f" into a query must
    /// not also fire a shortcut.
    pub search_focused: bool,
}

const fn bind(key: Key, mods: Mods, command: Command) -> (Shortcut, Command) {
    (Shortcut { key, mods }, command)
}

/// Every binding, in the order their commands run when several keys are
/// pressed in the same frame.
pub const BINDINGS: &[(Shortcut, Command)] = &[
    bind(Key::F, Mods::CTRL, Command::FocusSearch),
    bind(Key::N, Mods::CTRL, Command::Compose),
    bind(Key::J, Mods::NONE, Command::NextMessage),
    bind(Key::K, Mods::NONE, Command::PreviousMessage),
    bind(Key::Enter, Mods::NONE, Command::OpenMessage),
    bind(Key::R, Mods::NONE, Command::Reply),
    bind(Key::A, Mods::NONE, Command::Archive),
    bind(Key::Delete, Mods::NONE, Command::Delete),
    bind(Key::Backspace, Mods::NONE, Command::Delete),
    bind(Key::F, Mods::NONE, Command::ToggleStar),
];

pub fn lookup(shortcut: Shortcut, context: Context) -> Option<Command> {
    if context.search_focused {
        return None;
    }
    BINDINGS.iter().find(|(bound, _)| *bound == shortcut).map(|&(_, command)| command)
}

/// The commands for every key `is_pressed` reports in one frame, in
/// [`BINDINGS`] order, each at most once (Delete and Backspace are one
/// command).
pub fn pressed_commands(mods: Mods, context: Context, is_pressed: impl Fn(Key) -> bool) -> Vec<Command> {
    let mut commands = Vec::new();
    for &(shortcut, _) in BINDINGS {
        if shortcut.mods != mods || !is_pressed(shortcut.key) {
            continue;
        }
        if let Some(command) = lookup(shortcut, context) {
            if !commands.contains(&command) {
                commands.push(command);
            }
        }
    }
    commands
}

#[cfg(test)]
mod tests {
    use super::*;

    const IDLE: Context = Context { search_focused: false };

    fn press(key: Key, mods: Mods) -> Option<Command> {
        lookup(Shortcut { key, mods }, IDLE)
    }

    #[test]
    fn plain_keys_map_to_message_commands() {
        assert_eq!(press(Key::J, Mods::NONE), Some(Command::NextMessage));
        assert_eq!(press(Key::K, Mods::NONE), Some(Command::PreviousMessage));
        assert_eq!(press(Key::Enter, Mods::NONE), Some(Command::OpenMessage));
        assert_eq!(press(Key::R, Mods::NONE), Some(Command::Reply));
        assert_eq!(press(Key::A, Mods::NONE), Some(Command::Archive));
        assert_eq!(press(Key::F, Mods::NONE), Some(Command::ToggleStar));
        assert_eq!(press(Key::Delete, Mods::NONE), Some(Command::Delete));
        assert_eq!(press(Key::Backspace, Mods::NONE), Some(Command::Delete));
    }

    #[test]
    fn ctrl_selects_the_window_commands() {
        assert_eq!(press(Key::F, Mods::CTRL), Some(Command::FocusSearch));
        assert_eq!(press(Key::N, Mods::CTRL), Some(Command::Compose));
    }

    #[test]
    fn ctrl_disables_the_plain_bindings() {
        for key in [Key::J, Key::K, Key::Enter, Key::R, Key::A, Key::Delete, Key::Backspace] {
            assert_eq!(press(key, Mods::CTRL), None, "{key:?}");
        }
    }

    #[test]
    fn plain_n_is_unbound() {
        assert_eq!(press(Key::N, Mods::NONE), None);
    }

    #[test]
    fn a_focused_search_box_suppresses_everything() {
        let typing = Context { search_focused: true };
        for &(shortcut, _) in BINDINGS {
            assert_eq!(lookup(shortcut, typing), None, "{shortcut:?}");
        }
        assert!(pressed_commands(Mods::NONE, typing, |_| true).is_empty());
    }

    #[test]
    fn no_shortcut_is_bound_twice() {
        for (i, (a, _)) in BINDINGS.iter().enumerate() {
            assert!(BINDINGS[i + 1..].iter().all(|(b, _)| a != b), "{a:?}");
        }
    }

    #[test]
    fn pressed_commands_follow_binding_order_and_dedupe() {
        let commands = pressed_commands(Mods::NONE, IDLE, |key| matches!(key, Key::F | Key::Delete | Key::Backspace | Key::J));
        assert_eq!(commands, [Command::NextMessage, Command::Delete, Command::ToggleStar]);
    }

    #[test]
    fn pressed_commands_respect_the_modifier_state() {
        let commands = pressed_commands(Mods::CTRL, IDLE, |key| matches!(key, Key::F | Key::N | Key::J));
        assert_eq!(commands, [Command::FocusSearch, Command::Compose]);
    }
}
