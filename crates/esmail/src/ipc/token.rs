//! The per-session token: a random secret the listener writes to a file in the
//! data directory when it starts, and the GUI reads to prove it is a process of
//! the same user with access to that directory. A new one is made every time
//! the listener starts, so a token that leaked from an earlier session is
//! worthless.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

const FILE_NAME: &str = "ipc.token";

/// Where the token lives: next to the cache, in the per-user data directory.
pub fn path() -> Option<PathBuf> {
    crate::paths::data_dir().map(|dir| dir.join(FILE_NAME))
}

/// The file name, for `uninstall`.
pub const TOKEN_FILE_NAME: &str = FILE_NAME;

/// 256 bits from the OS, as hex.
pub fn generate() -> io::Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| io::Error::other(format!("could not read OS randomness: {e}")))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// Write `token` to `path`, replacing any earlier one. On Unix the file is
/// created readable by its owner only; on Windows it has the ACL of the
/// per-user data directory it sits in, which is already private to the user.
pub fn write(path: &Path, token: &str) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(token.as_bytes())
}

pub fn read(path: &Path) -> io::Result<String> {
    Ok(std::fs::read_to_string(path)?.trim().to_string())
}

/// Compare two tokens without stopping at the first difference, so how long the
/// comparison takes says nothing about how much of a guess was right.
pub fn matches(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |difference, (x, y)| difference | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_long_random_and_different_each_time() {
        let a = generate().unwrap();
        let b = generate().unwrap();
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn a_token_survives_a_round_trip_through_its_file() {
        let dir = std::env::temp_dir().join(format!("esmail-token-test-{}", std::process::id()));
        let path = dir.join("nested").join(FILE_NAME);
        write(&path, "abc123").unwrap();
        assert_eq!(read(&path).unwrap(), "abc123");
        write(&path, "def").unwrap();
        assert_eq!(read(&path).unwrap(), "def", "an older, longer token is fully replaced");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn matches_is_exact() {
        assert!(matches("abc", "abc"));
        assert!(!matches("abc", "abd"));
        assert!(!matches("abc", "abcd"));
        assert!(!matches("", "a"));
        assert!(matches("", ""));
    }
}
