//! Optional debug logging helpers.
//!
//! Runtime logs are intentionally opt-in (ORCA_DEBUG_LOG=1) and path-light:
//! they record counts and state transitions useful for diagnosis without
//! persisting user directory names, prompts, commands, tokens, or raw payloads.

use std::fmt;
use std::io::Write;

/// Environment variable that enables the debug log.
pub(crate) const ENV: &str = "ORCA_DEBUG_LOG";

/// Shared live-debug sink used by existing terminal diagnostics.
pub(crate) const PATH: &str = "/tmp/orca-live.log";

/// Unique prefix for worktree/sidebar diagnosis.
pub(crate) const WORKTREE_PREFIX: &str = "[DEBUG-worktree]";

/// Whether optional debug logging is enabled.
#[must_use]
pub(crate) fn enabled() -> bool {
    std::env::var_os(ENV).is_some()
}

/// Append one already-sanitized line to the debug log.
///
/// I/O errors are ignored because diagnostics must never crash or disturb the
/// TUI. Callers are responsible for avoiding raw absolute paths and payloads.
pub(crate) fn append(args: fmt::Arguments<'_>) {
    if !enabled() {
        return;
    }
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(PATH)
    else {
        return;
    };
    let _ = file.write_fmt(args);
    let _ = file.write_all(b"\n");
}

/// Classify an error without logging the error text, which may contain paths.
#[must_use]
pub(crate) fn classify_error(err: &(dyn std::error::Error + 'static)) -> &'static str {
    let msg = err.to_string();
    if msg.contains("not inside a git work tree") {
        "not_git_work_tree"
    } else if msg.contains("failed to spawn") {
        "spawn_failed"
    } else if msg.contains("not accessible") {
        "path_inaccessible"
    } else if msg.contains("timed out") {
        "timeout"
    } else {
        "other"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_error_uses_safe_categories() {
        let err = anyhow::anyhow!("/Users/example/private is not inside a git work tree");
        assert_eq!(classify_error(err.as_ref()), "not_git_work_tree");

        let err = anyhow::anyhow!("repo root /Users/example/private is not accessible");
        assert_eq!(classify_error(err.as_ref()), "path_inaccessible");
    }
}
