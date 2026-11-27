use crate::error::{CaissonError, Result};
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, Serialize)]
pub struct GitRepoContext {
    pub main_repo_root: PathBuf,
    pub git_common_dir: PathBuf,
    pub repo_name: String,
    pub repo_id: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct GitWorktreeInfo {
    pub path: PathBuf,
    pub head: Option<String>,
    pub branch: Option<String>,
    pub detached: bool,
    pub bare: bool,
    pub prunable: bool,
    pub locked: bool,
}

#[derive(Debug, Clone, Serialize, Default, PartialEq, Eq)]
pub struct WorktreeGitState {
    pub dirty: bool,
    pub unpushed: bool,
    pub inspection_failed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inspection_error: Option<String>,
}

/// Deterministic 64-bit FNV-1a hash (first 8 hex chars) for collision-free repo IDs.
fn short_path_hash(path: &Path) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in path.to_string_lossy().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{:08x}", (hash >> 32) as u32 ^ (hash as u32))
}

/// Sanitize a branch or directory name into a clean path-safe slug (`[a-zA-Z0-9._-]`).
pub fn sanitize_slug(raw: &str) -> Result<String> {
    let mut out = String::with_capacity(raw.len());
    let mut prev_dash = false;
    for ch in raw.trim().chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.') {
            out.push(ch);
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    let cleaned = out.trim_matches(|c| c == '-' || c == '.').to_string();
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        return Err(CaissonError::Message(format!(
            "cannot derive a valid worktree slug from '{raw}'"
        )));
    }
    if cleaned.len() > 120 {
        return Ok(cleaned[..120].trim_end_matches('-').to_string());
    }
    Ok(cleaned)
}

