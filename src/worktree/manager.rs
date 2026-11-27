use crate::apfs::clonefile::clone_path;
use crate::apfs::statfs::same_volume;
use crate::config::{CaissonConfig, RepoConfig};
use crate::error::{CaissonError, Result};
use crate::units::{format_age, parse_duration};
use crate::worktree::activity::{compute_last_activity, inactive_duration};
use crate::worktree::git::{
    add_git_worktree, discover_repo, inspect_worktree_git_state, owner_repo_root_from_worktree,
    prune_git_worktrees, sanitize_slug, worktree_current_branch, GitRepoContext,
};
use crate::worktree::quotas::{evaluate_add_quotas, measure_root_usage, QuotaSnapshot};
use crate::worktree::trash::{allocated_dir_bytes, drain_trash, trash_worktree, TrashDrainReport};
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

fn ensure_caisson_root_ready(caisson_root: &Path) -> Result<()> {
    if !caisson_root.exists() {
        return Err(CaissonError::Message(format!(
            "Caisson root '{}' does not exist. Run `sudo caisson init` once to create `/Volumes/caisson`, or set `CAISSON_ROOT`.",
            caisson_root.display()
        )));
    }
    fs::create_dir_all(caisson_root.join("worktrees"))?;
    fs::create_dir_all(caisson_root.join(".trash"))?;
    Ok(())
}

