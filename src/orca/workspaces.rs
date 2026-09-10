//! Orca workspace catalog integration.
//!
//! Orca owns a persistent catalog that can span multiple repositories and
//! execution hosts. The local `git worktree list` command only describes the
//! repository containing the current directory, so it is deliberately kept as
//! a fallback rather than treated as the complete workspace inventory.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde_json::Value;

const INITIAL_LIMIT: usize = 10_000;

/// Host coverage reported by Orca for one listing page.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CatalogScope {
    /// `false` means the server predates host-scope reporting, so an empty
    /// omitted list must not be interpreted as global coverage.
    pub reported: bool,
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
    pub unverifiable_scope_host_ids: Vec<String>,
    /// Whether the rows came from a successful Orca catalog query. `false`
    /// identifies the Git fallback (or an unavailable CLI), even when it has
    /// no rows; this keeps an empty authoritative catalog distinct from a
    /// missing catalog in the TUI.
    pub loaded_from_orca: bool,
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
    /// Paired runtime through which this row was loaded. This disambiguates
    /// identical worktree ids (and identical SSH target ids) returned by two
    /// different Orca servers.
    pub catalog_source_host_id: Option<String>,
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
        let display_host = match (
            self.catalog_source_host_id.as_deref(),
            self.host_id.as_deref(),
        ) {
            (Some(source), Some(host)) if host != "local" && host != source => {
                Some(format!("{source} / {host}"))
            }
            (Some(source), _) => Some(source.to_owned()),
            (None, Some(host)) if host != "local" => Some(host.to_owned()),
            _ => None,
        };
        if let Some(host) = display_host {
            label.push_str(" @ ");
            label.push_str(&host);
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
        let local_host = self.is_on_local_host();
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
        self.is_on_local_host() && self.path.is_dir()
    }

    /// Whether the workspace belongs to the Orca runtime on this machine.
    #[must_use]
    pub fn is_on_local_host(&self) -> bool {
        self.catalog_source_host_id.is_none()
            && self
                .host_id
                .as_deref()
                .map(|host| host == "local")
                .unwrap_or(true)
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

    let executable = std::env::var_os("ORCA_CLI").unwrap_or_else(|| "orca".into());
    list_all_with_executable(&executable)
}

fn list_all_with_executable(executable: &OsStr) -> Result<OrcaWorkspaceCatalog> {
    let mut workspaces = Vec::new();
    let mut unresolved = Vec::new();
    let mut unverifiable = Vec::new();
    let mut visited = HashSet::new();
    let mut pending = Vec::new();
    let mut successful_queries = 0usize;
    let mut initial_error = None;

    match query_complete(executable, None) {
        Ok((rows, scope)) => {
            successful_queries += 1;
            workspaces.extend(rows);
            if scope.reported {
                pending.extend(scope.omitted_host_ids);
            } else {
                unverifiable.push("local".to_owned());
            }
        }
        Err(error) => {
            unresolved.push("local".to_owned());
            initial_error = Some(error);
        }
    }

    // Match Orca's desktop all-host loading rule: every paired environment is
    // a catalog source even when the local runtime has not recorded its host
    // id in `omittedHostIds` yet.
    match paired_environment_ids(executable) {
        Ok(environment_ids) => pending.extend(
            environment_ids
                .into_iter()
                .map(|environment_id| format!("runtime:{environment_id}")),
        ),
        Err(_) => unverifiable.push("paired-environment-list".to_owned()),
    }
    pending.sort();
    pending.dedup();

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
        match query_complete(executable, Some(environment_id)) {
            Ok((rows, scope)) => {
                successful_queries += 1;
                workspaces.extend(rows);
                if scope.reported {
                    // An omitted host reported by a paired server is relative
                    // to that server's own host registry. The local CLI cannot
                    // safely reinterpret it as one of this machine's selectors.
                    unresolved.extend(scope.omitted_host_ids);
                } else {
                    unverifiable.push(host_id);
                }
            }
            Err(_) => unresolved.push(host_id),
        }
    }

    if successful_queries == 0 {
        return Err(initial_error
            .unwrap_or_else(|| anyhow::anyhow!("no reachable Orca workspace catalog source")));
    }

    // Orca's public worktree id is `<repo-id>::<path>`, which is not globally
    // unique: the same repository/path can exist on more than one execution
    // host. Deduplicate only repeated rows from the same host query, while
    // preserving distinct copies on different hosts.
    let mut seen = HashSet::new();
    workspaces.retain(|workspace| {
        seen.insert((
            workspace.catalog_source_host_id.clone().unwrap_or_default(),
            workspace.host_id.clone().unwrap_or_default(),
            workspace.id.clone(),
        ))
    });
    unresolved.sort();
    unresolved.dedup();
    unverifiable.sort();
    unverifiable.dedup();
    Ok(OrcaWorkspaceCatalog {
        workspaces,
        unresolved_host_ids: unresolved,
        unverifiable_scope_host_ids: unverifiable,
        loaded_from_orca: true,
    })
}

