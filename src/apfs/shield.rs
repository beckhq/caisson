use crate::apfs::statfs::statfs_path;
use crate::error::{CaissonError, Result};
use serde::Serialize;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

#[derive(Debug, Clone, Serialize)]
pub struct ShieldStepResult {
    pub step: String,
    pub command: Option<String>,
    pub status: String,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ShieldApplyReport {
    pub mount_point: String,
    pub dry_run: bool,
    pub steps: Vec<ShieldStepResult>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ShieldStatus {
    pub mount_point: String,
    pub mounted: bool,
    pub metadata_never_index: bool,
    pub fseventsd_no_log: bool,
    pub trashes_blocked: bool,
    pub noatime: bool,
    pub nobrowse: bool,
    pub spotlight_disabled: bool,
    pub tm_excluded: bool,
    pub all_healthy: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ShieldOptions {
    pub nobrowse: bool,
    pub owner: Option<String>,
    pub dry_run: bool,
}

/// Apply the idempotent 7-step Caisson shield stack (`ARCHITECTURE.md` §4).
///
/// When `opts.dry_run` is true, no filesystem or system mutations are performed; every step
/// reports the exact command or file operation that would be executed.
pub fn apply_shields(mount_point: &Path, opts: &ShieldOptions) -> Result<ShieldApplyReport> {
    let mp = mount_point.display().to_string();
    let mut steps = Vec::new();

    // Step 1: mdutil -i off <mountpoint>
    let mdutil_cmd = format!("mdutil -i off {mp}");
    if opts.dry_run {
        steps.push(ShieldStepResult {
            step: "spotlight_mdutil".to_string(),
            command: Some(mdutil_cmd),
            status: "planned".to_string(),
            detail: None,
        });
    } else {
        let out = Command::new("mdutil").args(["-i", "off", &mp]).output()?;
        if !out.status.success() {
            return Err(CaissonError::CommandFailed {
                cmd: mdutil_cmd,
                code: out.status.code(),
                stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
            });
        }
        steps.push(ShieldStepResult {
            step: "spotlight_mdutil".to_string(),
            command: Some(mdutil_cmd),
            status: "applied".to_string(),
            detail: Some(String::from_utf8_lossy(&out.stdout).trim().to_string()),
        });
    }

    // Step 2: <mountpoint>/.metadata_never_index (0444)
    let never_index = mount_point.join(".metadata_never_index");
    if opts.dry_run {
        steps.push(ShieldStepResult {
            step: "metadata_never_index".to_string(),
            command: Some(format!(
                "touch {} && chmod 0444 {}",
                never_index.display(),
                never_index.display()
            )),
            status: "planned".to_string(),
            detail: None,
        });
    } else {
        write_readonly_marker(&never_index)?;
        steps.push(ShieldStepResult {
            step: "metadata_never_index".to_string(),
            command: None,
            status: "applied".to_string(),
            detail: Some(never_index.display().to_string()),
        });
    }

    // Step 3: <mountpoint>/.fseventsd/no_log
    let fseventsd_dir = mount_point.join(".fseventsd");
    let no_log = fseventsd_dir.join("no_log");
    if opts.dry_run {
        steps.push(ShieldStepResult {
            step: "fseventsd_no_log".to_string(),
            command: Some(format!(
                "mkdir -p {} && touch {}",
                fseventsd_dir.display(),
                no_log.display()
            )),
            status: "planned".to_string(),
            detail: None,
        });
    } else {
        fs::create_dir_all(&fseventsd_dir)?;
        if !no_log.exists() {
            fs::write(&no_log, b"")?;
        }
        steps.push(ShieldStepResult {
            step: "fseventsd_no_log".to_string(),
            command: None,
            status: "applied".to_string(),
            detail: Some(no_log.display().to_string()),
        });
    }

    // Step 4: <mountpoint>/.Trashes (0-byte regular file, mode 0444)
    let trashes = mount_point.join(".Trashes");
    if opts.dry_run {
        steps.push(ShieldStepResult {
            step: "block_trashes".to_string(),
            command: Some(format!(
                "touch {} && chmod 0444 {}",
                trashes.display(),
                trashes.display()
            )),
            status: "planned".to_string(),
            detail: None,
        });
    } else {
        if let Ok(meta) = fs::symlink_metadata(&trashes) {
            if meta.is_dir() {
                let _ = fs::remove_dir(&trashes);
            }
        }
        if !trashes.is_dir() {
            write_readonly_marker(&trashes)?;
            steps.push(ShieldStepResult {
                step: "block_trashes".to_string(),
                command: None,
                status: "applied".to_string(),
                detail: Some(trashes.display().to_string()),
            });
        } else {
            steps.push(ShieldStepResult {
                step: "block_trashes".to_string(),
                command: None,
                status: "skipped".to_string(),
                detail: Some("non-empty .Trashes directory already exists".to_string()),
            });
        }
    }

    // Step 5: tmutil addexclusion <mountpoint>
    let tmutil_cmd = format!("tmutil addexclusion {mp}");
    if opts.dry_run {
        steps.push(ShieldStepResult {
            step: "timemachine_exclusion".to_string(),
            command: Some(tmutil_cmd),
            status: "planned".to_string(),
            detail: None,
        });
    } else {
        let out = Command::new("tmutil")
            .args(["addexclusion", &mp])
            .output()?;
        if !out.status.success() {
            return Err(CaissonError::CommandFailed {
                cmd: tmutil_cmd,
                code: out.status.code(),
                stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
            });
        }
        steps.push(ShieldStepResult {
            step: "timemachine_exclusion".to_string(),
            command: Some(tmutil_cmd),
            status: "applied".to_string(),
            detail: None,
        });
    }

    // Step 6: mount -u -o noatime[,nobrowse] <mountpoint>
    let mount_opts = if opts.nobrowse {
        "noatime,nobrowse"
    } else {
        "noatime"
    };
    let mount_cmd = format!("mount -u -o {mount_opts} {mp}");
    if opts.dry_run {
        steps.push(ShieldStepResult {
            step: "mount_flags".to_string(),
            command: Some(mount_cmd),
            status: "planned".to_string(),
            detail: None,
        });
    } else {
        let out = Command::new("mount")
            .args(["-u", "-o", mount_opts, &mp])
            .output()?;
        if !out.status.success() {
            return Err(CaissonError::CommandFailed {
                cmd: mount_cmd,
                code: out.status.code(),
                stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
            });
        }
        steps.push(ShieldStepResult {
            step: "mount_flags".to_string(),
            command: Some(mount_cmd),
            status: "applied".to_string(),
            detail: Some(mount_opts.to_string()),
        });
    }

    // Step 7: chown <invoking-user>:staff <mountpoint>
    if let Some(ref owner) = opts.owner {
        let spec = format!("{owner}:staff");
        let chown_cmd = format!("chown {spec} {mp}");
        if opts.dry_run {
            steps.push(ShieldStepResult {
                step: "chown_mount".to_string(),
                command: Some(chown_cmd),
                status: "planned".to_string(),
                detail: None,
            });
        } else {
            let out = Command::new("chown").args([&spec, &mp]).output()?;
            if !out.status.success() {
                return Err(CaissonError::CommandFailed {
                    cmd: chown_cmd,
                    code: out.status.code(),
                    stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
                });
            }
            steps.push(ShieldStepResult {
                step: "chown_mount".to_string(),
                command: Some(chown_cmd),
                status: "applied".to_string(),
                detail: Some(spec),
            });
        }
    }

    Ok(ShieldApplyReport {
        mount_point: mp,
        dry_run: opts.dry_run,
        steps,
    })
}

fn write_readonly_marker(path: &Path) -> Result<()> {
    if path.exists() {
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o644));
    }
    fs::write(path, b"")?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o444))?;
    Ok(())
}

