//! Adapter for Orca's public `terminal` CLI.
//!
//! The GUI/runtime owns terminal attachment state.  `orca-tui run --daemon`
//! therefore talks to it through this small adapter instead of opening the
//! daemon's private Unix sockets.  Keeping the seam here makes the rest of the
//! app independent of argv details and gives tests a single JSON parsing
//! surface.

use std::ffi::{OsStr, OsString};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use serde_json::Value;

use crate::orca_workspaces::{parse_terminals, OrcaTerminal};

/// A text-only screen projection returned by `orca terminal read --screen`.
///
/// The public CLI intentionally returns rendered lines rather than a PTY byte
/// stream.  It is sufficient for a non-owning viewer and, importantly, does
/// not create an attachment or compete with Orca's GUI input owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrcaScreen {
    pub lines: Vec<String>,
    pub status: Option<String>,
    pub next_cursor: Option<String>,
}

/// Result of `orca terminal create --json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedTerminal {
    pub handle: String,
    pub pty_id: Option<String>,
    pub worktree_id: Option<String>,
    pub worktree_path: Option<String>,
    pub title: Option<String>,
}

/// Public Orca CLI adapter.  It is cheap to clone and contains no live socket
/// or attachment state; every operation is an independent process invocation.
#[derive(Debug, Clone)]
pub(crate) struct OrcaCliBridge {
    executable: OsString,
}

impl Default for OrcaCliBridge {
    fn default() -> Self {
        Self::new()
    }
}