fn run_git(cwd: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git").arg("-C").arg(cwd).args(args).output()?;
    if !output.status.success() {
        return Err(CaissonError::CommandFailed {
            cmd: format!("git -C {} {}", cwd.display(), args.join(" ")),
            code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Discover the top-level working tree directory for `start_dir` (canonicalized).
pub fn worktree_toplevel(start_dir: &Path) -> Result<PathBuf> {
    let raw = run_git(start_dir, &["rev-parse", "--show-toplevel"])?;
    let path = PathBuf::from(raw);
    Ok(fs::canonicalize(&path).unwrap_or(path))
}

/// Discover the primary git repository root and stable `repo_id` from any path inside the repo
/// (works identically from the main checkout or any linked worktree).
pub fn discover_repo(start_dir: &Path) -> Result<GitRepoContext> {
    let common_raw = run_git(start_dir, &["rev-parse", "--git-common-dir"])?;
    let common_path = {
        let p = PathBuf::from(&common_raw);
        if p.is_absolute() {
            p
        } else {
            start_dir.join(p)
        }
    };
    let git_common_dir = fs::canonicalize(&common_path).unwrap_or(common_path);

    let main_repo_root = if git_common_dir.file_name().and_then(|n| n.to_str()) == Some(".git") {
        git_common_dir
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| {
                CaissonError::Message(format!(
                    "could not determine repository root from {}",
                    git_common_dir.display()
                ))
            })?
    } else {
        // Bare repo fallback
        git_common_dir.clone()
    };

    let main_repo_root = fs::canonicalize(&main_repo_root).unwrap_or(main_repo_root);
    let raw_name = main_repo_root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("repo");
    let repo_name = sanitize_slug(raw_name)
        .unwrap_or_else(|_| "repo".to_string())
        .to_ascii_lowercase();
    let repo_id = format!("{repo_name}-{}", short_path_hash(&main_repo_root));

    Ok(GitRepoContext {
        main_repo_root,
        git_common_dir,
        repo_name,
        repo_id,
    })
}

/// Parse `git worktree list --porcelain` output into structured entries.
pub fn parse_worktree_porcelain(output: &str) -> Vec<GitWorktreeInfo> {
    let mut list = Vec::new();
    let mut current: Option<GitWorktreeInfo> = None;

    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            if let Some(wt) = current.take() {
                list.push(wt);
            }
            continue;
        }
        if let Some(path_str) = trimmed.strip_prefix("worktree ") {
            if let Some(wt) = current.take() {
                list.push(wt);
            }
            current = Some(GitWorktreeInfo {
                path: PathBuf::from(path_str),
                head: None,
                branch: None,
                detached: false,
                bare: false,
                prunable: false,
                locked: false,
            });
        } else if let Some(ref mut wt) = current {
            if let Some(head) = trimmed.strip_prefix("HEAD ") {
                wt.head = Some(head.to_string());
            } else if let Some(branch) = trimmed.strip_prefix("branch ") {
                let short = branch.strip_prefix("refs/heads/").unwrap_or(branch);
                wt.branch = Some(short.to_string());
            } else if trimmed == "detached" {
                wt.detached = true;
            } else if trimmed == "bare" {
                wt.bare = true;
            } else if trimmed.starts_with("prunable") {
                wt.prunable = true;
            } else if trimmed.starts_with("locked") {
                wt.locked = true;
            }
        }
    }
    if let Some(wt) = current {
        list.push(wt);
    }
    list
}

pub fn list_git_worktrees(repo_root: &Path) -> Result<Vec<GitWorktreeInfo>> {
    let out = run_git(repo_root, &["worktree", "list", "--porcelain"])?;
    Ok(parse_worktree_porcelain(&out))
}

pub fn branch_exists(repo_root: &Path, branch: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args([
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Create a new git worktree at `target_path` for `branch` (creating the branch if it does not exist).
pub fn add_git_worktree(
    repo_root: &Path,
    target_path: &Path,
    branch: &str,
    from_ref: Option<&str>,
) -> Result<()> {
    let target_str = target_path.to_string_lossy().into_owned();
    let exists = branch_exists(repo_root, branch);

    let mut args: Vec<&str> = vec!["worktree", "add"];
    if exists {
        if let Some(base) = from_ref {
            return Err(CaissonError::Message(format!(
                "local branch '{branch}' already exists; omit `--from {base}` to check it out, or choose a new branch name"
            )));
        }
        args.push(&target_str);
        args.push(branch);
    } else {
        args.push("-b");
        args.push(branch);
        args.push(&target_str);
        if let Some(base) = from_ref {
            args.push(base);
        }
    }

    let _ = run_git(repo_root, &args)?;
    Ok(())
}

/// Deregister pruned/deleted worktree entries via `git worktree prune`.
pub fn prune_git_worktrees(repo_root: &Path) -> Result<()> {
    let _ = run_git(repo_root, &["worktree", "prune"])?;
    Ok(())
}

/// Resolve the `.git/worktrees/<name>` admin directory for a linked worktree by reading `<wt>/.git`.
pub fn resolve_worktree_admin_dir(wt_path: &Path) -> Option<PathBuf> {
    let dot_git = wt_path.join(".git");
    if dot_git.is_dir() {
        return Some(dot_git);
    }
    let content = fs::read_to_string(&dot_git).ok()?;
    let gitdir_raw = content.lines().next()?.strip_prefix("gitdir:")?.trim();
    let p = PathBuf::from(gitdir_raw);
    let resolved = if p.is_absolute() { p } else { wt_path.join(p) };
    Some(fs::canonicalize(&resolved).unwrap_or(resolved))
}

/// Given a linked worktree path, recover the owning main repository root path (if accessible).
pub fn owner_repo_root_from_worktree(wt_path: &Path) -> Option<PathBuf> {
    let admin = resolve_worktree_admin_dir(wt_path)?;
    // Typical layout: <main_repo>/.git/worktrees/<name>
    let worktrees_dir = admin.parent()?;
    if worktrees_dir.file_name().and_then(|s| s.to_str()) == Some("worktrees") {
        let dot_git = worktrees_dir.parent()?;
        if dot_git.file_name().and_then(|s| s.to_str()) == Some(".git") {
            return dot_git.parent().map(Path::to_path_buf);
        }
        return Some(dot_git.to_path_buf());
    }
    None
}

/// Read the currently checked-out branch name (or detached HEAD short SHA) for a worktree.
/// Fast path reads `<admin_dir>/HEAD` directly without spawning a git subprocess.
pub fn worktree_current_branch(wt_path: &Path) -> Option<String> {
    if let Some(admin) = resolve_worktree_admin_dir(wt_path) {
        if let Ok(head) = fs::read_to_string(admin.join("HEAD")) {
            let trimmed = head.trim();
            if let Some(branch) = trimmed.strip_prefix("ref: refs/heads/") {
                if !branch.is_empty() {
                    return Some(branch.to_string());
                }
            } else if trimmed.len() >= 7 && trimmed.chars().all(|c| c.is_ascii_hexdigit()) {
                return Some(format!("HEAD@{}", &trimmed[..7]));
            }
        }
    }
    if let Ok(branch) = run_git(wt_path, &["symbolic-ref", "--short", "HEAD"]) {
        if !branch.is_empty() {
            return Some(branch);
        }
    }
    run_git(wt_path, &["rev-parse", "--short", "HEAD"])
        .ok()
        .filter(|s| !s.is_empty())
        .map(|sha| format!("HEAD@{sha}"))
}

/// Detect whether a worktree has uncommitted changes (`dirty`), unpushed commits (`unpushed`),
/// or failed git inspection (`inspection_failed`, treated as protected unless overridden).
pub fn inspect_worktree_git_state(
    wt_path: &Path,
    main_repo_root: Option<&Path>,
) -> WorktreeGitState {
    if !wt_path.exists() {
        return WorktreeGitState {
            dirty: false,
            unpushed: false,
            inspection_failed: true,
            inspection_error: Some("worktree path does not exist".to_string()),
        };
    }

    let dirty = match run_git(wt_path, &["status", "--porcelain"]) {
        Ok(out) => !out.trim().is_empty(),
        Err(e) => {
            return WorktreeGitState {
                dirty: false,
                unpushed: false,
                inspection_failed: true,
                inspection_error: Some(format!("git status failed: {e}")),
            };
        }
    };

    match detect_unpushed(wt_path, main_repo_root) {
        Ok(unpushed) => WorktreeGitState {
            dirty,
            unpushed,
            inspection_failed: false,
            inspection_error: None,
        },
        Err(e) => WorktreeGitState {
            dirty,
            unpushed: false,
            inspection_failed: true,
            inspection_error: Some(e),
        },
    }
}

fn detect_unpushed(
    wt_path: &Path,
    main_repo_root: Option<&Path>,
) -> std::result::Result<bool, String> {
    // 1. If an upstream tracking branch is configured, check commits ahead of `@{u}`.
    if run_git(
        wt_path,
        &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"],
    )
    .is_ok()
    {
        let out = run_git(wt_path, &["rev-list", "--count", "@{u}..HEAD"])
            .map_err(|e| format!("git rev-list @{{u}}..HEAD failed: {e}"))?;
        let count = out
            .parse::<u64>()
            .map_err(|_| format!("invalid rev-list count '{out}'"))?;
        return Ok(count > 0);
    }

    // 2. No upstream configured: if any remote exists, check commits unreachable from `--remotes`.
    let remotes = run_git(wt_path, &["remote"]).map_err(|e| format!("git remote failed: {e}"))?;
    if !remotes.trim().is_empty() {
        let out = run_git(
            wt_path,
            &["rev-list", "--count", "HEAD", "--not", "--remotes"],
        )
        .map_err(|e| format!("git rev-list --not --remotes failed: {e}"))?;
        let count = out
            .parse::<u64>()
            .map_err(|_| format!("invalid rev-list count '{out}'"))?;
        return Ok(count > 0);
    }

    // 3. Local-only repository (no remotes): compare HEAD against the primary checkout's branch.
    let Some(root) = main_repo_root else {
        return Err("owning main repository root could not be resolved".to_string());
    };
    if !root.exists() {
        return Err(format!(
            "owning main repository '{}' does not exist",
            root.display()
        ));
    }
    let base_branch = run_git(root, &["symbolic-ref", "--short", "HEAD"])
        .map_err(|e| format!("could not determine primary branch in main repo: {e}"))?;
    if base_branch.is_empty() {
        return Err("primary branch name is empty".to_string());
    }

    let cur_branch = run_git(wt_path, &["symbolic-ref", "--short", "HEAD"]).ok();
    if cur_branch.as_deref() == Some(base_branch.as_str()) {
        return Ok(false);
    }
    let out = run_git(
        wt_path,
        &["rev-list", "--count", "HEAD", "--not", &base_branch],
    )
    .map_err(|e| format!("git rev-list --not {base_branch} failed: {e}"))?;
    let count = out
        .parse::<u64>()
        .map_err(|_| format!("invalid rev-list count '{out}'"))?;
    Ok(count > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_branch_names_to_safe_slugs() {
        assert_eq!(sanitize_slug("feat/auth-v2").unwrap(), "feat-auth-v2");
        assert_eq!(sanitize_slug("fix/bug 123!").unwrap(), "fix-bug-123");
        assert_eq!(sanitize_slug("release_1.2.3").unwrap(), "release_1.2.3");
        assert!(sanitize_slug("///").is_err());
        assert!(sanitize_slug("..").is_err());
    }

    #[test]
    fn parses_git_worktree_porcelain() {
        let raw = "\
worktree /tmp/repo
HEAD 1111111111111111111111111111111111111111
branch refs/heads/main

worktree /Volumes/caisson/worktrees/repo-1234/feat-x
HEAD 2222222222222222222222222222222222222222
branch refs/heads/feat/x

worktree /Volumes/caisson/worktrees/repo-1234/detached-1
HEAD 3333333333333333333333333333333333333333
detached
prunable gitdir file points to non-existent location
";
        let wts = parse_worktree_porcelain(raw);
        assert_eq!(wts.len(), 3);
        assert_eq!(wts[0].branch.as_deref(), Some("main"));
        assert_eq!(wts[1].branch.as_deref(), Some("feat/x"));
        assert!(wts[2].detached);
        assert!(wts[2].prunable);
    }
}
