//! Exit codes and the error type every command returns.

use std::fmt;

/// The exit codes of every atelier command. Code 6 is unused since revision 5.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum Exit {
    /// Done; GREEN / PASS.
    Ok = 0,
    /// RED / FAIL.
    Fail = 1,
    /// Wrong usage.
    Usage = 2,
    /// No machine available for a platform; nothing passed.
    NoMachine = 3,
    /// Refused, nothing changed; the message names what is not in place.
    Refused = 4,
    /// Merge conflict; files listed.
    Conflict = 5,
    /// GREEN, not promoted: the main checkout was busy.
    NotPromoted = 7,
}

impl Exit {
    pub fn code(self) -> i32 {
        self as i32
    }
}

/// A command's failure: the exit code and the message printed on stderr.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub exit: Exit,
    pub message: String,
}

impl Failure {
    pub fn new(exit: Exit, message: impl Into<String>) -> Self {
        Failure {
            exit,
            message: message.into(),
        }
    }

    pub fn refused(message: impl Into<String>) -> Self {
        Self::new(Exit::Refused, message)
    }

    pub fn usage(message: impl Into<String>) -> Self {
        Self::new(Exit::Usage, message)
    }

    pub fn fail(message: impl Into<String>) -> Self {
        Self::new(Exit::Fail, message)
    }

    /// Prefixes the message with context, keeping the exit code.
    pub fn context(mut self, context: impl fmt::Display) -> Self {
        self.message = format!("{context}: {}", self.message);
        self
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Failure {}

/// An I/O error in atelier's own files is a refusal: nothing was changed by the command itself.
impl From<std::io::Error> for Failure {
    fn from(e: std::io::Error) -> Self {
        Failure::refused(e.to_string())
    }
}

pub type Result<T, E = Failure> = std::result::Result<T, E>;

/// Adds a path or other context to I/O errors.
pub trait IoContext<T> {
    fn ctx(self, what: impl fmt::Display) -> Result<T>;
}

impl<T> IoContext<T> for std::io::Result<T> {
    fn ctx(self, what: impl fmt::Display) -> Result<T> {
        self.map_err(|e| Failure::refused(format!("{what}: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_match_the_design_table() {
        assert_eq!(Exit::Ok.code(), 0);
        assert_eq!(Exit::Fail.code(), 1);
        assert_eq!(Exit::Usage.code(), 2);
        assert_eq!(Exit::NoMachine.code(), 3);
        assert_eq!(Exit::Refused.code(), 4);
        assert_eq!(Exit::Conflict.code(), 5);
        assert_eq!(Exit::NotPromoted.code(), 7);
    }
}
