//! Rift's own secrets and the environment of the processes it starts.
//!
//! Rift reads its tokens from environment variables (`admin.token_env`,
//! `messaging.principals[].token_env`, `nodes.*.token_env`,
//! `forwarding.secret_env`, `extensions.integrations.*.headers_env`, and the
//! node agent's `--token-env` and `--forwarding-secret-env`). A child inherits
//! its parent's environment by default, so before this module every managed
//! Minecraft server could read the admin token with `System.getenv`, and any
//! plugin on that server with it. The names are known from configuration, so
//! Rift removes exactly those from every child it spawns and leaves everything
//! else alone: networks hand database passwords to their server plugins
//! through the environment, and an allowlist by default would break them.
//! [`ProcessEnvironment`](crate::config::ProcessEnvironment) is the opt-in for
//! a stricter policy per server or group.
//!
//! The registry only grows. A reload that drops a variable from the
//! configuration does not remove the value from Rift's own environment, where
//! the next child would find it, so a name once protected stays protected for
//! the life of the process.
use std::{
    collections::BTreeSet,
    sync::{PoisonError, RwLock},
};

static PROTECTED: RwLock<BTreeSet<String>> = RwLock::new(BTreeSet::new());

/// Treat these variables as Rift's secrets from now on.
pub fn protect<I, S>(names: I)
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut protected = PROTECTED.write().unwrap_or_else(PoisonError::into_inner);
    for name in names {
        protected.insert(name.into());
    }
}

/// Every variable registered with [`protect`].
pub fn protected() -> Vec<String> {
    PROTECTED
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .cloned()
        .collect()
}

/// Whether `name` holds one of Rift's secrets. Windows compares variable names
/// without regard to case, so this does too there.
pub fn is_protected(name: &str) -> bool {
    let protected = PROTECTED.read().unwrap_or_else(PoisonError::into_inner);
    if cfg!(windows) {
        protected
            .iter()
            .any(|secret| secret.eq_ignore_ascii_case(name))
    } else {
        protected.contains(name)
    }
}

/// Process hardening that must run before any secret is read and before any
/// other thread exists.
///
/// On Linux this clears the dumpable flag. `/proc/<pid>/environ`, `mem` and
/// `maps` of a non-dumpable process are owned by root and refused to other
/// processes of the same user, and so is `ptrace`, so a managed server running
/// as Rift's user can no longer read Rift's environment or memory from
/// outside. It also disables core dumps, which would otherwise write every
/// secret to disk. This is hygiene, not isolation: a process of the same user
/// can still write Rift's files or signal it, and the flag resets on `execve`,
/// so children are dumpable again (they do not hold Rift's secrets after
/// [`protect`]). Elsewhere this does nothing.
///
/// `RIFT_DUMPABLE=1` skips it, for a debugging session or for monitoring
/// that counts Rift's descriptors through `/proc/<pid>/fd` (see
/// docs/operations.md). The variable is read before anything else, so it
/// cannot be turned on later by a reload.
pub fn harden_process() {
    #[cfg(target_os = "linux")]
    {
        if std::env::var_os(KEEP_DUMPABLE).is_some_and(|value| value == "1") {
            eprintln!(
                "rift: warning: {KEEP_DUMPABLE}=1: processes of the same user can read Rift's environment and memory, and a crash writes a core dump"
            );
            return;
        }
        // SAFETY: a plain prctl without pointer arguments.
        if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } != 0 {
            eprintln!(
                "rift: warning: could not clear the dumpable flag ({}); processes of the same user can read Rift's environment",
                std::io::Error::last_os_error()
            );
        }
    }
}

/// Set to `1` to keep Rift dumpable on Linux; see [`harden_process`].
pub const KEEP_DUMPABLE: &str = "RIFT_DUMPABLE";

/// Valid names for `env` keys and `inherit_env` entries: what a POSIX shell
/// accepts, so the variable can be read the same way everywhere.
pub fn valid_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    name.len() <= 256
        && bytes
            .next()
            .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_stay_protected_once_registered() {
        protect(["RIFT_TEST_REGISTRY_A"]);
        protect(Vec::<String>::new());
        assert!(is_protected("RIFT_TEST_REGISTRY_A"));
        assert!(protected().contains(&"RIFT_TEST_REGISTRY_A".to_owned()));
        assert!(!is_protected("RIFT_TEST_REGISTRY_B"));
    }

    #[test]
    fn variable_names_follow_the_shell() {
        for name in ["PATH", "_X", "DB_PASSWORD_2"] {
            assert!(valid_name(name), "{name}");
        }
        for name in ["", "2X", "A-B", "A=B", "A B", "Ä"] {
            assert!(!valid_name(name), "{name}");
        }
    }
}
