use std::fmt;

/// Every failure in this crate: validation, compilation limits, and inputs
/// with no Rift equivalent.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// `value` is not a valid `field`.
    Invalid {
        field: &'static str,
        value: String,
        reason: &'static str,
    },
    /// A compiled subject would exceed the entry cap.
    TooManyEntries { limit: usize },
    /// LuckPerms regex nodes (`r=`/`R=`) are refused rather than guessed.
    RegexNode { key: String },
    /// A promotion or demotion cannot be applied.
    Track { track: String, reason: &'static str },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid {
                field,
                value,
                reason,
            } => write!(f, "invalid {field} {value:?}: {reason}"),
            Self::TooManyEntries { limit } => {
                write!(f, "compiled subject exceeds {limit} entries")
            }
            Self::RegexNode { key } => {
                write!(f, "LuckPerms regex node {key:?} has no Rift equivalent")
            }
            Self::Track { track, reason } => write!(f, "track {track:?}: {reason}"),
        }
    }
}

impl std::error::Error for Error {}

/// An [`Error::Invalid`], quoting at most 96 bytes of the offending value.
pub(crate) fn invalid(field: &'static str, value: &str, reason: &'static str) -> Error {
    let mut end = value.len().min(96);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    Error::Invalid {
        field,
        value: value[..end].to_owned(),
        reason,
    }
}