fn paired_environment_ids(executable: &OsStr) -> Result<HashSet<String>> {
    let output = Command::new(executable)
        .args(["environment", "list", "--json"])
        .output()
        .with_context(|| {
            format!(
                "failed to run {} environment list",
                executable.to_string_lossy()
            )
        })?;
    if !output.status.success() {
        bail!("{} environment list failed", executable.to_string_lossy());
    }
    let root: Value =
        serde_json::from_slice(&output.stdout).context("parsing Orca environment list JSON")?;
    if root.get("ok").and_then(Value::as_bool) == Some(false) {
        bail!("Orca environment list rejected");
    }
    let environments = root
        .get("result")
        .and_then(|result| result.get("environments"))
        .and_then(Value::as_array)
        .context("Orca environment response has no environments array")?;
    let mut ids = HashSet::with_capacity(environments.len());
    for (index, environment) in environments.iter().enumerate() {
        let id = environment
            .get("id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .with_context(|| format!("Orca environment row {index} has no id"))?;
        ids.insert(id.to_owned());
    }
    Ok(ids)
}

fn query_complete(
    executable: &OsStr,
    environment: Option<&str>,
) -> Result<(Vec<OrcaWorkspace>, CatalogScope)> {
    let mut limit = INITIAL_LIMIT;
    loop {
        let (mut workspaces, total_count, truncated, mut scope) =
            query(executable, limit, environment)?;
        let complete = !truncated
            && total_count
                .map(|total| workspaces.len() >= total)
                .unwrap_or(true);
        if complete {
            if let Some(environment_id) = environment {
                qualify_remote_catalog(&mut workspaces, &mut scope, environment_id);
            }
            return Ok((workspaces, scope));
        }
        let next_limit = total_count
            .filter(|total| *total > limit)
            .or_else(|| limit.checked_mul(10))
            .context("Orca workspace catalog limit overflowed")?;
        if next_limit <= limit {
            bail!(
                "Orca workspace catalog is truncated (received {}, total {:?})",
                workspaces.len(),
                total_count
            );
        }
        limit = next_limit;
    }
}

/// A response routed with `--environment` is produced by the selected Orca
/// server. That server calls its own machine `local`, but from this process'
/// point of view those rows belong to `runtime:<environment-id>`. Rebase that
/// local spelling before merging catalogs so a remote copy of
/// `repo::/same/path` cannot collide with the local copy.
fn qualify_remote_catalog(
    workspaces: &mut [OrcaWorkspace],
    scope: &mut CatalogScope,
    environment_id: &str,
) {
    let remote_host = format!("runtime:{environment_id}");
    for workspace in workspaces {
        workspace.catalog_source_host_id = Some(remote_host.clone());
        if workspace
            .host_id
            .as_deref()
            .map(|host| host == "local")
            .unwrap_or(true)
        {
            workspace.host_id = Some(remote_host.clone());
        }
    }
    for host_id in &mut scope.covered_host_ids {
        *host_id = qualify_remote_scope_host(host_id, &remote_host);
    }
    for host_id in &mut scope.omitted_host_ids {
        *host_id = qualify_remote_scope_host(host_id, &remote_host);
    }
}

fn qualify_remote_scope_host(host_id: &str, remote_host: &str) -> String {
    if host_id == "local" || host_id == remote_host {
        remote_host.to_owned()
    } else {
        format!("{remote_host} / {host_id}")
    }
}

fn query(
    executable: &OsStr,
    limit: usize,
    environment: Option<&str>,
) -> Result<(Vec<OrcaWorkspace>, Option<usize>, bool, CatalogScope)> {
    let mut command = Command::new(executable);
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
        .map(usize::try_from)
        .transpose()
        .context("Orca workspace totalCount exceeds this platform's usize")?;
    let truncated = result
        .get("truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let scope = result
        .get("hostScope")
        .and_then(|scope| {
            Some(CatalogScope {
                reported: true,
                covered_host_ids: parse_host_id_array(scope.get("hostIds")?)?,
                omitted_host_ids: parse_host_id_array(scope.get("omittedHostIds")?)?,
            })
        })
        .unwrap_or_default();
    Ok((workspaces, total_count, truncated, scope))
}

fn parse_host_id_array(value: &Value) -> Option<Vec<String>> {
    value
        .as_array()?
        .iter()
        .map(|host| {
            host.as_str()
                .map(str::trim)
                .filter(|host| !host.is_empty())
                .map(str::to_owned)
        })
        .collect()
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
        catalog_source_host_id: None,
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

    #[cfg(unix)]
    fn install_fake_orca(script: &str, suffix: &str) -> (PathBuf, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        use std::time::{SystemTime, UNIX_EPOCH};

        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "orca-tui-catalog-{suffix}-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir(&dir).expect("create test directory");
        let executable = dir.join("fake-orca");
        std::fs::write(&executable, script).expect("write fake Orca");
        let mut permissions = std::fs::metadata(&executable)
            .expect("fake Orca metadata")
            .permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&executable, permissions).expect("make fake Orca executable");
        (dir, executable)
    }

    #[cfg(unix)]
    #[test]
    fn catalog_queries_every_paired_environment_and_keeps_same_ids() {
        let script = r##"#!/bin/sh
case "$*" in
  "environment list --json")
    printf '%s\n' '{"ok":true,"result":{"environments":[{"id":"env-a"},{"id":"env-b"}]}}'
    ;;
  *"--environment env-a"*)
    printf '%s\n' '{"ok":true,"result":{"worktrees":[{"id":"repo::/same","path":"/same","branch":"main","displayName":"env-a","repoId":"repo","hostId":"local"}],"totalCount":1,"truncated":false,"hostScope":{"hostIds":["local"],"omittedHostIds":[]}}}'
    ;;
  *"--environment env-b"*)
    printf '%s\n' '{"ok":true,"result":{"worktrees":[{"id":"repo::/same","path":"/same","branch":"main","displayName":"env-b","repoId":"repo","hostId":"local"}],"totalCount":1,"truncated":false}}'
    ;;
  "worktree list --json --limit 10000")
    printf '%s\n' '{"ok":true,"result":{"worktrees":[{"id":"repo::/same","path":"/same","branch":"main","displayName":"local","repoId":"repo","hostId":"local"}],"totalCount":1,"truncated":false,"hostScope":{"hostIds":["local"],"omittedHostIds":[]}}}'
    ;;
  *) exit 2 ;;
