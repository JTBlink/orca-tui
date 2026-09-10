//! Orca workspace catalog integration.
//!
//! Orca owns a persistent catalog that can span multiple repositories and
//! execution hosts. The local `git worktree list` command only describes the
//! repository containing the current directory, so it is deliberately kept as
//! a fallback rather than treated as the complete workspace inventory.

use std::collections::HashSet;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde_json::Value;

const INITIAL_LIMIT: usize = 10_000;
const MAX_LIMIT: usize = 100_000;

/// Host coverage reported by Orca for one listing page.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CatalogScope {
    /// Hosts whose rows are represented by this page.
    pub covered_host_ids: Vec<String>,
    /// Hosts that Orca knows about but did not include in this page.
    pub omitted_host_ids: Vec<String>,
}

/// Complete catalog result, including hosts that could not be queried from the
/// current machine. The rows are still useful, but callers must surface the
/// unresolved host list instead of claiming that the catalog is exhaustive.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OrcaWorkspaceCatalog {
    pub workspaces: Vec<OrcaWorkspace>,
    pub unresolved_host_ids: Vec<String>,
}

/// One row returned by `orca worktree list --json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrcaWorkspace {
    /// Stable Orca selector (`<repo-id>::<path>` in current runtimes).
    pub id: String,
    /// Checkout path. It may belong to another execution host.
    pub path: PathBuf,
    /// Branch name without the `refs/heads/` prefix, or `detached`.
    pub branch: String,
    /// User-facing Orca workspace title.
    pub display_name: String,
    /// Stable Orca repository id.
    pub repo_id: String,
    /// Durable project id, when the runtime exposes one.
    pub project_id: Option<String>,
    /// Owning execution host, e.g. `local` or a remote host id.
    pub host_id: Option<String>,
    /// Whether Orca has archived the workspace. Archived rows remain in the
    /// catalog and are intentionally not silently discarded.
    pub is_archived: bool,
    /// Orca's current workspace status, when available.
    pub workspace_status: Option<String>,
    /// Whether this is the repository's main checkout.
    pub is_main_worktree: bool,
}

impl OrcaWorkspace {
    /// A compact label that remains unique enough when several repositories
    /// expose the same branch name such as main or master.
    #[must_use]
    pub fn sidebar_name(&self) -> String {
        let repository = self
            .project_id
            .as_deref()
            .and_then(|project| project.rsplit('/').next())
            .filter(|name| !name.is_empty())
            .or_else(|| (!self.repo_id.is_empty()).then_some(self.repo_id.as_str()));
        match repository {
            Some(repository) if repository != self.display_name => {
                format!("{repository}/{}", self.display_name)
            }
            _ => self.display_name.clone(),
        }
    }

    /// The branch/host text shown on the second sidebar line.
    #[must_use]
    pub fn sidebar_branch(&self) -> String {
        let mut label = self.branch.clone();
        if let Some(host) = self.host_id.as_deref().filter(|host| *host != "local") {
            label.push_str(" @ ");
            label.push_str(host);
        }
        if self.is_archived {
            label.push_str(" [archived]");
        }
        label
    }

    /// Optional secondary sidebar label. For ordinary local workspaces the
    /// primary name already contains the branch (`repo/branch`), so omitting
    /// the duplicate branch gives the fixed-width sidebar enough room to show
    /// the complete workspace name. Host/archive annotations remain visible
    /// because they are not encoded in the primary name.
    #[must_use]
    pub fn sidebar_detail(&self) -> Option<String> {
        let detail = self.sidebar_branch();
        let local_host = self
            .host_id
            .as_deref()
            .map(|host| host == "local")
            .unwrap_or(true);
        if local_host
            && !self.is_archived
            && self
                .sidebar_name()
                .strip_suffix(&self.branch)
                .is_some_and(|prefix| prefix.is_empty() || prefix.ends_with('/'))
        {
            None
        } else {
            Some(detail)
        }
    }

    /// Whether this row can be launched by a local PTY.
    #[must_use]
    pub fn is_local(&self) -> bool {
        self.host_id
            .as_deref()
            .map(|host| host == "local")
            .unwrap_or(true)
            && self.path.is_dir()
    }
}

