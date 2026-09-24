//! Command-line flags.

use std::path::PathBuf;

use esmail_win32::core_glue::ThemeChoice;
use esmail_win32::core_glue::compose::Kind;

const USAGE: &str = "usage: esmail-win32 [--theme light|dark|system] [--profile NAME] \
[--screenshot OUT.png] [--select ROW] [--folder NAME] \n[--compose new|reply|reply-all|forward] [--acrylic] [--account N] [--remote-images] [--accounts]";

/// What the flags asked for.
#[derive(Debug)]
pub struct Args {
    /// `--theme`: overrides the saved choice for this run.
    pub theme: Option<ThemeChoice>,
    /// Read `profiles/NAME/config.toml` instead of the normal config.
    pub profile: Option<String>,
    /// Capture the window to this PNG once the first message is on screen, then exit.
    pub screenshot: Option<PathBuf>,
    /// Open this folder instead of the first account's inbox.
    pub folder: Option<String>,
    /// Open the folder of this account (0-based) instead of the first one's.
    pub account: usize,
    /// Select this list row once the folder loads (for screenshots).
    pub select: Option<usize>,
    /// Open a compose window of this kind once the folder (and the message chosen
    /// with `--select`) is up; screenshots then include it.
    pub compose: Option<Kind>,
    /// `--acrylic`: an acrylic extended title strip with the menu on it (off by
    /// default).
    pub acrylic: bool,
    /// `--remote-images`: start with View > Load remote images on.
    pub remote_images: bool,
    /// `--accounts`: open the Accounts window at start.
    pub accounts: bool,
}

impl Args {
    /// Parses `std::env::args`, or explains what was wrong.
    pub fn parse() -> Result<Args, String> {
        Self::parse_from(std::env::args().skip(1))
    }

    fn parse_from(mut args: impl Iterator<Item = String>) -> Result<Args, String> {
        let mut parsed = Args { theme: None, profile: None, screenshot: None, folder: None, account: 0, select: None, compose: None, acrylic: false, remote_images: false, accounts: false };
        while let Some(flag) = args.next() {
            let mut value = || args.next().ok_or_else(|| format!("{flag} needs a value\n{USAGE}"));
            match flag.as_str() {
                "--theme" => {
                    parsed.theme = Some(match value()?.as_str() {
                        "light" => ThemeChoice::Light,
                        "dark" => ThemeChoice::Dark,
                        "system" => ThemeChoice::System,
                        other => return Err(format!("unknown theme {other:?}\n{USAGE}")),
                    })
                }
                "--profile" => parsed.profile = Some(value()?),
                "--screenshot" => parsed.screenshot = Some(PathBuf::from(value()?)),
                "--folder" => parsed.folder = Some(value()?),
                "--account" => parsed.account = value()?.parse().map_err(|_| format!("--account needs an account number
{USAGE}"))?,
                "--select" => parsed.select = Some(value()?.parse().map_err(|_| format!("--select needs a row number\n{USAGE}"))?),
                "--acrylic" => parsed.acrylic = true,
                "--remote-images" => parsed.remote_images = true,
                "--accounts" => parsed.accounts = true,
                "--compose" => {
                    parsed.compose = Some(match value()?.as_str() {
                        "new" => Kind::New,
                        "reply" => Kind::Reply,
                        "reply-all" => Kind::ReplyAll,
                        "forward" => Kind::Forward,
                        other => return Err(format!("unknown compose kind {other:?}
{USAGE}")),
                    })
                }
                other => return Err(format!("unknown flag {other}\n{USAGE}")),
            }
        }
        Ok(parsed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Args, String> {
        Args::parse_from(args.iter().map(|a| a.to_string()))
    }

    #[test]
    fn no_flags_means_the_saved_theme_on_the_real_profile() {
        let args = parse(&[]).unwrap();
        assert_eq!(args.theme, None);
        assert!(args.profile.is_none() && args.screenshot.is_none());
    }

    #[test]
    fn every_flag_is_read() {
        let args = parse(&["--theme", "light", "--profile", "mock", "--screenshot", "a.png", "--folder", "INBOX", "--select", "2"]).unwrap();
        assert_eq!(args.theme, Some(ThemeChoice::Light));
        assert_eq!(args.profile.as_deref(), Some("mock"));
        assert_eq!(args.screenshot, Some(PathBuf::from("a.png")));
        assert_eq!((args.folder.as_deref(), args.select), (Some("INBOX"), Some(2)));
        assert_eq!(parse(&["--account", "1"]).unwrap().account, 1);
    }

    #[test]
    fn a_bad_flag_or_value_is_an_error() {
        assert!(parse(&["--nope"]).is_err());
        assert!(parse(&["--theme"]).is_err());
        assert!(parse(&["--theme", "pink"]).is_err());
        assert!(parse(&["--select", "x"]).is_err());
    }
}