/// Inspect shield health for a mounted volume (100% read-only, unprivileged).
pub fn inspect_shields(mount_point: &Path) -> ShieldStatus {
    let mp = mount_point.display().to_string();
    if !mount_point.exists() {
        return ShieldStatus {
            mount_point: mp,
            mounted: false,
            metadata_never_index: false,
            fseventsd_no_log: false,
            trashes_blocked: false,
            noatime: false,
            nobrowse: false,
            spotlight_disabled: false,
            tm_excluded: false,
            all_healthy: false,
        };
    }

    let metadata_never_index = mount_point.join(".metadata_never_index").exists();
    let fseventsd_no_log = mount_point.join(".fseventsd").join("no_log").exists();
    let trashes_blocked = fs::symlink_metadata(mount_point.join(".Trashes"))
        .map(|m| m.is_file() && m.len() == 0)
        .unwrap_or(false);

    let (noatime, nobrowse) = match statfs_path(mount_point) {
        Ok(info) => (info.noatime, info.nobrowse),
        Err(_) => (false, false),
    };

    let spotlight_disabled = Command::new("mdutil")
        .args(["-s", &mp])
        .output()
        .ok()
        .map(|o| parse_mdutil_disabled(&String::from_utf8_lossy(&o.stdout)))
        .unwrap_or(false);

    let tm_excluded = Command::new("tmutil")
        .args(["isexcluded", &mp])
        .output()
        .ok()
        .map(|o| parse_tmutil_excluded(&String::from_utf8_lossy(&o.stdout)))
        .unwrap_or(false);

    let all_healthy = metadata_never_index
        && fseventsd_no_log
        && trashes_blocked
        && noatime
        && spotlight_disabled
        && tm_excluded;

    ShieldStatus {
        mount_point: mp,
        mounted: true,
        metadata_never_index,
        fseventsd_no_log,
        trashes_blocked,
        noatime,
        nobrowse,
        spotlight_disabled,
        tm_excluded,
        all_healthy,
    }
}

