//! Fetching a message's remote images, once the user has allowed it.
//!
//! The HTML view has no network layer; a frontend hands it [`fetch`] on the view's
//! render thread, where blocking on the network is fine.

use std::sync::OnceLock;
use std::time::Duration;

/// Give up on one image after this long, so a dead tracking-pixel host cannot
/// hold up the rest of the message (ureq has no timeout unless asked).
const FETCH_TIMEOUT: Duration = Duration::from_secs(15);

fn agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| ureq::Agent::new_with_config(ureq::Agent::config_builder().timeout_global(Some(FETCH_TIMEOUT)).build()))
}

/// The bytes at an `http(s)` `url`, or `None` for any other scheme or a failed
/// fetch (which is logged).
pub fn fetch(url: &str) -> Option<Vec<u8>> {
    let scheme_ok = ["http://", "https://"].iter().any(|scheme| url.len() > scheme.len() && url[..scheme.len()].eq_ignore_ascii_case(scheme));
    if !scheme_ok {
        return None;
    }
    match agent().get(url).call().and_then(|response| response.into_body().read_to_vec()) {
        Ok(bytes) => Some(bytes),
        Err(error) => {
            log::warn!("could not fetch remote image {url}: {error}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_web_urls_are_fetched() {
        assert_eq!(fetch("file:///c:/secret.png"), None);
        assert_eq!(fetch("ftp://example.com/a.png"), None);
        assert_eq!(fetch("cid:logo"), None);
    }
}