/// Scan managed worktrees under `<caisson_root>/worktrees/`.
/// When `repo_filter` is `Some(repo_id)`, limits scanning to that repository.
pub fn scan_managed_worktrees(
    caisson_root: &Path,
    repo_filter: Option<&str>,
    now: SystemTime,
) -> Result<Vec<WorktreeEntry>> {
    let worktrees_base = caisson_root.join("worktrees");
    if !worktrees_base.exists() {
        return Ok(Vec::new());
    }

    let mut results = Vec::new();
    let repo_dirs: Vec<(String, PathBuf)> = if let Some(rid) = repo_filter {
        let p = worktrees_base.join(rid);
        if p.is_dir() {
            vec![(rid.to_string(), p)]
        } else {
            Vec::new()
        }
    } else {
        fs::read_dir(&worktrees_base)?
            .flatten()
            .filter_map(|e| {
                let p = e.path();
                if p.is_dir() {
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
            if !wt_path.is_dir() {
                continue;
            }
            let slug = entry.file_name().to_string_lossy().into_owned();
            let owner_root = owner_repo_root_from_worktree(&wt_path);
            let orphaned = owner_root.as_ref().map(|r| !r.exists()).unwrap_or(true);
            let branch = worktree_current_branch(&wt_path);
            let git_state = inspect_worktree_git_state(&wt_path, owner_root.as_deref());

            let last_activity = compute_last_activity(&wt_path);
            let inactive = last_activity
                .map(|t| inactive_duration(t, now))
                .unwrap_or_default();
            let last_activity_utc = last_activity.map(|t| DateTime::<Utc>::from(t).to_rfc3339());
            let disk_bytes = allocated_dir_bytes(&wt_path);

            results.push(WorktreeEntry {
                repo_id: repo_id.clone(),
                slug,
                path: wt_path.display().to_string(),
                main_repo_root: owner_root.map(|p| p.display().to_string()),
                branch,
                last_activity_utc,
                inactive_secs: inactive.as_secs(),
                inactive_human: format_age(inactive),
                dirty: git_state.dirty,
                unpushed: git_state.unpushed,
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
    let all_wts = scan_managed_worktrees(&caisson_root, None, now)?;
    let repo_wts_count = all_wts.iter().filter(|w| w.repo_id == repo.repo_id).count();
    let default_inactive = config.default_inactive_duration()?;
    let reclaimable = all_wts
        .iter()
        .filter(|w| {
            w.inactive_secs >= default_inactive.as_secs()
                && (!config.prune.protect_dirty || !w.dirty)
                && (!config.prune.protect_unpushed || !w.unpushed)
        })
        .count();

    let (caisson_used_bytes, host_available_bytes) = measure_root_usage(&caisson_root)?;
    let quota_outcome = evaluate_add_quotas(
        config,
        &QuotaSnapshot {
            repo_worktrees: repo_wts_count,
            total_worktrees: all_wts.len(),
            reclaimable_worktrees: reclaimable,
            caisson_used_bytes,
            host_available_bytes,
        },
        opts.force,
    )?;

    let slug = match opts.name {
        Some(ref explicit) => sanitize_slug(explicit)?,
        None => sanitize_slug(&opts.branch)?,
    };

    let repo_wt_parent = caisson_root.join("worktrees").join(&repo.repo_id);
    fs::create_dir_all(&repo_wt_parent)?;
    let target_path = repo_wt_parent.join(&slug);
    if target_path.exists() {
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
    if p.is_absolute()
        || p.components().any(|c| {
            matches!(
                c,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(CaissonError::Message(format!(
            "unsafe `cow_copy` path '{entry}': must be relative to the worktree root"
        )));
    }
    Ok(p.to_path_buf())
}

fn cow_clone_entry(
    caisson_root: &Path,
    repo: &GitRepoContext,
    siblings: &[PathBuf],
    target_wt: &Path,
    entry: &str,
) -> CowCloneOutcome {
    let rel = match validate_relative_cow_entry(entry) {
        Ok(p) => p,
        Err(e) => {
            return CowCloneOutcome {
                entry: entry.to_string(),
                source: None,
                status: "skipped".to_string(),
                detail: Some(e.to_string()),
            };
        }
    };

    let dst = target_wt.join(&rel);
    if dst.exists() {
        return CowCloneOutcome {
            entry: entry.to_string(),
            source: None,
            status: "skipped".to_string(),
            detail: Some("already exists in new worktree".to_string()),
        };
    }

    // Priority order for donor selection (all same-volume candidates first):
    // 1. Warm cache: `<root>/cache/<repo-id>/<rel>` or `<root>/cache/<repo-name>/<rel>`
    // 2. Sibling worktrees on `<root>/worktrees/<repo-id>/*/<rel>` (most recently active first)
    // 3. Main repo `<main_repo>/<rel>` if on the same APFS volume
    let mut candidates = vec![
        caisson_root.join("cache").join(&repo.repo_id).join(&rel),
        caisson_root.join("cache").join(&repo.repo_name).join(&rel),
    ];
    for sib in siblings {
        candidates.push(sib.join(&rel));
    }

    let main_candidate = repo.main_repo_root.join(&rel);
    let mut cross_volume_main_exists = false;
    if main_candidate.exists() {
        if same_volume(&main_candidate, target_wt).unwrap_or(false) {
            candidates.push(main_candidate);
        } else {
            cross_volume_main_exists = true;
        }
    }

    let donor = candidates.into_iter().find(|p| p.exists());
    let Some(donor_path) = donor else {
        let detail = if cross_volume_main_exists {
            format!(
                "source exists in main repo on a different volume (cross-volume clonefile EXDEV); seed `{}` or create one worktree with deps to enable instant CoW cloning",
                caisson_root
                    .join("cache")
                    .join(&repo.repo_name)
                    .join(&rel)
                    .display()
            )
        } else {
            "no donor directory found in cache, sibling worktrees, or main repo".to_string()
        };
        return CowCloneOutcome {
            entry: entry.to_string(),
            source: None,
            status: "skipped".to_string(),
            detail: Some(detail),
        };
    };

    if let Some(parent) = dst.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            return CowCloneOutcome {
                entry: entry.to_string(),
                source: Some(donor_path.display().to_string()),
                status: "failed".to_string(),
                detail: Some(e.to_string()),
            };
        }
    }

    match clone_path(&donor_path, &dst) {
        Ok(()) => CowCloneOutcome {
            entry: entry.to_string(),
            source: Some(donor_path.display().to_string()),
            status: "cloned".to_string(),
            detail: None,
        },
        Err(e) => CowCloneOutcome {
            entry: entry.to_string(),
            source: Some(donor_path.display().to_string()),
            status: "failed".to_string(),
            detail: Some(e.to_string()),
        },
    }
}

/// List Caisson worktrees (for the current repo by default, or across all repos with `--all`).
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
        discover_repo(&cwd).ok().map(|r| r.repo_id)
    };

    scan_managed_worktrees(&caisson_root, repo_filter.as_deref(), SystemTime::now())
}

/// Resolve a managed worktree entry by slug, branch name, or path.
pub fn wt_path(
    config: &CaissonConfig,
    target: &str,
    repo_path: Option<&Path>,
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
    let all_wts = scan_managed_worktrees(&caisson_root, None, SystemTime::now())?;
    resolve_worktree_target(target, &all_wts, current_repo.as_ref())
}

/// Remove a worktree via instant `<1 ms` atomic rename to `.trash/<uuid>` + `git worktree prune`.
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
    let all_wts = scan_managed_worktrees(&caisson_root, None, SystemTime::now())?;

    let matched = resolve_worktree_target(target, &all_wts, current_repo.as_ref())?;

    // Verify safety boundary: target MUST reside under `<caisson_root>/worktrees/`.
    let canonical_wt = fs::canonicalize(&matched.path)?;
    let canonical_base = fs::canonicalize(caisson_root.join("worktrees"))?;
    if !canonical_wt.starts_with(&canonical_base) || canonical_wt == canonical_base {
        return Err(CaissonError::Message(format!(
            "refusing to remove path outside Caisson worktrees root: {}",
            canonical_wt.display()
        )));
    }

    if !force && (matched.dirty || matched.unpushed) {
        return Err(CaissonError::WorktreeProtected(matched.slug.clone()));
    }

    let trash_res = trash_worktree(&caisson_root, &canonical_wt, sync)?;

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

fn resolve_worktree_target(
    target: &str,
    all_wts: &[WorktreeEntry],
    current_repo: Option<&GitRepoContext>,
) -> Result<WorktreeEntry> {
    // 1. Exact path match (canonicalized if possible)
    let target_path = Path::new(target);
    let canon_target = fs::canonicalize(target_path).ok();
    if let Some(found) = all_wts.iter().find(|w| {
        w.path == target
            || canon_target
                .as_ref()
                .and_then(|ct| fs::canonicalize(&w.path).ok().map(|cw| cw == *ct))
                .unwrap_or(false)
    }) {
        return Ok(found.clone());
    }

    let slug_candidate = sanitize_slug(target).unwrap_or_else(|_| target.to_string());

    // 2. Prefer current repo matches by slug or branch
    if let Some(repo) = current_repo {
        if let Some(found) = all_wts.iter().find(|w| {
            w.repo_id == repo.repo_id
                && (w.slug == target
                    || w.slug == slug_candidate
                    || w.branch.as_deref() == Some(target))
        }) {
            return Ok(found.clone());
        }
    }

    // 3. Fallback across all repos if unambiguous
    let global_matches: Vec<&WorktreeEntry> = all_wts
        .iter()
        .filter(|w| {
            w.slug == target || w.slug == slug_candidate || w.branch.as_deref() == Some(target)
        })
        .collect();

    match global_matches.as_slice() {
        [only] => Ok((*only).clone()),
        [] => Err(CaissonError::Message(format!(
            "worktree '{target}' not found (run `caisson wt ls --all` to list managed worktrees)"
        ))),
        many => Err(CaissonError::Message(format!(
            "ambiguous worktree '{target}' matches {} repos ({}); pass full path or run inside the target repo",
            many.len(),
            many.iter()
                .map(|w| format!("{}/{}", w.repo_id, w.slug))
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

/// Prune inactive worktrees (dry-run by default; pass `--apply` to remove).
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

    let entries = wt_ls(config, opts.repo_path.as_deref(), opts.all)?;
    let protect_dirty = config.prune.protect_dirty && !opts.include_dirty;
    let protect_unpushed = config.prune.protect_unpushed && !opts.include_unpushed;

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

        let mut reasons = Vec::new();
        if protect_dirty && entry.dirty {
            reasons.push("dirty");
        }
        if protect_unpushed && entry.unpushed {
            reasons.push("unpushed");
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
            let _ = trash_worktree(&caisson_root, Path::new(&entry.path), opts.sync)?;
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

    // Remove empty `<caisson_root>/worktrees/<repo-id>/` directories
    let mut removed_empty_repo_dirs = 0usize;
    let worktrees_base = caisson_root.join("worktrees");
    if let Ok(entries) = fs::read_dir(&worktrees_base) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir()
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
