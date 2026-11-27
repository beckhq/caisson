use crate::apfs::clonefile::clone_path;
use crate::apfs::shield::inspect_shields;
use crate::apfs::statfs::{same_volume, statfs_path};
use crate::config::{CaissonConfig, RepoConfig};
use crate::error::{CaissonError, Result};
use crate::units::{format_age, parse_duration};
use crate::worktree::activity::{compute_last_activity, inactive_duration};
use crate::worktree::git::{
    add_git_worktree, discover_repo, inspect_worktree_git_state, list_git_worktrees,
    owner_repo_root_from_worktree, prune_git_worktrees, sanitize_slug, worktree_current_branch,
    worktree_toplevel, GitRepoContext,
};
use crate::worktree::quotas::{evaluate_add_quotas, measure_root_usage, QuotaSnapshot};
use crate::worktree::trash::{
    allocated_dir_bytes, drain_trash, ensure_safe_trash_dir, trash_worktree, TrashDrainReport,
    TrashResult,
};
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

#[derive(Debug, Clone)]
pub struct WtAddOptions {
    pub branch: String,
    pub from_ref: Option<String>,
    pub name: Option<String>,
    pub repo_path: Option<PathBuf>,
    pub no_cow: bool,
    pub force: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct CowCloneOutcome {
    pub entry: String,
    pub source: Option<String>,
    pub status: String,
    pub detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lockfile_match: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WtAddReport {
    pub repo_id: String,
    pub main_repo_root: String,
    pub branch: String,
    pub slug: String,
    pub path: String,
    pub cow_clones: Vec<CowCloneOutcome>,
    pub post_create_ran: bool,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorktreeEntry {
    pub repo_id: String,
    pub slug: String,
    pub path: String,
    pub main_repo_root: Option<String>,
    pub branch: Option<String>,
    pub last_activity_utc: Option<String>,
    pub inactive_secs: u64,
    pub inactive_human: String,
    pub dirty: bool,
    pub unpushed: bool,
    pub inspection_failed: bool,
    pub disk_bytes: u64,
    pub orphaned: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct WtRmReport {
    pub slug: String,
    pub path: String,
    pub trash_path: String,
    pub rename_ms: f64,
    pub cleaned_sync: bool,
    pub pruned_repo: Option<String>,
}

#[derive(Debug, Clone)]
pub struct WtPruneOptions {
    pub inactive: Option<String>,
    pub include_dirty: bool,
    pub include_unpushed: bool,
    pub all: bool,
    pub repo_path: Option<PathBuf>,
    pub apply: bool,
    pub sync: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct PruneSkipped {
    pub slug: String,
    pub path: String,
    pub inactive_human: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct WtPruneReport {
    pub dry_run: bool,
    pub inactive_threshold: String,
    pub pruned: Vec<WorktreeEntry>,
    pub skipped_protected: Vec<PruneSkipped>,
    pub active_count: usize,
    pub reclaimed_bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct GcReport {
    pub trash: TrashDrainReport,
    pub pruned_repos: Vec<String>,
    pub removed_empty_repo_dirs: usize,
}

#[derive(Debug, Clone)]
pub struct WtHydrateOptions {
    pub path: Option<PathBuf>,
    pub dry_run: bool,
    pub run_post_create: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct WtHydrateReport {
    pub repo_id: String,
    pub main_repo_root: String,
    pub worktree_path: String,
    pub branch: Option<String>,
    pub volume_mount: Option<String>,
    pub volume_shielded: bool,
    pub auto_detected: bool,
    pub dry_run: bool,
    pub cow_clones: Vec<CowCloneOutcome>,
    pub post_create_ran: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub post_create_command: Option<String>,
}

fn ensure_non_symlink_dir(path: &Path, label: &str) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            if meta.file_type().is_symlink() || !meta.is_dir() {
                return Err(CaissonError::Message(format!(
                    "refusing to use symlink or non-directory for {label}: {}",
                    path.display()
                )));
            }
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(path)?;
            Ok(())
        }
        Err(e) => Err(CaissonError::Io(e)),
    }
}

fn ensure_caisson_root_ready(caisson_root: &Path) -> Result<()> {
    if !caisson_root.exists() {
        return Err(CaissonError::Message(format!(
            "Caisson root '{}' does not exist. Run `sudo caisson init` once to create `/Volumes/caisson`, or set `CAISSON_ROOT`.",
            caisson_root.display()
        )));
    }
    ensure_non_symlink_dir(&caisson_root.join("worktrees"), "worktrees root")?;
    let _ = ensure_safe_trash_dir(caisson_root)?;
    Ok(())
}

/// Shared containment validation & atomic trashing routine used by BOTH `wt rm` and `wt prune`.
/// Rejects symlinks at every managed component (`worktrees/`, `<repo-id>/`, `<slug>/`) and
/// verifies canonical containment inside `<caisson_root>/worktrees/`.
fn validate_and_trash_managed_worktree(
    caisson_root: &Path,
    wt_path: &Path,
    sync: bool,
) -> Result<TrashResult> {
    let worktrees_base = caisson_root.join("worktrees");
    let base_meta = fs::symlink_metadata(&worktrees_base)?;
    if base_meta.file_type().is_symlink() || !base_meta.is_dir() {
        return Err(CaissonError::Message(format!(
            "refusing to operate on symlinked worktrees root: {}",
            worktrees_base.display()
        )));
    }

    let wt_meta = fs::symlink_metadata(wt_path)?;
    if wt_meta.file_type().is_symlink() || !wt_meta.is_dir() {
        return Err(CaissonError::Message(format!(
            "refusing to remove symlink or non-directory worktree: {}",
            wt_path.display()
        )));
    }

    let repo_dir = wt_path.parent().ok_or_else(|| {
        CaissonError::Message(format!(
            "invalid managed worktree path: {}",
            wt_path.display()
        ))
    })?;
    let repo_meta = fs::symlink_metadata(repo_dir)?;
    if repo_meta.file_type().is_symlink() || !repo_meta.is_dir() {
        return Err(CaissonError::Message(format!(
            "refusing to remove worktree under symlinked repository directory: {}",
            repo_dir.display()
        )));
    }

    let canonical_base = fs::canonicalize(&worktrees_base)?;
    let canonical_repo = fs::canonicalize(repo_dir)?;
    let canonical_wt = fs::canonicalize(wt_path)?;

    if canonical_repo.parent() != Some(canonical_base.as_path())
        || canonical_wt.parent() != Some(canonical_repo.as_path())
        || !canonical_wt.starts_with(&canonical_base)
    {
        return Err(CaissonError::Message(format!(
            "refusing to remove path outside Caisson worktrees root: {}",
            canonical_wt.display()
        )));
    }

    trash_worktree(caisson_root, &canonical_wt, sync)
}

#[derive(Debug, Clone, Copy)]
struct ScanOptions {
    include_disk_bytes: bool,
    include_git_state: bool,
    include_activity: bool,
}

/// Scan managed worktrees under `<caisson_root>/worktrees/` with full disk, git, and activity data.
/// Never follows symlinked `worktrees/`, `<repo-id>/`, or `<slug>/` directories.
pub fn scan_managed_worktrees(
    caisson_root: &Path,
    repo_filter: Option<&str>,
    now: SystemTime,
) -> Result<Vec<WorktreeEntry>> {
    scan_managed_worktrees_with_options(
        caisson_root,
        repo_filter,
        now,
        ScanOptions {
            include_disk_bytes: true,
            include_git_state: true,
            include_activity: true,
        },
    )
}

fn scan_managed_worktrees_with_options(
    caisson_root: &Path,
    repo_filter: Option<&str>,
    now: SystemTime,
    opts: ScanOptions,
) -> Result<Vec<WorktreeEntry>> {
    let worktrees_base = caisson_root.join("worktrees");
    match fs::symlink_metadata(&worktrees_base) {
        Ok(meta) if !meta.file_type().is_symlink() && meta.is_dir() => {}
        Ok(_) => {
            return Err(CaissonError::Message(format!(
                "refusing to scan symlink or non-directory worktrees root: {}",
                worktrees_base.display()
            )));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(CaissonError::Io(e)),
    }

    let mut results = Vec::new();
    let repo_dirs: Vec<(String, PathBuf)> = if let Some(rid) = repo_filter {
        let p = worktrees_base.join(rid);
        match fs::symlink_metadata(&p) {
            Ok(meta) if !meta.file_type().is_symlink() && meta.is_dir() => {
                vec![(rid.to_string(), p)]
            }
            _ => Vec::new(),
        }
    } else {
        fs::read_dir(&worktrees_base)?
            .flatten()
            .filter_map(|e| {
                let p = e.path();
                let meta = fs::symlink_metadata(&p).ok()?;
                if !meta.file_type().is_symlink() && meta.is_dir() {
                    let id = e.file_name().to_string_lossy().into_owned();
                    Some((id, p))
                } else {
                    None
                }
            })
            .collect()
    };

    for (repo_id, repo_dir) in repo_dirs {
        let Ok(slug_entries) = fs::read_dir(&repo_dir) else {
            continue;
        };
        for entry in slug_entries.flatten() {
            let wt_path = entry.path();
            let Ok(wt_meta) = fs::symlink_metadata(&wt_path) else {
                continue;
            };
            if wt_meta.file_type().is_symlink() || !wt_meta.is_dir() {
                continue;
            }
            let slug = entry.file_name().to_string_lossy().into_owned();
            let owner_root = owner_repo_root_from_worktree(&wt_path);
            let orphaned = owner_root.as_ref().map(|r| !r.exists()).unwrap_or(true);
            let branch = worktree_current_branch(&wt_path);
            let git_state = if opts.include_git_state {
                inspect_worktree_git_state(&wt_path, owner_root.as_deref())
            } else {
                crate::worktree::git::WorktreeGitState::default()
            };

            let (last_activity_utc, inactive_secs, inactive_human) = if opts.include_activity {
                let last_activity = compute_last_activity(&wt_path);
                let inactive = last_activity
                    .map(|t| inactive_duration(t, now))
                    .unwrap_or_default();
                let utc = last_activity.map(|t| DateTime::<Utc>::from(t).to_rfc3339());
                (utc, inactive.as_secs(), format_age(inactive))
            } else {
                (None, 0, "0m".to_string())
            };
            let disk_bytes = if opts.include_disk_bytes {
                allocated_dir_bytes(&wt_path)
            } else {
                0
            };

            results.push(WorktreeEntry {
                repo_id: repo_id.clone(),
                slug,
                path: wt_path.display().to_string(),
                main_repo_root: owner_root.map(|p| p.display().to_string()),
                branch,
                last_activity_utc,
                inactive_secs,
                inactive_human,
                dirty: git_state.dirty,
                unpushed: git_state.unpushed,
                inspection_failed: git_state.inspection_failed,
                disk_bytes,
                orphaned,
            });
        }
    }

    // Most recently active first
    results.sort_by(|a, b| {
        a.inactive_secs
            .cmp(&b.inactive_secs)
            .then_with(|| a.repo_id.cmp(&b.repo_id))
            .then_with(|| a.slug.cmp(&b.slug))
    });
    Ok(results)
}

/// Create a new CoW-enabled git worktree on `<caisson_root>` with soft quota enforcement.
pub fn wt_add(config: &CaissonConfig, opts: &WtAddOptions) -> Result<WtAddReport> {
    let cwd = match opts.repo_path {
        Some(ref p) => p.clone(),
        None => std::env::current_dir()?,
    };
    let repo = discover_repo(&cwd)?;
    let caisson_root = config.resolve_root();
    ensure_caisson_root_ready(&caisson_root)?;

    let now = SystemTime::now();
    // Fast metadata-only scan: never walk `allocated_dir_bytes` or run `git status` on sibling
    // worktrees on the happy path (prevents O(N^2 * files) slowdown with large `node_modules`).
    let all_wts = scan_managed_worktrees_with_options(
        &caisson_root,
        None,
        now,
        ScanOptions {
            include_disk_bytes: false,
            include_git_state: false,
            include_activity: false,
        },
    )?;
    let repo_wts_count = all_wts.iter().filter(|w| w.repo_id == repo.repo_id).count();
    let (caisson_used_bytes, host_available_bytes) = measure_root_usage(&caisson_root)?;

    let quota_outcome = match evaluate_add_quotas(
        config,
        &QuotaSnapshot {
            repo_worktrees: repo_wts_count,
            total_worktrees: all_wts.len(),
            reclaimable_worktrees: 0,
            caisson_used_bytes,
            host_available_bytes,
        },
        opts.force,
    ) {
        Ok(outcome) => outcome,
        Err(CaissonError::QuotaExceeded { .. }) => {
            // Only compute exact reclaimable inactive count when a soft quota is actually hit.
            let full_wts = scan_managed_worktrees_with_options(
                &caisson_root,
                None,
                now,
                ScanOptions {
                    include_disk_bytes: false,
                    include_git_state: true,
                    include_activity: true,
                },
            )?;
            let default_inactive = config.default_inactive_duration()?;
            let reclaimable = full_wts
                .iter()
                .filter(|w| {
                    w.inactive_secs >= default_inactive.as_secs()
                        && !w.inspection_failed
                        && (!config.prune.protect_dirty || !w.dirty)
                        && (!config.prune.protect_unpushed || !w.unpushed)
                })
                .count();
            evaluate_add_quotas(
                config,
                &QuotaSnapshot {
                    repo_worktrees: repo_wts_count,
                    total_worktrees: full_wts.len(),
                    reclaimable_worktrees: reclaimable,
                    caisson_used_bytes,
                    host_available_bytes,
                },
                opts.force,
            )?
        }
        Err(e) => return Err(e),
    };

    let slug = match opts.name {
        Some(ref explicit) => sanitize_slug(explicit)?,
        None => sanitize_slug(&opts.branch)?,
    };

    let repo_wt_parent = caisson_root.join("worktrees").join(&repo.repo_id);
    ensure_non_symlink_dir(&repo_wt_parent, "repository worktree directory")?;
    let target_path = repo_wt_parent.join(&slug);
    if target_path.exists() || fs::symlink_metadata(&target_path).is_ok() {
        return Err(CaissonError::Message(format!(
            "worktree target '{}' already exists (choose a different `--name` or remove it with `caisson wt rm {slug}`)",
            target_path.display()
        )));
    }

    // Create git worktree
    add_git_worktree(
        &repo.main_repo_root,
        &target_path,
        &opts.branch,
        opts.from_ref.as_deref(),
    )?;

    // Apply `.caisson.toml` CoW clones and optional `post_create` hook
    let repo_cfg = RepoConfig::load_from_repo(&repo.main_repo_root)?;
    let mut cow_clones = Vec::new();

    if !opts.no_cow && !repo_cfg.worktree.cow_copy.is_empty() {
        let sibling_paths: Vec<PathBuf> = all_wts
            .iter()
            .filter(|w| w.repo_id == repo.repo_id)
            .map(|w| PathBuf::from(&w.path))
            .collect();

        for entry in &repo_cfg.worktree.cow_copy {
            let outcome =
                cow_clone_entry(&caisson_root, &repo, &sibling_paths, &target_path, entry);
            cow_clones.push(outcome);
        }
    }

    let mut post_create_ran = false;
    if let Some(ref hook) = repo_cfg.worktree.post_create {
        let trimmed = hook.trim();
        if !trimmed.is_empty() {
            let out = Command::new("/bin/sh")
                .args(["-c", trimmed])
                .current_dir(&target_path)
                .env("CAISSON_WORKTREE", &target_path)
                .env("CAISSON_REPO", &repo.main_repo_root)
                .env("CAISSON_BRANCH", &opts.branch)
                .output()?;
            if !out.status.success() {
                return Err(CaissonError::CommandFailed {
                    cmd: format!("post_create: {trimmed}"),
                    code: out.status.code(),
                    stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
                });
            }
            post_create_ran = true;
        }
    }

    Ok(WtAddReport {
        repo_id: repo.repo_id,
        main_repo_root: repo.main_repo_root.display().to_string(),
        branch: opts.branch.clone(),
        slug,
        path: target_path.display().to_string(),
        cow_clones,
        post_create_ran,
        warnings: quota_outcome.warnings,
    })
}

fn validate_relative_cow_entry(entry: &str) -> Result<PathBuf> {
    let p = Path::new(entry);
    if p.as_os_str().is_empty()
        || p.is_absolute()
        || p.components().any(|c| {
            matches!(
                c,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(CaissonError::Message(format!(
            "unsafe `cow_copy` path '{entry}': must be a non-empty path relative to the worktree root"
        )));
    }
    Ok(p.to_path_buf())
}

/// Verify that no existing ancestor component of `rel` under `base` is a symlink, and that
/// the resolved parent stays strictly inside `canonical(base)`.
fn ensure_no_symlink_escape(base: &Path, rel: &Path) -> Result<()> {
    let mut cur = base.to_path_buf();
    for comp in rel.components() {
        cur.push(comp);
        if let Ok(meta) = fs::symlink_metadata(&cur) {
            if meta.file_type().is_symlink() {
                return Err(CaissonError::Message(format!(
                    "refusing to traverse symlink component '{}' inside '{}'",
                    cur.display(),
                    base.display()
                )));
            }
        }
    }
    Ok(())
}

fn cow_clone_entry(
    caisson_root: &Path,
    repo: &GitRepoContext,
    siblings: &[PathBuf],
    target_wt: &Path,
    entry: &str,
) -> CowCloneOutcome {
    let mut candidate_roots: Vec<PathBuf> = vec![
        caisson_root.join("cache").join(&repo.repo_id),
        caisson_root.join("cache").join(&repo.repo_name),
    ];
    candidate_roots.extend(siblings.iter().cloned());

    let mut cross_volume_sources = Vec::new();
    if same_volume(&repo.main_repo_root, target_wt).unwrap_or(false) {
        candidate_roots.push(repo.main_repo_root.clone());
    } else {
        cross_volume_sources.push(repo.main_repo_root.clone());
    }

    let seed_hint = caisson_root.join("cache").join(&repo.repo_name);
    cow_clone_from_candidates(
        &candidate_roots,
        &cross_volume_sources,
        &seed_hint,
        target_wt,
        entry,
        false,
    )
}

/// Known lockfiles associated with standard dependency directories.
fn lockfiles_for_entry(entry: &str) -> &'static [&'static str] {
    let trimmed = entry.trim_matches('/');
    match trimmed {
        "node_modules" => &[
            "pnpm-lock.yaml",
            "package-lock.json",
            "yarn.lock",
            "bun.lock",
            "bun.lockb",
            "npm-shrinkwrap.json",
        ],
        ".venv" | "venv" => &[
            "uv.lock",
            "poetry.lock",
            "Pipfile.lock",
            "pdm.lock",
            "requirements.txt",
        ],
        _ => &[],
    }
}

/// Bounded, non-blocking, symlink-rejecting read of a regular file (max 16 MiB).
fn read_regular_file_nofollow_bounded(path: &Path) -> Option<Vec<u8>> {
    use std::ffi::CString;
    use std::io::Read;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;

    const MAX_LOCKFILE_BYTES: u64 = 16 * 1024 * 1024;
    let c_path = CString::new(path.as_os_str().as_bytes()).ok()?;
    let raw_fd = unsafe {
        libc::open(
            c_path.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if raw_fd < 0 {
        return None;
    }
    let fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd.as_raw_fd(), &mut st) } != 0 {
        return None;
    }
    if (st.st_mode & libc::S_IFMT) != libc::S_IFREG || (st.st_size as u64) > MAX_LOCKFILE_BYTES {
        return None;
    }
    let file = fs::File::from(fd);
    let mut buf = Vec::with_capacity(st.st_size.max(0) as usize);
    file.take(MAX_LOCKFILE_BYTES + 1)
        .read_to_end(&mut buf)
        .ok()?;
    if (buf.len() as u64) > MAX_LOCKFILE_BYTES {
        return None;
    }
    Some(buf)
}

/// Compare lockfile contents between `donor_root` and `target_wt` for standard dependency entries.
/// Returns `None` when the entry has no known lockfile present in `target_wt`, `Some(true)` on
/// exact byte-for-byte match (regular non-symlink files), and `Some(false)` otherwise.
fn check_lockfile_match(entry: &str, donor_root: &Path, target_wt: &Path) -> Option<bool> {
    let lockfiles = lockfiles_for_entry(entry);
    let target_lock = lockfiles.iter().find(|&&name| {
        fs::symlink_metadata(target_wt.join(name))
            .map(|m| !m.file_type().is_symlink() && m.is_file())
            .unwrap_or(false)
    })?;
    let target_bytes = read_regular_file_nofollow_bounded(&target_wt.join(target_lock))?;
    let Some(donor_bytes) = read_regular_file_nofollow_bounded(&donor_root.join(target_lock))
    else {
        return Some(false);
    };
    Some(donor_bytes == target_bytes)
}

/// Zero-config dependency auto-detection when a repository has no `.caisson.toml`.
/// Strictly targets `node_modules` (relocatable); `.venv` embeds absolute shebang paths so
/// Python virtualenvs remain explicit opt-in via `.caisson.toml`.
fn auto_detect_cow_entries(wt_root: &Path) -> Vec<String> {
    let mut entries = Vec::new();
    if wt_root.join("package.json").is_file() {
        entries.push("node_modules".to_string());
    }
    entries
}

/// Push a cache donor root under `base/rel_sub/name` only if no component is a symlink.
fn push_safe_cache_candidate(
    candidates: &mut Vec<PathBuf>,
    base: &Path,
    rel_sub: &str,
    name: &str,
) {
    let rel = Path::new(rel_sub).join(name);
    if ensure_no_symlink_escape(base, &rel).is_ok() {
        let full = base.join(rel);
        if fs::symlink_metadata(&full)
            .map(|m| !m.file_type().is_symlink() && m.is_dir())
            .unwrap_or(false)
        {
            candidates.push(full);
        }
    }
}

fn cow_clone_from_candidates(
    candidate_roots: &[PathBuf],
    cross_volume_roots: &[PathBuf],
    seed_cache_base: &Path,
    target_wt: &Path,
    entry: &str,
    dry_run: bool,
) -> CowCloneOutcome {
    let rel = match validate_relative_cow_entry(entry) {
        Ok(p) => p,
        Err(e) => {
            return CowCloneOutcome {
                entry: entry.to_string(),
                source: None,
                status: "skipped".to_string(),
                detail: Some(e.to_string()),
                lockfile_match: None,
            };
        }
    };

    // Guard destination against intermediate symlink escapes inside the target worktree.
    if let Err(e) = ensure_no_symlink_escape(target_wt, &rel) {
        return CowCloneOutcome {
            entry: entry.to_string(),
            source: None,
            status: "skipped".to_string(),
            detail: Some(e.to_string()),
            lockfile_match: None,
        };
    }

    let dst = target_wt.join(&rel);
    if fs::symlink_metadata(&dst).is_ok() {
        return CowCloneOutcome {
            entry: entry.to_string(),
            source: None,
            status: "skipped".to_string(),
            detail: Some("already exists in worktree".to_string()),
            lockfile_match: None,
        };
    }

    // Collect valid same-volume donors in priority order, then prefer an exact lockfile match
    // when available so agents can skip package-manager installs safely.
    let valid_donors: Vec<(&PathBuf, PathBuf, Option<bool>)> = candidate_roots
        .iter()
        .filter_map(|root| {
            if !root.is_dir() {
                return None;
            }
            if !same_volume(root, target_wt).unwrap_or(false) {
                return None;
            }
            let candidate = root.join(&rel);
            if fs::symlink_metadata(&candidate).is_err() {
                return None;
            }
            // Reject intermediate symlink traversal in donor root as well.
            if let Some(rel_parent) = rel.parent() {
                if !rel_parent.as_os_str().is_empty()
                    && ensure_no_symlink_escape(root, rel_parent).is_err()
                {
                    return None;
                }
            }
            if !same_volume(&candidate, target_wt).unwrap_or(false) {
                return None;
            }
            let lf_match = check_lockfile_match(entry, root, target_wt);
            Some((root, candidate, lf_match))
        })
        .collect();

    let selected = valid_donors
        .iter()
        .find(|(_, _, lf)| *lf == Some(true))
        .or_else(|| valid_donors.first())
        .map(|(_, path, lf)| (path.clone(), *lf));

    let Some((donor_path, lockfile_match)) = selected else {
        let cross_volume_exists = cross_volume_roots
            .iter()
            .any(|root| fs::symlink_metadata(root.join(&rel)).is_ok());
        let detail = if cross_volume_exists {
            format!(
                "source exists on a different volume (cross-volume clonefile EXDEV); seed `{}` or build one worktree on this volume to enable instant CoW cloning",
                seed_cache_base.join(&rel).display()
            )
        } else {
            "no same-volume donor found in cache, sibling worktrees, or main repo".to_string()
        };
        return CowCloneOutcome {
            entry: entry.to_string(),
            source: None,
            status: "skipped".to_string(),
            detail: Some(detail),
            lockfile_match: None,
        };
    };

    if dry_run {
        return CowCloneOutcome {
            entry: entry.to_string(),
            source: Some(donor_path.display().to_string()),
            status: "planned".to_string(),
            detail: None,
            lockfile_match,
        };
    }

    if let Some(parent) = dst.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            return CowCloneOutcome {
                entry: entry.to_string(),
                source: Some(donor_path.display().to_string()),
                status: "failed".to_string(),
                detail: Some(e.to_string()),
                lockfile_match,
            };
        }
        // Verify canonical destination parent stays strictly inside canonical target_wt.
        let Ok(canon_wt) = fs::canonicalize(target_wt) else {
            return CowCloneOutcome {
                entry: entry.to_string(),
                source: Some(donor_path.display().to_string()),
                status: "failed".to_string(),
                detail: Some("failed to canonicalize target worktree".to_string()),
                lockfile_match,
            };
        };
        let Ok(canon_parent) = fs::canonicalize(parent) else {
            return CowCloneOutcome {
                entry: entry.to_string(),
                source: Some(donor_path.display().to_string()),
                status: "failed".to_string(),
                detail: Some("failed to canonicalize destination parent".to_string()),
                lockfile_match,
            };
        };
        if !canon_parent.starts_with(&canon_wt) {
            return CowCloneOutcome {
                entry: entry.to_string(),
                source: Some(donor_path.display().to_string()),
                status: "skipped".to_string(),
                detail: Some("destination parent escapes worktree root".to_string()),
                lockfile_match,
            };
        }
    }

    match clone_path(&donor_path, &dst) {
        Ok(()) => CowCloneOutcome {
            entry: entry.to_string(),
            source: Some(donor_path.display().to_string()),
            status: "cloned".to_string(),
            detail: None,
            lockfile_match,
        },
        Err(e) => CowCloneOutcome {
            entry: entry.to_string(),
            source: Some(donor_path.display().to_string()),
            status: "failed".to_string(),
            detail: Some(e.to_string()),
            lockfile_match,
        },
    }
}

/// Hydrate an existing git worktree (created by T3 Code, an IDE, or `git worktree add`)
/// using `.caisson.toml` `cow_copy` entries (or zero-config `node_modules` / `.venv` detection
/// when `.caisson.toml` is absent) via same-volume APFS `clonefile(2)` and optional `post_create`.
pub fn wt_hydrate(config: &CaissonConfig, opts: &WtHydrateOptions) -> Result<WtHydrateReport> {
    let start_dir = match opts.path {
        Some(ref p) => p.clone(),
        None => std::env::current_dir()?,
    };
    let repo = discover_repo(&start_dir)?;
    let target_wt = worktree_toplevel(&start_dir)?;
    let branch = worktree_current_branch(&target_wt);

    // Prefer `.caisson.toml` from the target worktree for `cow_copy` paths (validated relative,
    // symlink-safe), falling back to main repo root, or zero-config `node_modules` auto-detection.
    // Security: `post_create` is NEVER executed automatically from a branch; it only runs when
    // `--run-post-create` is explicitly requested and is read strictly from `main_repo_root`.
    let main_repo_cfg = RepoConfig::load_from_repo(&repo.main_repo_root)?;
    let (cow_cfg, has_explicit_cfg) = if target_wt.join(".caisson.toml").is_file() {
        (RepoConfig::load_from_repo(&target_wt)?, true)
    } else if repo.main_repo_root.join(".caisson.toml").is_file() {
        (main_repo_cfg.clone(), true)
    } else {
        (RepoConfig::default(), false)
    };

    let (cow_entries, auto_detected) = if has_explicit_cfg {
        (cow_cfg.worktree.cow_copy.clone(), false)
    } else {
        let detected = auto_detect_cow_entries(&target_wt);
        let is_auto = !detected.is_empty();
        (detected, is_auto)
    };

    let volume_mount = statfs_path(&target_wt).ok().map(|st| st.mount_point);
    let volume_shielded = volume_mount
        .as_deref()
        .map(|mp| mp != "/" && inspect_shields(Path::new(mp)).all_healthy)
        .unwrap_or(false);
    let caisson_root = config.resolve_root();

    // Build same-volume donor search order (rejecting symlinked cache ancestors):
    // 1. `<volume_mount>/.caisson-cache/<repo_id>` and `<repo_name>`
    // 2. `<caisson_root>/cache/<repo_id>` and `<repo_name>` (when on same volume)
    // 3. Other git worktrees of this repo on the same volume, ordered by newest activity first.
    let mut candidate_roots: Vec<PathBuf> = Vec::new();
    let mut cross_volume_roots: Vec<PathBuf> = Vec::new();

    if let Some(ref mp) = volume_mount {
        if mp != "/" {
            let mp_path = Path::new(mp);
            push_safe_cache_candidate(
                &mut candidate_roots,
                mp_path,
                ".caisson-cache",
                &repo.repo_id,
            );
            push_safe_cache_candidate(
                &mut candidate_roots,
                mp_path,
                ".caisson-cache",
                &repo.repo_name,
            );
        }
    }
    if caisson_root.exists() && same_volume(&caisson_root, &target_wt).unwrap_or(false) {
        push_safe_cache_candidate(&mut candidate_roots, &caisson_root, "cache", &repo.repo_id);
        push_safe_cache_candidate(
            &mut candidate_roots,
            &caisson_root,
            "cache",
            &repo.repo_name,
        );
    }

    let git_worktrees = list_git_worktrees(&repo.main_repo_root).unwrap_or_default();
    let mut same_vol_peers: Vec<(Option<SystemTime>, PathBuf)> = Vec::new();
    for wt in git_worktrees {
        if wt.bare || wt.prunable || !wt.path.is_dir() {
            continue;
        }
        let canon = fs::canonicalize(&wt.path).unwrap_or(wt.path);
        if canon == target_wt {
            continue;
        }
        if same_volume(&canon, &target_wt).unwrap_or(false) {
            let activity = compute_last_activity(&canon);
            same_vol_peers.push((activity, canon));
        } else {
            cross_volume_roots.push(canon);
        }
    }
    // Newest activity first, tie-break deterministically by path.
    same_vol_peers.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    for (_, peer_path) in same_vol_peers {
        if !candidate_roots.contains(&peer_path) {
            candidate_roots.push(peer_path);
        }
    }

    let seed_cache_base = match volume_mount.as_deref() {
        Some(mp) if mp != "/" => PathBuf::from(mp)
            .join(".caisson-cache")
            .join(&repo.repo_name),
        _ => caisson_root.join("cache").join(&repo.repo_name),
    };

    let mut cow_clones = Vec::new();
    for entry in &cow_entries {
        cow_clones.push(cow_clone_from_candidates(
            &candidate_roots,
            &cross_volume_roots,
            &seed_cache_base,
            &target_wt,
            entry,
            opts.dry_run,
        ));
    }

    let mut post_create_ran = false;
    let mut post_create_command = None;
    if opts.run_post_create {
        if let Some(ref hook) = main_repo_cfg.worktree.post_create {
            let trimmed = hook.trim();
            if !trimmed.is_empty() {
                post_create_command = Some(trimmed.to_string());
                if !opts.dry_run {
                    let out = Command::new("/bin/sh")
                        .args(["-c", trimmed])
                        .current_dir(&target_wt)
                        .env("CAISSON_WORKTREE", &target_wt)
                        .env("CAISSON_REPO", &repo.main_repo_root)
                        .env("CAISSON_BRANCH", branch.as_deref().unwrap_or(""))
                        .output()?;
                    if !out.status.success() {
                        return Err(CaissonError::CommandFailed {
                            cmd: format!("post_create: {trimmed}"),
                            code: out.status.code(),
                            stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
                        });
                    }
                    post_create_ran = true;
                }
            }
        }
    }

    Ok(WtHydrateReport {
        repo_id: repo.repo_id,
        main_repo_root: repo.main_repo_root.display().to_string(),
        worktree_path: target_wt.display().to_string(),
        branch,
        volume_mount,
        volume_shielded,
        auto_detected,
        dry_run: opts.dry_run,
        cow_clones,
        post_create_ran,
        post_create_command,
    })
}

/// List Caisson worktrees (for the current repo by default, or across all repos with `--all`).
/// Never silently expands to all repositories when repo discovery fails unless `--all` is given.
pub fn wt_ls(
    config: &CaissonConfig,
    repo_path: Option<&Path>,
    all: bool,
) -> Result<Vec<WorktreeEntry>> {
    let caisson_root = config.resolve_root();
    if !caisson_root.exists() {
        return Ok(Vec::new());
    }

    let repo_filter = if all {
        None
    } else {
        let cwd = match repo_path {
            Some(p) => p.to_path_buf(),
            None => std::env::current_dir()?,
        };
        let repo = discover_repo(&cwd).map_err(|_| {
            CaissonError::Message(format!(
                "'{}' is not inside a git repository; pass `--repo <path>` or `--all` to list worktrees across all repositories",
                cwd.display()
            ))
        })?;
        Some(repo.repo_id)
    };

    scan_managed_worktrees(&caisson_root, repo_filter.as_deref(), SystemTime::now())
}

/// Resolve a managed worktree entry by slug, branch name, or path.
/// When `full_metadata` is true (e.g. `--json`), populates live git state, activity, and disk usage
/// for the single matched worktree without scanning siblings.
pub fn wt_path(
    config: &CaissonConfig,
    target: &str,
    repo_path: Option<&Path>,
    full_metadata: bool,
) -> Result<WorktreeEntry> {
    let caisson_root = config.resolve_root();
    if !caisson_root.exists() {
        return Err(CaissonError::Message(format!(
            "Caisson root '{}' does not exist",
            caisson_root.display()
        )));
    }
    let cwd = match repo_path {
        Some(p) => p.to_path_buf(),
        None => std::env::current_dir()?,
    };
    let current_repo = discover_repo(&cwd).ok();
    let now = SystemTime::now();
    let all_wts = scan_managed_worktrees_with_options(
        &caisson_root,
        None,
        now,
        ScanOptions {
            include_disk_bytes: false,
            include_git_state: false,
            include_activity: false,
        },
    )?;
    let mut matched = resolve_worktree_target(target, &all_wts, current_repo.as_ref())?;
    if full_metadata {
        let wt_path = Path::new(&matched.path);
        let owner_root = matched.main_repo_root.as_deref().map(Path::new);
        let git_state = inspect_worktree_git_state(wt_path, owner_root);
        let last_activity = compute_last_activity(wt_path);
        let inactive = last_activity
            .map(|t| inactive_duration(t, now))
            .unwrap_or_default();
        matched.dirty = git_state.dirty;
        matched.unpushed = git_state.unpushed;
        matched.inspection_failed = git_state.inspection_failed;
        matched.last_activity_utc = last_activity.map(|t| DateTime::<Utc>::from(t).to_rfc3339());
        matched.inactive_secs = inactive.as_secs();
        matched.inactive_human = format_age(inactive);
        matched.disk_bytes = allocated_dir_bytes(wt_path);
    }
    Ok(matched)
}

/// Remove a worktree via instant `<1 ms` atomic rename to `.trash/<uuid>` + `git worktree prune`.
/// Re-checks live git status immediately before trashing to avoid TOCTOU races.
pub fn wt_rm(
    config: &CaissonConfig,
    target: &str,
    repo_path: Option<&Path>,
    force: bool,
    sync: bool,
) -> Result<WtRmReport> {
    let caisson_root = config.resolve_root();
    ensure_caisson_root_ready(&caisson_root)?;

    let cwd = match repo_path {
        Some(p) => p.to_path_buf(),
        None => std::env::current_dir()?,
    };
    let current_repo = discover_repo(&cwd).ok();
    let all_wts = scan_managed_worktrees_with_options(
        &caisson_root,
        None,
        SystemTime::now(),
        ScanOptions {
            include_disk_bytes: false,
            include_git_state: false,
            include_activity: false,
        },
    )?;

    let matched = resolve_worktree_target(target, &all_wts, current_repo.as_ref())?;
    let wt_path = Path::new(&matched.path);

    // Re-inspect git state immediately prior to removal (prevents stale scan races & protects
    // worktrees whose git inspection failed unless `--force` is explicitly provided).
    if !force {
        let owner_root = matched.main_repo_root.as_deref().map(Path::new);
        let live_state = inspect_worktree_git_state(wt_path, owner_root);
        if live_state.dirty || live_state.unpushed || live_state.inspection_failed {
            return Err(CaissonError::WorktreeProtected(matched.slug.clone()));
        }
    }

    let trash_res = validate_and_trash_managed_worktree(&caisson_root, wt_path, sync)?;

    // Immediately deregister from git via `git worktree prune`.
    let pruned_repo = if let Some(ref owner) = matched.main_repo_root {
        let p = Path::new(owner);
        if p.exists() {
            let _ = prune_git_worktrees(p);
            Some(owner.clone())
        } else {
            None
        }
    } else if let Some(ref repo) = current_repo {
        let _ = prune_git_worktrees(&repo.main_repo_root);
        Some(repo.main_repo_root.display().to_string())
    } else {
        None
    };

    Ok(WtRmReport {
        slug: matched.slug,
        path: matched.path,
        trash_path: trash_res.trash_path,
        rename_ms: trash_res.rename_ms,
        cleaned_sync: trash_res.cleaned_sync,
        pruned_repo,
    })
}

fn match_tier_unambiguous<'a>(
    target: &str,
    candidates: &[&'a WorktreeEntry],
    tier_label: &str,
) -> Result<Option<&'a WorktreeEntry>> {
    match candidates {
        [] => Ok(None),
        [only] => Ok(Some(*only)),
        many => Err(CaissonError::Message(format!(
            "ambiguous worktree '{target}' matches {} entries by {tier_label} ({}); pass full path to disambiguate",
            many.len(),
            many.iter()
                .map(|w| format!("{}/{}", w.repo_id, w.slug))
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

fn resolve_within_scope<'a>(
    target: &str,
    slug_candidate: &str,
    pool: &[&'a WorktreeEntry],
) -> Result<Option<&'a WorktreeEntry>> {
    // Tier A: exact slug match
    let exact_slug: Vec<&WorktreeEntry> =
        pool.iter().copied().filter(|w| w.slug == target).collect();
    if let Some(found) = match_tier_unambiguous(target, &exact_slug, "slug")? {
        return Ok(Some(found));
    }

    // Tier B: exact branch match
    let exact_branch: Vec<&WorktreeEntry> = pool
        .iter()
        .copied()
        .filter(|w| w.branch.as_deref() == Some(target))
        .collect();
    if let Some(found) = match_tier_unambiguous(target, &exact_branch, "branch")? {
        return Ok(Some(found));
    }

    // Tier C: sanitized slug match (e.g. `feat/foo` -> `feat-foo`)
    if slug_candidate != target {
        let sanitized: Vec<&WorktreeEntry> = pool
            .iter()
            .copied()
            .filter(|w| w.slug == slug_candidate)
            .collect();
        if let Some(found) = match_tier_unambiguous(target, &sanitized, "sanitized slug")? {
            return Ok(Some(found));
        }
    }

    Ok(None)
}

fn resolve_worktree_target(
    target: &str,
    all_wts: &[WorktreeEntry],
    current_repo: Option<&GitRepoContext>,
) -> Result<WorktreeEntry> {
    // 1. Exact path match (canonicalized if possible)
    let target_path = Path::new(target);
    let canon_target = fs::canonicalize(target_path).ok();
    let path_matches: Vec<&WorktreeEntry> = all_wts
        .iter()
        .filter(|w| {
            w.path == target
                || canon_target
                    .as_ref()
                    .and_then(|ct| fs::canonicalize(&w.path).ok().map(|cw| cw == *ct))
                    .unwrap_or(false)
        })
        .collect();
    if let Some(found) = match_tier_unambiguous(target, &path_matches, "path")? {
        return Ok(found.clone());
    }

    let slug_candidate = sanitize_slug(target).unwrap_or_else(|_| target.to_string());

    // 2. Prefer current repo with strict tier precedence (slug -> branch -> sanitized slug),
    //    never silently picking an arbitrary entry if a tier has multiple matches.
    if let Some(repo) = current_repo {
        let repo_pool: Vec<&WorktreeEntry> = all_wts
            .iter()
            .filter(|w| w.repo_id == repo.repo_id)
            .collect();
        if let Some(found) = resolve_within_scope(target, &slug_candidate, &repo_pool)? {
            return Ok(found.clone());
        }
    }

    // 3. Global fallback across all repos with the same strict tier precedence & ambiguity check.
    let global_pool: Vec<&WorktreeEntry> = all_wts.iter().collect();
    if let Some(found) = resolve_within_scope(target, &slug_candidate, &global_pool)? {
        return Ok(found.clone());
    }

    Err(CaissonError::Message(format!(
        "worktree '{target}' not found (run `caisson wt ls --all` to list managed worktrees)"
    )))
}

/// Prune inactive worktrees (dry-run by default; pass `--apply` to remove).
///
/// Requires a valid git repository unless `--all` is explicitly passed, uses the same containment
/// boundary check as `wt rm`, and re-inspects git state immediately before trashing each worktree.
pub fn wt_prune(config: &CaissonConfig, opts: &WtPruneOptions) -> Result<WtPruneReport> {
    let threshold_str = opts
        .inactive
        .clone()
        .unwrap_or_else(|| config.prune.default_inactive.clone());
    let threshold = parse_duration(&threshold_str)?;

    let caisson_root = config.resolve_root();
    if !caisson_root.exists() {
        return Ok(WtPruneReport {
            dry_run: !opts.apply,
            inactive_threshold: threshold_str,
            pruned: Vec::new(),
            skipped_protected: Vec::new(),
            active_count: 0,
            reclaimed_bytes: 0,
        });
    }

    // Propagate repo discovery errors unless `--all` is explicitly requested!
    let entries = wt_ls(config, opts.repo_path.as_deref(), opts.all)?;
    let protect_dirty = config.prune.protect_dirty && !opts.include_dirty;
    let protect_unpushed = config.prune.protect_unpushed && !opts.include_unpushed;
    let protect_unknown = protect_dirty || protect_unpushed;

    let mut pruned = Vec::new();
    let mut skipped_protected = Vec::new();
    let mut active_count = 0usize;
    let mut reclaimed_bytes = 0u64;
    let mut repos_to_prune: BTreeSet<PathBuf> = BTreeSet::new();

    for entry in entries {
        if entry.inactive_secs < threshold.as_secs() {
            active_count += 1;
            continue;
        }

        let wt_path = Path::new(&entry.path);
        // Re-check live git state right before acting on this worktree.
        let live_state = if opts.apply {
            inspect_worktree_git_state(wt_path, entry.main_repo_root.as_deref().map(Path::new))
        } else {
            crate::worktree::git::WorktreeGitState {
                dirty: entry.dirty,
                unpushed: entry.unpushed,
                inspection_failed: entry.inspection_failed,
                inspection_error: None,
            }
        };

        let mut reasons = Vec::new();
        if protect_dirty && live_state.dirty {
            reasons.push("dirty");
        }
        if protect_unpushed && live_state.unpushed {
            reasons.push("unpushed");
        }
        if protect_unknown && live_state.inspection_failed {
            reasons.push("unknown-git-state");
        }

        if !reasons.is_empty() {
            skipped_protected.push(PruneSkipped {
                slug: entry.slug,
                path: entry.path,
                inactive_human: entry.inactive_human,
                reason: reasons.join("+"),
            });
            continue;
        }

        reclaimed_bytes = reclaimed_bytes.saturating_add(entry.disk_bytes);

        if opts.apply {
            let _ = validate_and_trash_managed_worktree(&caisson_root, wt_path, opts.sync)?;
            if let Some(ref owner) = entry.main_repo_root {
                repos_to_prune.insert(PathBuf::from(owner));
            }
        }
        pruned.push(entry);
    }

    if opts.apply {
        for repo_root in repos_to_prune {
            if repo_root.exists() {
                let _ = prune_git_worktrees(&repo_root);
            }
        }
    }

    Ok(WtPruneReport {
        dry_run: !opts.apply,
        inactive_threshold: threshold_str,
        pruned,
        skipped_protected,
        active_count,
        reclaimed_bytes,
    })
}

/// Drain `<caisson_root>/.trash/` and reconcile orphaned git worktree metadata.
pub fn run_gc(config: &CaissonConfig, repo_path: Option<&Path>) -> Result<GcReport> {
    let caisson_root = config.resolve_root();
    ensure_caisson_root_ready(&caisson_root)?;

    let trash = drain_trash(&caisson_root)?;
    let mut repos_to_prune: BTreeSet<PathBuf> = BTreeSet::new();

    if let Ok(entries) = scan_managed_worktrees(&caisson_root, None, SystemTime::now()) {
        for e in entries {
            if let Some(owner) = e.main_repo_root {
                repos_to_prune.insert(PathBuf::from(owner));
            }
        }
    }

    let cwd = match repo_path {
        Some(p) => p.to_path_buf(),
        None => std::env::current_dir()?,
    };
    if let Ok(repo) = discover_repo(&cwd) {
        repos_to_prune.insert(repo.main_repo_root);
    }

    let mut pruned_repos = Vec::new();
    for repo_root in repos_to_prune {
        if repo_root.exists() && prune_git_worktrees(&repo_root).is_ok() {
            pruned_repos.push(repo_root.display().to_string());
        }
    }

    // Remove empty `<caisson_root>/worktrees/<repo-id>/` directories (never following symlinks)
    let mut removed_empty_repo_dirs = 0usize;
    let worktrees_base = caisson_root.join("worktrees");
    if let Ok(entries) = fs::read_dir(&worktrees_base) {
        for entry in entries.flatten() {
            let p = entry.path();
            let Ok(meta) = fs::symlink_metadata(&p) else {
                continue;
            };
            if !meta.file_type().is_symlink()
                && meta.is_dir()
                && fs::read_dir(&p)
                    .map(|mut rd| rd.next().is_none())
                    .unwrap_or(false)
                && fs::remove_dir(&p).is_ok()
            {
                removed_empty_repo_dirs += 1;
            }
        }
    }

    Ok(GcReport {
        trash,
        pruned_repos,
        removed_empty_repo_dirs,
    })
}
