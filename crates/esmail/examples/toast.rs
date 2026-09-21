//! Pops up ONE real toast on the desktop (a made-up "Example Sender" mail) and
//! waits for a click, to check the platform toast code by hand:
//! `cargo run -p esmail --example toast`.
//!
//! Prints the account id when the toast is clicked (what esmail uses to open
//! that account). Waits 60 seconds, or pass a number of seconds. Set
//! `RUST_LOG=debug` to see failures the toast code only logs.

use esmail::platform;

fn main() {
    env_logger::init();
    let seconds: u64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(60);

    platform::set_toast_click_handler(|account| println!("toast clicked, account id: {account}"));
    platform::show_new_mail_toast(
        "test-account@example.invalid",
        "Example: New mail from Example Sender (test toast)",
        "Only a test of esmail's notifications: <b>markup</b> & \"quotes\" stay plain text",
    );
    println!("toast shown; waiting {seconds}s for a click (Ctrl+C to stop)");
    std::thread::sleep(std::time::Duration::from_secs(seconds));
}
