//! What the running window needs to live in the tray: the icon's tooltip, and
//! the "open a new message" request a second launch (`--compose`) leaves for the
//! first. Requests reuse the files `esmail::shell` polls for its own requests,
//! but are read when the first instance is woken (see the app's `instance`
//! module), not on a timer.

use std::io;
use std::path::Path;

const COMPOSE_REQUEST: &str = "compose.request";

/// The tray icon's tooltip for `unread` unread messages over all accounts.
pub fn tray_tooltip(unread: u32) -> String {
    if unread == 0 { "esMail".to_string() } else { format!("esMail \u{2014} {unread} unread") }
}

/// Leaves a request in `dir` for the running instance to open a new message.
pub fn send_compose_request(dir: &Path) -> io::Result<()> {
    std::fs::write(dir.join(COMPOSE_REQUEST), b"")
}

/// Consumes the compose request in `dir`, if a later launch left one.
pub fn take_compose_request(dir: &Path) -> bool {
    std::fs::remove_file(dir.join(COMPOSE_REQUEST)).is_ok()
}

/// Forgets a request an instance that died left behind, so the next one does
/// not open a message nobody asked it for.
pub fn discard_compose_request(dir: &Path) {
    let _ = std::fs::remove_file(dir.join(COMPOSE_REQUEST));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("esmail-resident-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_tooltip_shows_the_unread_total_only_when_there_is_one() {
        assert_eq!(tray_tooltip(0), "esMail");
        assert_eq!(tray_tooltip(12), "esMail \u{2014} 12 unread");
    }

    #[test]
    fn a_compose_request_is_consumed_once() {
        let dir = scratch("once");
        assert!(!take_compose_request(&dir));
        send_compose_request(&dir).unwrap();
        assert!(take_compose_request(&dir));
        assert!(!take_compose_request(&dir));
    }

    #[test]
    fn a_stale_compose_request_can_be_discarded() {
        let dir = scratch("stale");
        send_compose_request(&dir).unwrap();
        discard_compose_request(&dir);
        assert!(!take_compose_request(&dir));
    }
}