/// Query Orca's complete workspace catalog.
///
/// The command is intentionally argv-based (no shell interpolation). Set
/// `ORCA_CLI` to an alternate executable for development or tests. If Orca
/// reports a truncated page, the query is retried with a larger limit and then
/// fails instead of silently presenting an incomplete catalog.
pub fn list_all() -> Result<Vec<OrcaWorkspace>> {
    Ok(list_all_with_scope()?.workspaces)
}

/// Query every host that the local Orca CLI can reach.
///
/// `orca worktree list` intentionally reports host coverage. A successful
/// local page can therefore still omit paired runtime hosts. We follow those
/// `runtime:<id>` entries with `--environment <id>` requests and retain any
/// host that cannot be reached as an explicit completeness warning.
pub fn list_all_with_scope() -> Result<OrcaWorkspaceCatalog> {
    if std::env::var_os("ORCA_TUI_DISABLE_ORCA_CATALOG").is_some() {
        bail!("Orca workspace catalog disabled by ORCA_TUI_DISABLE_ORCA_CATALOG");
    }

    let (initial, initial_scope) = query_complete(None)?;
    let mut workspaces = initial;
    let mut unresolved = Vec::new();
    let mut visited = HashSet::new();
    let mut pending = initial_scope.omitted_host_ids;
    let paired_environment_ids = if pending.iter().any(|host| host.starts_with("runtime:")) {
        paired_environment_ids()
    } else {
        HashSet::new()
    };

    while let Some(host_id) = pending.pop() {
        if !visited.insert(host_id.clone()) {
            continue;
        }
        let Some(environment_id) = host_id.strip_prefix("runtime:") else {
            // SSH hosts require a pairing code or a configured host selector;
            // the unscoped worktree command cannot invent either one.
            unresolved.push(host_id);
            continue;
        };
        if !paired_environment_ids.contains(environment_id) {
            unresolved.push(host_id);
            continue;
        }
        match query_complete(Some(environment_id)) {
            Ok((rows, scope)) => {
                workspaces.extend(rows);
                pending.extend(scope.omitted_host_ids);
            }
            Err(_) => unresolved.push(host_id),
        }
    }

    // A workspace id is stable across repeated host queries. Deduplicate rows
    // while preserving Orca's ordering (local rows first, then remote rows).
    let mut seen = HashSet::new();
    workspaces.retain(|workspace| seen.insert(workspace.id.clone()));
    unresolved.sort();
    unresolved.dedup();
    Ok(OrcaWorkspaceCatalog {
        workspaces,
        unresolved_host_ids: unresolved,
    })
}

fn paired_environment_ids() -> HashSet<String> {
    let executable = std::env::var_os("ORCA_CLI").unwrap_or_else(|| "orca".into());
    let Ok(output) = Command::new(executable)
        .args(["environment", "list", "--json"])
        .output()
    else {
        return HashSet::new();
    };
    if !output.status.success() {
        return HashSet::new();
    }
    let Ok(root) = serde_json::from_slice::<Value>(&output.stdout) else {
        return HashSet::new();
    };
    root.get("result")
        .and_then(|result| result.get("environments"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|environment| environment.get("id").and_then(Value::as_str))
        .map(str::to_owned)
        .collect()
}

fn query_complete(environment: Option<&str>) -> Result<(Vec<OrcaWorkspace>, CatalogScope)> {
    let mut limit = INITIAL_LIMIT;
    loop {
        let (workspaces, total_count, truncated, scope) = query(limit, environment)?;
        let complete = !truncated
            && total_count
                .map(|total| workspaces.len() >= total)
                .unwrap_or(true);
        if complete {
            return Ok((workspaces, scope));
        }
        if limit >= MAX_LIMIT {
            bail!(
                "Orca workspace catalog is truncated (received {}, total {:?})",
                workspaces.len(),
                total_count
            );
        }
        limit = MAX_LIMIT;
    }
}

fn query(
    limit: usize,
    environment: Option<&str>,
) -> Result<(Vec<OrcaWorkspace>, Option<usize>, bool, CatalogScope)> {
    let executable = std::env::var_os("ORCA_CLI").unwrap_or_else(|| "orca".into());
    let mut command = Command::new(&executable);
    command.args(["worktree", "list", "--json", "--limit"]);
    command.arg(limit.to_string());
    if let Some(environment) = environment {
        command.args(["--environment", environment]);
    }
    let output = command.output().with_context(|| {
        format!(
            "failed to run {} worktree list",
            executable.to_string_lossy()
        )
    })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        bail!(
            "{} worktree list failed{}",
            executable.to_string_lossy(),
            if stderr.is_empty() {
                String::new()
            } else {
                format!(": {stderr}")
            }
        );
    }
    parse_catalog_details(&output.stdout)
}

