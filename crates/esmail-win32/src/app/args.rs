//! Command-line flags.

use std::path::PathBuf;

const USAGE: &str = "usage: esmail-win32 [--theme light|dark|system] [--profile NAME] \
[--screenshot OUT.png] [--select ROW] [--folder NAME]";

/// How the window is themed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThemeChoice {
    Light,
    Dark,
    /// Follow the Windows "app mode" setting as of when this was chosen.
    System,
}

/// What the flags asked for.
#[derive(Debug)]
pub struct Args {
    pub theme: ThemeChoice,
    /// Read `profiles/NAME/config.toml` instead of the normal config.
    pub profile: Option<String>,
    /// Capture the window to this PNG once the first message is on screen, then exit.
    pub screenshot: Option<PathBuf>,
    /// Open this folder instead of the first account's inbox.
    pub folder: Option<String>,
    /// Select this list row once the folder loads (for screenshots).
    pub select: Option<usize>,
}

impl Args {
    /// Parses `std::env::args`, or explains what was wrong.
    pub fn parse() -> Result<Args, String> {
        Self::parse_from(std::env::args().skip(1))
    }

    fn parse_from(mut args: impl Iterator<Item = String>) -> Result<Args, String> {
        let mut parsed = Args { theme: ThemeChoice::Dark, profile: None, screenshot: None, folder: None, select: None };
        while let Some(flag) = args.next() {
            let mut value = || args.next().ok_or_else(|| format!("{flag} needs a value\n{USAGE}"));
            match flag.as_str() {
                "--theme" => {
                    parsed.theme = match value()?.as_str() {
                        "light" => ThemeChoice::Light,
                        "dark" => ThemeChoice::Dark,
                        "system" => ThemeChoice::System,
                        other => return Err(format!("unknown theme {other:?}\n{USAGE}")),
                    }
                }
                "--profile" => parsed.profile = Some(value()?),
                "--screenshot" => parsed.screenshot = Some(PathBuf::from(value()?)),
                "--folder" => parsed.folder = Some(value()?),
                "--select" => parsed.select = Some(value()?.parse().map_err(|_| format!("--select needs a row number\n{USAGE}"))?),
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
    fn no_flags_means_a_dark_window_on_the_real_profile() {
        let args = parse(&[]).unwrap();
        assert_eq!(args.theme, ThemeChoice::Dark);
        assert!(args.profile.is_none() && args.screenshot.is_none());
    }

    #[test]
    fn every_flag_is_read() {
        let args = parse(&["--theme", "light", "--profile", "mock", "--screenshot", "a.png", "--folder", "INBOX", "--select", "2"]).unwrap();
        assert_eq!(args.theme, ThemeChoice::Light);
        assert_eq!(args.profile.as_deref(), Some("mock"));
        assert_eq!(args.screenshot, Some(PathBuf::from("a.png")));
        assert_eq!((args.folder.as_deref(), args.select), (Some("INBOX"), Some(2)));
    }

    #[test]
    fn a_bad_flag_or_value_is_an_error() {
        assert!(parse(&["--nope"]).is_err());
        assert!(parse(&["--theme"]).is_err());
        assert!(parse(&["--theme", "pink"]).is_err());
        assert!(parse(&["--select", "x"]).is_err());
    }
}
