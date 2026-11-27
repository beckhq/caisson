use crate::error::Result;
use serde::Serialize;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize)]
pub struct TrashResult {
    pub original_path: String,
    pub trash_path: String,
    pub rename_ms: f64,
    pub cleaned_sync: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct TrashDrainReport {
    pub trash_dir: String,
    pub entries_removed: usize,
    pub bytes_reclaimed: u64,
}

/// Atomically move `target` into `<caisson_root>/.trash/<uuid>` (`rename(2)`, `<1 ms` on the same
/// APFS volume) and either unlink it synchronously (`sync = true`) or spawn a detached background
/// `rm -rf` worker (`sync = false`).
pub fn trash_worktree(caisson_root: &Path, target: &Path, sync: bool) -> Result<TrashResult> {
    let trash_dir = caisson_root.join(".trash");
    fs::create_dir_all(&trash_dir)?;

    let trash_entry = trash_dir.join(Uuid::new_v4().to_string());
    let start = Instant::now();
    fs::rename(target, &trash_entry)?;
    let rename_ms = (start.elapsed().as_secs_f64() * 1000.0 * 100.0).round() / 100.0;

    if sync {
        if trash_entry.is_dir() {
            fs::remove_dir_all(&trash_entry)?;
        } else if trash_entry.exists() {
            fs::remove_file(&trash_entry)?;
        }
    } else {
        spawn_detached_unlink(&trash_entry);
    }

    Ok(TrashResult {
        original_path: target.display().to_string(),
        trash_path: trash_entry.display().to_string(),
        rename_ms,
        cleaned_sync: sync,
    })
}

fn spawn_detached_unlink(path: &Path) {
    let mut cmd = Command::new("/bin/rm");
    cmd.args(["-rf", "--"])
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    // Place the background unlink worker in its own session so it finishes cleanly after CLI exit.
    unsafe {
        cmd.pre_exec(|| {
            let _ = libc::setsid();
            Ok(())
        });
    }
    let _ = cmd.spawn();
}

/// Synchronously drain every pending item inside `<caisson_root>/.trash/`.
pub fn drain_trash(caisson_root: &Path) -> Result<TrashDrainReport> {
    let trash_dir = caisson_root.join(".trash");
    if !trash_dir.exists() {
        return Ok(TrashDrainReport {
            trash_dir: trash_dir.display().to_string(),
            entries_removed: 0,
            bytes_reclaimed: 0,
        });
    }

    let mut entries_removed = 0usize;
    let mut bytes_reclaimed = 0u64;

    for entry in fs::read_dir(&trash_dir)?.flatten() {
        let p = entry.path();
        bytes_reclaimed = bytes_reclaimed.saturating_add(allocated_dir_bytes(&p));
        let removed = if p.is_dir() {
            fs::remove_dir_all(&p).is_ok()
        } else {
            fs::remove_file(&p).is_ok()
        };
        if removed {
            entries_removed += 1;
        }
    }

    Ok(TrashDrainReport {
        trash_dir: trash_dir.display().to_string(),
        entries_removed,
        bytes_reclaimed,
    })
}

/// Calculate allocated disk bytes for a file or directory tree without following symlinks.
/// Uses `blocks() * 512` (with fallback to `len()` for tiny inline files) so sparse/APFS
/// files reflect real disk allocation.
pub fn allocated_dir_bytes(root: &Path) -> u64 {
    let Ok(meta) = fs::symlink_metadata(root) else {
        return 0;
    };
    if !meta.is_dir() {
        let blocks_bytes = meta.blocks().saturating_mul(512);
        return if blocks_bytes > 0 {
            blocks_bytes
        } else {
            meta.len()
        };
    }

    let mut total = 0u64;
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(m) = fs::symlink_metadata(entry.path()) else {
                continue;
            };
            if m.is_dir() {
                stack.push(entry.path());
            } else {
                let blocks_bytes = m.blocks().saturating_mul(512);
                total = total.saturating_add(if blocks_bytes > 0 {
                    blocks_bytes
                } else {
                    m.len()
                });
            }
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn trash_worktree_sync_renames_and_removes() {
        let root = tempdir().unwrap();
        let wt = root.path().join("worktrees/repo/slug");
        fs::create_dir_all(&wt).unwrap();
        fs::write(wt.join("file.txt"), "payload").unwrap();

        let res = trash_worktree(root.path(), &wt, true).unwrap();
        assert!(res.cleaned_sync);
        assert!(!wt.exists());
        assert!(!Path::new(&res.trash_path).exists());
    }

    #[test]
    fn drain_trash_cleans_leftover_entries() {
        let root = tempdir().unwrap();
        let leftover = root.path().join(".trash/old-entry");
        fs::create_dir_all(&leftover).unwrap();
        fs::write(leftover.join("big.bin"), vec![0u8; 8192]).unwrap();

        let report = drain_trash(root.path()).unwrap();
        assert_eq!(report.entries_removed, 1);
        assert!(report.bytes_reclaimed >= 4096);
        assert!(!leftover.exists());
    }
}