impl OrcaCliBridge {
    #[must_use]
    pub(crate) fn new() -> Self {
        Self {
            executable: std::env::var_os("ORCA_CLI").unwrap_or_else(|| OsString::from("orca")),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_executable(executable: impl Into<OsString>) -> Self {
        Self {
            executable: executable.into(),
        }
    }

    fn json(&self, args: &[&str], values: &[(&str, &str)]) -> Result<Value> {
        let mut command = Command::new(&self.executable);
        command.stdin(Stdio::null());
        command.args(args).arg("--json");
        for (name, value) in values {
            command.arg(name).arg(value);
        }
        let output = command.output().with_context(|| {
            format!(
                "failed to run {} {}",
                self.executable.to_string_lossy(),
                args.join(" ")
            )
        })?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            bail!(
                "{} {} failed{}",
                self.executable.to_string_lossy(),
                args.join(" "),
                if stderr.is_empty() {
                    String::new()
                } else {
                    format!(": {stderr}")
                }
            );
        }
        let root: Value = serde_json::from_slice(&output.stdout)
            .with_context(|| format!("parsing {} JSON", args.join(" ")))?;
        if root.get("ok").and_then(Value::as_bool) == Some(false) {
            let message = root
                .get("error")
                .and_then(|error| error.get("message").or(Some(error)))
                .and_then(Value::as_str)
                .unwrap_or("unknown Orca error");
            bail!("Orca {} rejected: {message}", args.join(" "));
        }
        Ok(root)
    }

    /// Return every terminal currently exposed by the Orca runtime.
    pub(crate) fn list_terminals(&self) -> Result<Vec<OrcaTerminal>> {
        let root = self.json(
            &[
                "terminal",
                "list",
                "--limit",
                "10000",
                "--include-visual-layouts",
            ],
            &[],
        )?;
        let bytes = serde_json::to_vec(&root).context("serializing terminal list response")?;
        parse_terminals(&bytes)
    }

    /// Read the current rendered screen for one runtime-issued handle.
    pub(crate) fn read_screen(&self, handle: &str) -> Result<OrcaScreen> {
        let root = self.json(&["terminal", "read", "--terminal", handle, "--screen"], &[])?;
        let terminal = root
            .get("result")
            .and_then(|result| result.get("terminal"))
            .context("Orca terminal read response has no terminal")?;
        let lines = terminal
            .get("tail")
            .and_then(Value::as_array)
            .map(|rows| {
                rows.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        Ok(OrcaScreen {
            lines,
            status: terminal
                .get("status")
                .and_then(Value::as_str)
                .map(str::to_owned),
            next_cursor: terminal
                .get("nextCursor")
                .and_then(Value::as_str)
                .map(str::to_owned),
        })
    }

    /// Send text through Orca's high-level runtime input path.
    pub(crate) fn send_text(&self, handle: &str, text: &str, enter: bool) -> Result<()> {
        let mut command = Command::new(&self.executable);
        command.stdin(Stdio::null());
        command.args(["terminal", "send", "--terminal", handle, "--text", text]);
        if enter {
            command.arg("--enter");
        }
        command.arg("--json");
        let output = command.output().with_context(|| {
            format!(
                "failed to run {} terminal send",
                self.executable.to_string_lossy()
            )
        })?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            bail!(
                "orca terminal send failed{}",
                if stderr.is_empty() {
                    String::new()
                } else {
                    format!(": {stderr}")
                }
            );
        }
        let root: Value =
            serde_json::from_slice(&output.stdout).context("parsing terminal send JSON")?;
        if root.get("ok").and_then(Value::as_bool) == Some(false) {
            bail!("Orca terminal send rejected");
        }
        Ok(())
    }

    /// Create a visible Orca terminal tab without taking ownership of an
    /// existing session.
    pub(crate) fn create(
        &self,
        worktree: Option<&str>,
        command_text: &str,
        title: Option<&str>,
    ) -> Result<CreatedTerminal> {
        let mut command = Command::new(&self.executable);
        command.stdin(Stdio::null());
        command.args(["terminal", "create", "--command", command_text]);
        if let Some(worktree) = worktree {
            command.args(["--worktree", worktree]);
        }
        if let Some(title) = title {
            command.args(["--title", title]);
        }
        command.arg("--json");
        let output = command
            .output()
            .context("failed to run orca terminal create")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            bail!(
                "orca terminal create failed{}",
                if stderr.is_empty() {
                    String::new()
                } else {
                    format!(": {stderr}")
                }
            );
        }
        let root: Value =
            serde_json::from_slice(&output.stdout).context("parsing terminal create JSON")?;
        if root.get("ok").and_then(Value::as_bool) == Some(false) {
            bail!("Orca terminal create rejected");
        }
        let terminal = root
            .get("result")
            .and_then(|result| result.get("terminal"))
            .context("Orca terminal create response has no terminal")?;
        Ok(CreatedTerminal {
            handle: required_string(terminal, "handle")?,
            pty_id: optional_string(terminal, "ptyId"),
            worktree_id: optional_string(terminal, "worktreeId"),
            worktree_path: optional_string(terminal, "worktreePath"),
            title: optional_string(terminal, "title"),
        })
    }

    /// Close the UI tab associated with a runtime-issued terminal handle.
    pub(crate) fn close_tab(&self, handle: &str) -> Result<()> {
        let _ = self.json(&["terminal", "close", "--terminal", handle, "--tab"], &[])?;
        Ok(())
    }
}

fn required_string(row: &Value, key: &str) -> Result<String> {
    row.get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .with_context(|| format!("Orca terminal response missing {key}"))
}

fn optional_string(row: &Value, key: &str) -> Option<String> {
    row.get(key).and_then(Value::as_str).map(str::to_owned)
}

/// Quote one argv token for Orca's shell-backed `--command` field.
pub(crate) fn shell_quote(value: &OsStr) -> String {
    let value = value.to_string_lossy();
    if value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || b"@%_+=:,./-".contains(&byte))
    {
        return value.into_owned();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_quote_preserves_safe_and_spaces() {
        assert_eq!(shell_quote(OsStr::new("codex")), "codex");
        assert_eq!(shell_quote(OsStr::new("--model x")), "'--model x'");
        assert_eq!(shell_quote(OsStr::new("a'b")), "'a'\\''b'");
    }

    #[test]
    fn parses_screen_tail() {
        let root = serde_json::json!({
            "ok": true,
            "result": {"terminal": {"tail": ["one", "two"], "status": "running", "nextCursor": "9"}}
        });
        let terminal = root.get("result").unwrap().get("terminal").unwrap();
        let lines = terminal
            .get("tail")
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect::<Vec<_>>();
        assert_eq!(lines, ["one", "two"]);
    }
}
