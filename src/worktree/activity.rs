use crate::worktree::git::resolve_worktree_admin_dir;
use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime};

const SKIP_TOP_LEVEL: &[&str] = &[
    ".git",
    "node_modules",
    ".venv",
    "target",
    "out",
    "build",
    ".next",
    ".nuxt",
    "dist",
    "DerivedData",
];

/// Compute a worktree's last-activity timestamp in `<1 ms` without crawling `node_modules`
/// (`ARCHITECTURE.md` §5):
/// inspects `.git/worktrees/<name>/{index,HEAD,logs/HEAD,ORIG_HEAD,FETCH_HEAD}` and top-level
/// worktree directory entries (depth 1 only).
pub fn compute_last_activity(wt_path: &Path) -> Option<SystemTime> {
    let mut latest: Option<SystemTime> = None;

    let mut consider = |p: &Path| {
        if let Ok(meta) = fs::symlink_metadata(p) {
            if let Ok(mtime) = meta.modified() {
                latest = Some(match latest {
                    Some(prev) => prev.max(mtime),
                    None => mtime,
                });
            }
        }
    };

    // 1. Worktree root directory mtime
    consider(wt_path);

    // 2. Depth-1 top-level entries (never recurses into subdirs or node_modules)
    if let Ok(entries) = fs::read_dir(wt_path) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            if let Some(name_str) = name.to_str() {
                if SKIP_TOP_LEVEL.contains(&name_str) {
                    continue;
                }
            }
            consider(&entry.path());
        }
    }

    // 3. Git worktree admin metadata files (`.git/worktrees/<name>/{index,HEAD,logs/HEAD}`)
    if let Some(admin_dir) = resolve_worktree_admin_dir(wt_path) {
        for rel in ["index", "HEAD", "logs/HEAD", "ORIG_HEAD", "FETCH_HEAD"] {
            consider(&admin_dir.join(rel));
        }
    }

    latest
}

/// Elapsed inactivity duration between `last_activity` and `now`.
pub fn inactive_duration(last_activity: SystemTime, now: SystemTime) -> Duration {
    now.duration_since(last_activity).unwrap_or(Duration::ZERO)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use tempfile::tempdir;

    #[test]
    fn activity_tracks_top_level_and_git_admin_without_crawling_node_modules() {
        let dir = tempdir().unwrap();
        let wt = dir.path().join("wt");
        let admin = dir.path().join("admin");
        let nm = wt.join("node_modules").join("huge-pkg");
        fs::create_dir_all(&nm).unwrap();
        fs::create_dir_all(admin.join("logs")).unwrap();
        fs::write(wt.join(".git"), format!("gitdir: {}\n", admin.display())).unwrap();
        fs::write(admin.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        fs::write(wt.join("README.md"), "hello\n").unwrap();

        let t1 = compute_last_activity(&wt).unwrap();
        thread::sleep(Duration::from_millis(25));

        // Touching a file deep inside node_modules should NOT bump last activity.
        fs::write(nm.join("index.js"), "updated").unwrap();
        let t2 = compute_last_activity(&wt).unwrap();
        assert_eq!(t1, t2);

        thread::sleep(Duration::from_millis(25));
        // Updating git admin `logs/HEAD` MUST bump last activity.
        fs::write(admin.join("logs/HEAD"), "new commit\n").unwrap();
        let t3 = compute_last_activity(&wt).unwrap();
        assert!(t3 > t2);
    }
}