/// Parse an Orca JSON envelope. Kept separate from process execution so the
/// response shape and completeness rules are unit-testable without a daemon.
pub fn parse_catalog(bytes: &[u8]) -> Result<(Vec<OrcaWorkspace>, Option<usize>, bool)> {
    let (workspaces, total_count, truncated, _) = parse_catalog_details(bytes)?;
    Ok((workspaces, total_count, truncated))
}

fn parse_catalog_details(
    bytes: &[u8],
) -> Result<(Vec<OrcaWorkspace>, Option<usize>, bool, CatalogScope)> {
    let root: Value = serde_json::from_slice(bytes).context("parsing Orca workspace JSON")?;
    if root.get("ok").and_then(Value::as_bool) == Some(false) {
        let message = root
            .get("error")
            .and_then(|error| error.get("message").or(Some(error)))
            .and_then(Value::as_str)
            .unwrap_or("unknown Orca error");
        bail!("Orca workspace catalog rejected: {message}");
    }
    let result = root
        .get("result")
        .context("Orca workspace response has no result")?;
    let rows = result
        .get("worktrees")
        .and_then(Value::as_array)
        .context("Orca workspace response has no worktrees array")?;
    let mut workspaces = Vec::with_capacity(rows.len());
    for (index, row) in rows.iter().enumerate() {
        workspaces.push(
            parse_workspace(row).with_context(|| format!("parsing Orca workspace row {index}"))?,
        );
    }
    let total_count = result
        .get("totalCount")
        .and_then(Value::as_u64)
        .map(|value| value as usize);
    let truncated = result
        .get("truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let host_scope = result.get("hostScope");
    let scope = CatalogScope {
        covered_host_ids: host_scope
            .and_then(|scope| scope.get("hostIds"))
            .and_then(Value::as_array)
            .map(|hosts| {
                hosts
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default(),
        omitted_host_ids: host_scope
            .and_then(|scope| scope.get("omittedHostIds"))
            .and_then(Value::as_array)
            .map(|hosts| {
                hosts
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default(),
    };
    Ok((workspaces, total_count, truncated, scope))
}

fn parse_workspace(row: &Value) -> Result<OrcaWorkspace> {
    let path = required_string(row, "path")?;
    let path_buf = PathBuf::from(path);
    let id = optional_string(row, "id")
        .or_else(|| optional_string(row, "worktreeId"))
        .unwrap_or_else(|| path_buf.display().to_string());
    let branch = optional_string(row, "branch")
        .map(|branch| normalize_branch(&branch))
        .unwrap_or_else(|| "detached".to_owned());
    let display_name = optional_string(row, "displayName")
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| branch.clone());
    let repo_id = optional_string(row, "repoId").unwrap_or_default();

    Ok(OrcaWorkspace {
        id,
        path: path_buf,
        branch,
        display_name,
        repo_id,
        project_id: optional_string(row, "projectId"),
        host_id: optional_string(row, "hostId").or_else(|| {
            row.get("identity")
                .and_then(|identity| optional_string(identity, "executionHostId"))
        }),
        is_archived: row
            .get("isArchived")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        workspace_status: optional_string(row, "workspaceStatus"),
        is_main_worktree: row
            .get("isMainWorktree")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

fn required_string(row: &Value, key: &str) -> Result<String> {
    optional_string(row, key).with_context(|| format!("missing non-empty `{key}`"))
}

fn optional_string(row: &Value, key: &str) -> Option<String> {
    row.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn normalize_branch(branch: &str) -> String {
    branch
        .strip_prefix("refs/heads/")
        .unwrap_or(branch)
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_catalog_reads_all_workspace_metadata() {
        let json = br#"{
          "ok": true,
          "result": {
            "totalCount": 2,
            "truncated": false,
            "worktrees": [
              {
                "id": "repo-a::/work/main",
                "path": "/work/main",
                "branch": "refs/heads/main",
                "displayName": "main",
                "repoId": "repo-a",
                "projectId": "github:org/repo-a",
                "hostId": "local",
                "isArchived": false,
                "workspaceStatus": "in-progress",
                "isMainWorktree": true
              },
              {
                "worktreeId": "repo-b::/remote/feature",
                "path": "/remote/feature",
                "branch": "refs/heads/feature/login",
                "displayName": "login",
                "repoId": "repo-b",
                "hostId": "ssh-host"
              }
            ]
          }
        }"#;
        let (rows, total, truncated) = parse_catalog(json).expect("parse catalog");
        assert_eq!(rows.len(), 2);
        assert_eq!(total, Some(2));
        assert!(!truncated);
        assert_eq!(rows[0].branch, "main");
        assert!(rows[0].is_main_worktree);
        assert_eq!(rows[1].id, "repo-b::/remote/feature");
        assert_eq!(rows[1].branch, "feature/login");
        assert_eq!(rows[1].host_id.as_deref(), Some("ssh-host"));
        assert!(!rows[1].is_archived);
    }

    #[test]
    fn parse_catalog_reads_host_scope_and_identity_fallback() {
        let json = br#"{
          "ok": true,
          "result": {
            "totalCount": 1,
            "truncated": false,
            "hostScope": {
              "hostIds": ["local"],
              "omittedHostIds": ["runtime:env-1"]
            },
            "worktrees": [{
              "id": "repo::/work/main",
              "path": "/work/main",
              "branch": "main",
              "displayName": "main",
              "identity": {"executionHostId": "runtime:env-1"}
            }]
          }
        }"#;
        let (rows, total, truncated) = parse_catalog(json).expect("parse catalog");
        assert_eq!(rows[0].host_id.as_deref(), Some("runtime:env-1"));
        assert_eq!(total, Some(1));
        assert!(!truncated);
        let (_, _, _, scope) = parse_catalog_details(json).expect("parse scope");
        assert_eq!(scope.covered_host_ids, vec!["local"]);
        assert_eq!(scope.omitted_host_ids, vec!["runtime:env-1"]);
    }

    #[test]
    fn parse_catalog_rejects_truncated_error_envelope() {
        let json = br#"{"ok":false,"error":{"message":"daemon unavailable"}}"#;
        let error = parse_catalog(json).expect_err("error envelope must fail");
        assert!(error.to_string().contains("daemon unavailable"));
    }

    #[test]
    fn normalize_branch_handles_detached_and_full_refs() {
        assert_eq!(normalize_branch("refs/heads/dev"), "dev");
        assert_eq!(normalize_branch("HEAD"), "HEAD");
    }

    #[test]
    fn sidebar_name_disambiguates_same_branch_across_repositories() {
        let row = OrcaWorkspace {
            id: "repo::/work/main".to_owned(),
            path: PathBuf::from("/work/main"),
            branch: "main".to_owned(),
            display_name: "main".to_owned(),
            repo_id: "repo".to_owned(),
            project_id: Some("github:org/project".to_owned()),
            host_id: Some("local".to_owned()),
            is_archived: false,
            workspace_status: None,
            is_main_worktree: true,
        };
        assert_eq!(row.sidebar_name(), "project/main");
        assert_eq!(row.sidebar_branch(), "main");
    }

    #[test]
    fn sidebar_detail_omits_redundant_local_branch_but_keeps_annotations() {
        let local = OrcaWorkspace {
            id: "repo::/work/main".to_owned(),
            path: PathBuf::from("/work/main"),
            branch: "main".to_owned(),
            display_name: "main".to_owned(),
            repo_id: "repo".to_owned(),
            project_id: Some("github:org/repo".to_owned()),
            host_id: Some("local".to_owned()),
            is_archived: false,
            workspace_status: None,
            is_main_worktree: true,
        };
        assert_eq!(local.sidebar_name(), "repo/main");
        assert_eq!(local.sidebar_detail(), None);

        let remote = OrcaWorkspace {
            host_id: Some("build-host".to_owned()),
            ..local.clone()
        };
        assert_eq!(
            remote.sidebar_detail().as_deref(),
            Some("main @ build-host")
        );

        let archived = OrcaWorkspace {
            is_archived: true,
            ..local
        };
        assert_eq!(
            archived.sidebar_detail().as_deref(),
            Some("main [archived]")
        );
    }
}