pub fn parse_mdutil_disabled(output: &str) -> bool {
    let lower = output.to_ascii_lowercase();
    lower.contains("indexing disabled") || lower.contains("indexing and searching disabled")
}

pub fn parse_tmutil_excluded(output: &str) -> bool {
    output.contains("[Excluded]")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn parses_mdutil_and_tmutil_outputs() {
        assert!(parse_mdutil_disabled(
            "/Volumes/caisson:\n\tIndexing disabled.\n"
        ));
        assert!(parse_mdutil_disabled(
            "/Volumes/caisson:\n\tIndexing and searching disabled.\n"
        ));
        assert!(!parse_mdutil_disabled("/:\n\tIndexing enabled.\n"));

        assert!(parse_tmutil_excluded("[Excluded]    /Volumes/caisson\n"));
        assert!(!parse_tmutil_excluded("[Included]    /\n"));
    }

    #[test]
    fn dry_run_plans_all_seven_shield_steps_without_touching_disk() {
        let dir = tempdir().unwrap();
        let report = apply_shields(
            dir.path(),
            &ShieldOptions {
                nobrowse: true,
                owner: Some("dev".to_string()),
                dry_run: true,
            },
        )
        .unwrap();

        assert!(report.dry_run);
        assert_eq!(report.steps.len(), 7);
        assert!(report.steps.iter().all(|s| s.status == "planned"));
        assert!(report.steps.iter().any(|s| s
            .command
            .as_deref()
            .unwrap_or("")
            .contains("noatime,nobrowse")));
        // Confirm dry-run created no marker files.
        assert!(!dir.path().join(".metadata_never_index").exists());
        assert!(!dir.path().join(".fseventsd").exists());
        assert!(!dir.path().join(".Trashes").exists());
    }

    #[test]
    fn inspect_shields_detects_local_marker_files() {
        let dir = tempdir().unwrap();
        let before = inspect_shields(dir.path());
        assert!(before.mounted);
        assert!(!before.metadata_never_index);
        assert!(!before.fseventsd_no_log);
        assert!(!before.trashes_blocked);

        write_readonly_marker(&dir.path().join(".metadata_never_index")).unwrap();
        fs::create_dir_all(dir.path().join(".fseventsd")).unwrap();
        fs::write(dir.path().join(".fseventsd/no_log"), b"").unwrap();
        write_readonly_marker(&dir.path().join(".Trashes")).unwrap();

        let after = inspect_shields(dir.path());
        assert!(after.metadata_never_index);
        assert!(after.fseventsd_no_log);
        assert!(after.trashes_blocked);
    }
}