esac
"##;
        let (dir, executable) = install_fake_orca(script, "all-hosts");

        let catalog = list_all_with_executable(executable.as_os_str()).expect("load all hosts");

        assert_eq!(catalog.workspaces.len(), 3);
        assert_eq!(
            catalog
                .workspaces
                .iter()
                .map(|workspace| workspace.catalog_source_host_id.as_deref())
                .collect::<Vec<_>>(),
            vec![None, Some("runtime:env-b"), Some("runtime:env-a")]
        );
        assert!(catalog.unresolved_host_ids.is_empty());
        assert_eq!(catalog.unverifiable_scope_host_ids, vec!["runtime:env-b"]);

        std::fs::remove_dir_all(&dir).expect("remove test directory");
    }

    #[cfg(unix)]
    #[test]
    fn catalog_retries_at_reported_total_without_a_fixed_ceiling() {
        let script = r##"#!/bin/sh
case "$*" in
  "environment list --json")
    printf '%s\n' '{"ok":true,"result":{"environments":[]}}'
    ;;
  "worktree list --json --limit 10000")
    printf '%s\n' '{"ok":true,"result":{"worktrees":[{"id":"repo::/first","path":"/first","displayName":"first","repoId":"repo","hostId":"local"}],"totalCount":250000,"truncated":true,"hostScope":{"hostIds":["local"],"omittedHostIds":[]}}}'
    ;;
  "worktree list --json --limit 250000")
    printf '%s\n' '{"ok":true,"result":{"worktrees":[{"id":"repo::/first","path":"/first","displayName":"first","repoId":"repo","hostId":"local"},{"id":"repo::/last","path":"/last","displayName":"last","repoId":"repo","hostId":"local"}],"totalCount":2,"truncated":false,"hostScope":{"hostIds":["local"],"omittedHostIds":[]}}}'
    ;;
  *) exit 2 ;;
