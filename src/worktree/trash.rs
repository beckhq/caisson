use crate::error::{CaissonError, Result};
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

/// Ensure `<caisson_root>/.trash` is a real directory (never a symlink) inside `caisson_root`.
pub fn ensure_safe_trash_dir(caisson_root: &Path) -> Result<PathBuf> {
    let trash_dir = caisson_root.join(".trash");
    match fs::symlink_metadata(&trash_dir) {
        Ok(meta) => {
            if meta.file_type().is_symlink() || !meta.is_dir() {
                return Err(CaissonError::Message(format!(
                    "refusing to use symlink or non-directory as `.trash`: {}",
                    trash_dir.display()
                )));
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(&trash_dir)?;
        }
        Err(e) => return Err(CaissonError::Io(e)),
    }
    Ok(trash_dir)
}

/// Atomically move `target` into `<caisson_root>/.trash/<uuid>` (`rename(2)`, `<1 ms` on the same
/// APFS volume) and either unlink it synchronously (`sync = true`) or spawn a detached background
/// `rm -rf` worker (`sync = false`).
pub fn trash_worktree(caisson_root: &Path, target: &Path, sync: bool) -> Result<TrashResult> {
    let target_meta = fs::symlink_metadata(target)?;
    if target_meta.file_type().is_symlink() || !target_meta.is_dir() {
        return Err(CaissonError::Message(format!(
            "refusing to trash symlink or non-directory: {}",
            target.display()
        )));
    }

    let trash_dir = ensure_safe_trash_dir(caisson_root)?;
    let trash_entry = trash_dir.join(Uuid::new_v4().to_string());
    let start = Instant::now();
    fs::rename(target, &trash_entry)?;
    let rename_ms = (start.elapsed().as_secs_f64() * 1000.0 * 100.0).round() / 100.0;

    if sync {
        let entry_meta = fs::symlink_metadata(&trash_entry)?;
        if entry_meta.file_type().is_symlink() || !entry_meta.is_dir() {
            fs::remove_file(&trash_entry)?;
        } else {
            fs::remove_dir_all(&trash_entry)?;
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
/// Rejects a symlinked `.trash` directory and unlinks symlink entries without following them.
pub fn drain_trash(caisson_root: &Path) -> Result<TrashDrainReport> {
    let trash_dir = caisson_root.join(".trash");
    match fs::symlink_metadata(&trash_dir) {
        Ok(meta) => {
            if meta.file_type().is_symlink() || !meta.is_dir() {
                return Err(CaissonError::Message(format!(
                    "refusing to drain symlink or non-directory `.trash`: {}",
                    trash_dir.display()
                )));
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(TrashDrainReport {
                trash_dir: trash_dir.display().to_string(),
                entries_removed: 0,
                bytes_reclaimed: 0,
            });
        }
        Err(e) => return Err(CaissonError::Io(e)),
    }

    let mut entries_removed = 0usize;
    let mut bytes_reclaimed = 0u64;

    for entry in fs::read_dir(&trash_dir)?.flatten() {
        let p = entry.path();
        let Ok(meta) = fs::symlink_metadata(&p) else {
            continue;
        };
        bytes_reclaimed = bytes_reclaimed.saturating_add(allocated_dir_bytes(&p));
        let removed = if meta.file_type().is_symlink() || !meta.is_dir() {
            fs::remove_file(&p).is_ok()
        } else {
            fs::remove_dir_all(&p).is_ok()
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
    if meta.file_type().is_symlink() || !meta.is_dir() {
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
            if !m.file_type().is_symlink() && m.is_dir() {
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
    use std::os::unix::fs::symlink;
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
    fn drain_trash_cleans_leftover_entries_and_rejects_symlink_trash_dir() {
        let root = tempdir().unwrap();
        let leftover = root.path().join(".trash/old-entry");
        fs::create_dir_all(&leftover).unwrap();
        fs::write(leftover.join("big.bin"), vec![0u8; 8192]).unwrap();

        let report = drain_trash(root.path()).unwrap();
        assert_eq!(report.entries_removed, 1);
        assert!(report.bytes_reclaimed >= 4096);
        assert!(!leftover.exists());

        // Symlinked `.trash` MUST be refused!
        let root2 = tempdir().unwrap();
        let victim = tempdir().unwrap();
        fs::write(victim.path().join("keep.txt"), "safe").unwrap();
        symlink(victim.path(), root2.path().join(".trash")).unwrap();
        assert!(drain_trash(root2.path()).is_err());
        assert!(victim.path().join("keep.txt").exists());
    }
}