esac
"##;
        let (dir, executable) = install_fake_orca(script, "pagination");

        let catalog = list_all_with_executable(executable.as_os_str()).expect("load full page");

        assert_eq!(catalog.workspaces.len(), 2);
        assert_eq!(catalog.workspaces[0].display_name, "first");
        assert_eq!(catalog.workspaces[1].display_name, "last");
        assert!(catalog.unresolved_host_ids.is_empty());
        assert!(catalog.unverifiable_scope_host_ids.is_empty());

        std::fs::remove_dir_all(&dir).expect("remove test directory");
    }

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
        assert!(scope.reported);
        assert_eq!(scope.covered_host_ids, vec!["local"]);
        assert_eq!(scope.omitted_host_ids, vec!["runtime:env-1"]);
    }

    #[test]
    fn catalog_without_host_scope_is_explicitly_unverifiable() {
        let json = br#"{
          "ok": true,
          "result": {"worktrees": [], "totalCount": 0, "truncated": false}
        }"#;

        let (_, _, _, scope) = parse_catalog_details(json).expect("parse old catalog");

        assert!(!scope.reported);
        assert!(scope.covered_host_ids.is_empty());
        assert!(scope.omitted_host_ids.is_empty());

        let null_scope = br#"{
          "ok": true,
          "result": {
            "worktrees": [],
            "totalCount": 0,
            "truncated": false,
            "hostScope": null
          }
        }"#;
        let (_, _, _, scope) = parse_catalog_details(null_scope).expect("parse null scope");
        assert!(!scope.reported);

        let partial_scope = br#"{
          "ok": true,
          "result": {
            "worktrees": [],
            "totalCount": 0,
            "truncated": false,
            "hostScope": {"hostIds": ["local"]}
          }
        }"#;
        let (_, _, _, scope) = parse_catalog_details(partial_scope).expect("parse partial scope");
        assert!(!scope.reported);
    }

    #[test]
    fn remote_catalog_rebases_local_rows_and_scope() {
        let mut rows = vec![OrcaWorkspace {
            id: "repo::/same/path".to_owned(),
            path: PathBuf::from("/same/path"),
            branch: "main".to_owned(),
            display_name: "main".to_owned(),
            repo_id: "repo".to_owned(),
            project_id: None,
            host_id: Some("local".to_owned()),
            catalog_source_host_id: None,
            is_archived: false,
            workspace_status: None,
            is_main_worktree: true,
        }];
        let mut scope = CatalogScope {
            reported: true,
            covered_host_ids: vec!["local".to_owned()],
            omitted_host_ids: vec!["ssh:box".to_owned(), "local".to_owned()],
        };

        qualify_remote_catalog(&mut rows, &mut scope, "env-1");

        assert_eq!(rows[0].host_id.as_deref(), Some("runtime:env-1"));
        assert_eq!(
            rows[0].catalog_source_host_id.as_deref(),
            Some("runtime:env-1")
        );
        assert_eq!(scope.covered_host_ids, vec!["runtime:env-1"]);
        assert_eq!(
            scope.omitted_host_ids,
            vec!["runtime:env-1 / ssh:box", "runtime:env-1"]
        );
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
            catalog_source_host_id: None,
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
            catalog_source_host_id: None,
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
