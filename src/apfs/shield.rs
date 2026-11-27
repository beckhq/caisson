use crate::apfs::statfs::statfs_path;
use crate::error::{CaissonError, Result};
use serde::Serialize;
use std::ffi::{CStr, CString};
use std::fs;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
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
    /// True when `.fseventsd` exists with root-only `0700` permissions and cannot be traversed
    /// by an unprivileged user (does not mark shields as degraded).
    pub fseventsd_no_log_unverified: bool,
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
/// Opens `mount_point` once with `O_DIRECTORY | O_NOFOLLOW` and performs all marker/directory/owner
/// mutations via descriptor-relative syscalls (`openat`, `fstatat`, `mkdirat`, `unlinkat`,
/// `fchmod`, `fchown`) so root execution never follows attacker-planted symlinks.
pub fn apply_shields(mount_point: &Path, opts: &ShieldOptions) -> Result<ShieldApplyReport> {
    let mp = mount_point.display().to_string();
    let mut steps = Vec::new();

    if opts.dry_run {
        let never_index = mount_point.join(".metadata_never_index");
        let fseventsd_dir = mount_point.join(".fseventsd");
        let no_log = fseventsd_dir.join("no_log");
        let trashes = mount_point.join(".Trashes");
        let mount_opts = if opts.nobrowse {
            "noatime,nobrowse"
        } else {
            "noatime"
        };

        steps.push(ShieldStepResult {
            step: "spotlight_mdutil".to_string(),
            command: Some(format!("mdutil -i off {mp}")),
            status: "planned".to_string(),
            detail: None,
        });
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
        steps.push(ShieldStepResult {
            step: "block_trashes".to_string(),
            command: Some(format!(
                "rmdir {} && touch {} && chmod 0444 {}",
                trashes.display(),
                trashes.display(),
                trashes.display()
            )),
            status: "planned".to_string(),
            detail: None,
        });
        steps.push(ShieldStepResult {
            step: "timemachine_exclusion".to_string(),
            command: Some(format!("tmutil addexclusion -v {mp}")),
            status: "planned".to_string(),
            detail: None,
        });
        steps.push(ShieldStepResult {
            step: "mount_flags".to_string(),
            command: Some(format!("mount -u -o {mount_opts} {mp}")),
            status: "planned".to_string(),
            detail: None,
        });
        if let Some(ref owner) = opts.owner {
            steps.push(ShieldStepResult {
                step: "chown_mount".to_string(),
                command: Some(format!("chown {owner}:staff {mp}")),
                status: "planned".to_string(),
                detail: None,
            });
        }
        return Ok(ShieldApplyReport {
            mount_point: mp,
            dry_run: true,
            steps,
        });
    }

    // Open mount root once without following symlinks.
    let root_fd = open_dir_nofollow(mount_point)?;

    // Step 1: mdutil -i off <mountpoint>
    let mdutil_cmd = format!("mdutil -i off {mp}");
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

    // Step 2: <mountpoint>/.metadata_never_index (0444, no-follow)
    write_readonly_marker_at(&root_fd, ".metadata_never_index")?;
    steps.push(ShieldStepResult {
        step: "metadata_never_index".to_string(),
        command: None,
        status: "applied".to_string(),
        detail: Some(
            mount_point
                .join(".metadata_never_index")
                .display()
                .to_string(),
        ),
    });

    // Step 3: <mountpoint>/.fseventsd/no_log (descriptor-relative, no-follow)
    ensure_fseventsd_no_log_at(&root_fd)?;
    steps.push(ShieldStepResult {
        step: "fseventsd_no_log".to_string(),
        command: None,
        status: "applied".to_string(),
        detail: Some(mount_point.join(".fseventsd/no_log").display().to_string()),
    });

    // Step 4: <mountpoint>/.Trashes (0-byte regular file, mode 0444; rmdir-only removal of empty dirs)
    let trashes_replaced = ensure_trashes_blocked_at(&root_fd)?;
    if trashes_replaced {
        steps.push(ShieldStepResult {
            step: "block_trashes".to_string(),
            command: None,
            status: "applied".to_string(),
            detail: Some(mount_point.join(".Trashes").display().to_string()),
        });
    } else {
        steps.push(ShieldStepResult {
            step: "block_trashes".to_string(),
            command: None,
            status: "skipped".to_string(),
            detail: Some(
                "non-empty .Trashes directory containing files already exists".to_string(),
            ),
        });
    }

    // Step 5: tmutil addexclusion -v <mountpoint> (volume UUID exclusion; requires root + Full Disk Access)
    let tmutil_cmd = format!("tmutil addexclusion -v {mp}");
    let out = Command::new("tmutil")
        .args(["addexclusion", "-v", &mp])
        .output()?;
    if out.status.success() {
        steps.push(ShieldStepResult {
            step: "timemachine_exclusion".to_string(),
            command: Some(tmutil_cmd),
            status: "applied".to_string(),
            detail: None,
        });
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let detail = format!(
            "{stderr} (Note: `tmutil addexclusion -v` requires Full Disk Access for your terminal in System Settings -> Privacy & Security -> Full Disk Access; re-run `sudo caisson vol shield` after granting FDA)"
        );
        steps.push(ShieldStepResult {
            step: "timemachine_exclusion".to_string(),
            command: Some(tmutil_cmd),
            status: "warning".to_string(),
            detail: Some(detail),
        });
    }

    // Step 6: mount -u -o noatime[,nobrowse] <mountpoint>
    let mount_opts = if opts.nobrowse {
        "noatime,nobrowse"
    } else {
        "noatime"
    };
    let mount_cmd = format!("mount -u -o {mount_opts} {mp}");
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

    // Step 7: fchown <invoking-user>:staff on the opened mount root descriptor
    if let Some(ref owner) = opts.owner {
        let spec = format!("{owner}:staff");
        fchown_user_staff(&root_fd, owner)?;
        steps.push(ShieldStepResult {
            step: "chown_mount".to_string(),
            command: Some(format!("chown {spec} {mp}")),
            status: "applied".to_string(),
            detail: Some(spec),
        });
    }

    Ok(ShieldApplyReport {
        mount_point: mp,
        dry_run: false,
        steps,
    })
}

fn open_dir_nofollow(path: &Path) -> Result<OwnedFd> {
    let c_path = CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        CaissonError::Message(format!("path contains interior NUL: {}", path.display()))
    })?;
    let fd = unsafe {
        libc::open(
            c_path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(CaissonError::Io(std::io::Error::last_os_error()));
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn open_subdir_nofollow(parent_fd: &OwnedFd, name: &str) -> Result<OwnedFd> {
    let c_name = CString::new(name)
        .map_err(|_| CaissonError::Message(format!("invalid directory name: {name}")))?;
    let fd = unsafe {
        libc::openat(
            parent_fd.as_raw_fd(),
            c_name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(CaissonError::Io(std::io::Error::last_os_error()));
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Create or update a 0-byte read-only (`0444`) regular file relative to `dir_fd`, strictly
/// rejecting symlinks or non-regular entries.
fn write_readonly_marker_at(dir_fd: &OwnedFd, name: &str) -> Result<()> {
    let c_name = CString::new(name)
        .map_err(|_| CaissonError::Message(format!("invalid marker name: {name}")))?;

    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    let exists = unsafe {
        libc::fstatat(
            dir_fd.as_raw_fd(),
            c_name.as_ptr(),
            &mut st,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } == 0;

    if exists {
        let ftype = st.st_mode & libc::S_IFMT;
        if ftype != libc::S_IFREG {
            return Err(CaissonError::Message(format!(
                "refusing to modify symlink or non-regular entry '{name}'"
            )));
        }
        // Temporarily make writable (no-follow) so non-root unit tests or existing 0444 files open cleanly.
        let _ = unsafe {
            libc::fchmodat(
                dir_fd.as_raw_fd(),
                c_name.as_ptr(),
                0o644,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
    }

    let raw_fd = unsafe {
        libc::openat(
            dir_fd.as_raw_fd(),
            c_name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            0o644 as libc::c_uint,
        )
    };
    if raw_fd < 0 {
        return Err(CaissonError::Io(std::io::Error::last_os_error()));
    }
    let file_fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };

    let mut fst: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(file_fd.as_raw_fd(), &mut fst) } != 0 {
        return Err(CaissonError::Io(std::io::Error::last_os_error()));
    }
    if (fst.st_mode & libc::S_IFMT) != libc::S_IFREG {
        return Err(CaissonError::Message(format!(
            "refusing to truncate non-regular file '{name}'"
        )));
    }
    if unsafe { libc::ftruncate(file_fd.as_raw_fd(), 0) } != 0 {
        return Err(CaissonError::Io(std::io::Error::last_os_error()));
    }
    if unsafe { libc::fchmod(file_fd.as_raw_fd(), 0o444) } != 0 {
        return Err(CaissonError::Io(std::io::Error::last_os_error()));
    }
    Ok(())
}

/// Create `.fseventsd` directory (rejecting symlinks) and `.fseventsd/no_log` regular file.
fn ensure_fseventsd_no_log_at(root_fd: &OwnedFd) -> Result<()> {
    let c_dir = CString::new(".fseventsd").unwrap();
    let rc = unsafe { libc::mkdirat(root_fd.as_raw_fd(), c_dir.as_ptr(), 0o755) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::EEXIST) {
            return Err(CaissonError::Io(err));
        }
    }

    // Open `.fseventsd` with O_DIRECTORY | O_NOFOLLOW (fails immediately if symlink).
    let fs_fd = open_subdir_nofollow(root_fd, ".fseventsd").map_err(|e| {
        CaissonError::Message(format!(
            "refusing to enter `.fseventsd` (symlink or invalid directory): {e}"
        ))
    })?;

    // Allow unprivileged `stat(".fseventsd/no_log")` via execute/search bit (`0711`) while
    // keeping `.fseventsd` non-listable and root-only writable.
    if unsafe { libc::fchmod(fs_fd.as_raw_fd(), 0o711) } != 0 {
        return Err(CaissonError::Io(std::io::Error::last_os_error()));
    }

    let c_nolog = CString::new("no_log").unwrap();
    let raw_fd = unsafe {
        libc::openat(
            fs_fd.as_raw_fd(),
            c_nolog.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            0o644 as libc::c_uint,
        )
    };
    if raw_fd < 0 {
        return Err(CaissonError::Io(std::io::Error::last_os_error()));
    }
    let file_fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };
    let mut fst: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(file_fd.as_raw_fd(), &mut fst) } != 0 {
        return Err(CaissonError::Io(std::io::Error::last_os_error()));
    }
    if (fst.st_mode & libc::S_IFMT) != libc::S_IFREG {
        return Err(CaissonError::Message(
            "refusing to use non-regular `.fseventsd/no_log`".to_string(),
        ));
    }
    Ok(())
}

/// Replace an empty `.Trashes` directory tree (only empty subdirectories, removed exclusively via
/// atomic `unlinkat(..., AT_REMOVEDIR)`) with a 0-byte `0444` regular file.
///
/// Returns `Ok(true)` if `.Trashes` is now a 0-byte read-only file, or `Ok(false)` if `.Trashes`
/// contains user files and was safely left untouched. Rejects symlinks with an error.
fn ensure_trashes_blocked_at(root_fd: &OwnedFd) -> Result<bool> {
    let c_trashes = CString::new(".Trashes").unwrap();
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    let exists = unsafe {
        libc::fstatat(
            root_fd.as_raw_fd(),
            c_trashes.as_ptr(),
            &mut st,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } == 0;

    if exists {
        match st.st_mode & libc::S_IFMT {
            libc::S_IFREG => {
                write_readonly_marker_at(root_fd, ".Trashes")?;
                return Ok(true);
            }
            libc::S_IFLNK => {
                return Err(CaissonError::Message(
                    "refusing to modify symlinked `.Trashes`".to_string(),
                ));
            }
            libc::S_IFDIR => {
                if !remove_empty_dir_tree_at(root_fd, ".Trashes")? {
                    return Ok(false);
                }
            }
            _ => {
                return Err(CaissonError::Message(
                    "refusing to modify non-regular `.Trashes` entry".to_string(),
                ));
            }
        }
    }

    write_readonly_marker_at(root_fd, ".Trashes")?;
    Ok(true)
}

/// Remove a directory tree ONLY if every node inside it is an empty directory, using
/// `openat(O_DIRECTORY | O_NOFOLLOW)` and `unlinkat(AT_REMOVEDIR)` exclusively.
/// Never calls `unlink()` on any file or symlink; if any file exists or races in, `rmdir`
/// atomically fails with `ENOTEMPTY` and returns `Ok(false)`.
fn remove_empty_dir_tree_at(parent_fd: &OwnedFd, name: &str) -> Result<bool> {
    let dir_fd = match open_subdir_nofollow(parent_fd, name) {
        Ok(fd) => fd,
        Err(_) => return Ok(false),
    };

    // Duplicate fd because `fdopendir` takes ownership of the fd it receives.
    let dup_fd = unsafe { libc::fcntl(dir_fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
    if dup_fd < 0 {
        return Err(CaissonError::Io(std::io::Error::last_os_error()));
    }
    let dir_ptr = unsafe { libc::fdopendir(dup_fd) };
    if dir_ptr.is_null() {
        unsafe { libc::close(dup_fd) };
        return Err(CaissonError::Io(std::io::Error::last_os_error()));
    }

    let mut subdirs: Vec<String> = Vec::new();
    let mut has_non_dir = false;

    loop {
        let ent = unsafe { libc::readdir(dir_ptr) };
        if ent.is_null() {
            break;
        }
        let cname = unsafe { CStr::from_ptr((*ent).d_name.as_ptr()) };
        let Ok(entry_name) = cname.to_str() else {
            has_non_dir = true;
            break;
        };
        if entry_name == "." || entry_name == ".." {
            continue;
        }
        let mut child_st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe {
            libc::fstatat(
                dir_fd.as_raw_fd(),
                cname.as_ptr(),
                &mut child_st,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            has_non_dir = true;
            break;
        }
        if (child_st.st_mode & libc::S_IFMT) == libc::S_IFDIR {
            subdirs.push(entry_name.to_string());
        } else {
            has_non_dir = true;
            break;
        }
    }
    unsafe { libc::closedir(dir_ptr) };

    if has_non_dir {
        return Ok(false);
    }

    for sub in subdirs {
        if !remove_empty_dir_tree_at(&dir_fd, &sub)? {
            return Ok(false);
        }
    }
    drop(dir_fd);

    let c_name = CString::new(name)
        .map_err(|_| CaissonError::Message(format!("invalid directory name: {name}")))?;
    if unsafe { libc::unlinkat(parent_fd.as_raw_fd(), c_name.as_ptr(), libc::AT_REMOVEDIR) } != 0 {
        let err = std::io::Error::last_os_error();
        if matches!(err.raw_os_error(), Some(libc::ENOTEMPTY | libc::EEXIST)) {
            return Ok(false);
        }
        return Err(CaissonError::Io(err));
    }
    Ok(true)
}

/// Apply `fchown(root_fd, uid, gid)` for `<user>:staff` directly on the open directory descriptor.
fn fchown_user_staff(root_fd: &OwnedFd, username: &str) -> Result<()> {
    let c_user = CString::new(username)
        .map_err(|_| CaissonError::Message(format!("invalid username: {username}")))?;
    let c_group = CString::new("staff").unwrap();

    let pw = unsafe { libc::getpwnam(c_user.as_ptr()) };
    if pw.is_null() {
        return Err(CaissonError::Message(format!(
            "unknown macOS user '{username}' for mount ownership"
        )));
    }
    let uid = unsafe { (*pw).pw_uid };

    let gr = unsafe { libc::getgrnam(c_group.as_ptr()) };
    let gid = if gr.is_null() {
        unsafe { (*pw).pw_gid }
    } else {
        unsafe { (*gr).gr_gid }
    };

    if unsafe { libc::fchown(root_fd.as_raw_fd(), uid, gid) } != 0 {
        return Err(CaissonError::Io(std::io::Error::last_os_error()));
    }
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
            fseventsd_no_log_unverified: false,
            trashes_blocked: false,
            noatime: false,
            nobrowse: false,
            spotlight_disabled: false,
            tm_excluded: false,
            all_healthy: false,
        };
    }

    let metadata_never_index = fs::symlink_metadata(mount_point.join(".metadata_never_index"))
        .map(|m| m.is_file())
        .unwrap_or(false);
    let fseventsd_meta = fs::symlink_metadata(mount_point.join(".fseventsd")).ok();
    let fseventsd_is_dir = fseventsd_meta.as_ref().map(|m| m.is_dir()).unwrap_or(false);
    let fseventsd_root_owned = fseventsd_meta
        .as_ref()
        .map(|m| std::os::unix::fs::MetadataExt::uid(m) == 0)
        .unwrap_or(false);
    let (fseventsd_no_log, fseventsd_no_log_unverified) = if !fseventsd_is_dir {
        (false, false)
    } else {
        match fs::symlink_metadata(mount_point.join(".fseventsd").join("no_log")) {
            Ok(m) => (m.is_file(), false),
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied && fseventsd_root_owned => {
                (false, true)
            }
            Err(_) => (false, false),
        }
    };
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

    let fseventsd_ok = fseventsd_no_log || fseventsd_no_log_unverified;
    let all_healthy = metadata_never_index
        && fseventsd_ok
        && trashes_blocked
        && noatime
        && spotlight_disabled
        && tm_excluded;

    ShieldStatus {
        mount_point: mp,
        mounted: true,
        metadata_never_index,
        fseventsd_no_log,
        fseventsd_no_log_unverified,
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
    use std::os::unix::fs::symlink;
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
        assert!(!dir.path().join(".metadata_never_index").exists());
        assert!(!dir.path().join(".fseventsd").exists());
        assert!(!dir.path().join(".Trashes").exists());
    }

    #[test]
    fn marker_helpers_reject_symlinks_without_truncating_targets() {
        let dir = tempdir().unwrap();
        let victim = dir.path().join("victim.txt");
        fs::write(&victim, "precious-root-file").unwrap();

        let mount = dir.path().join("mount");
        fs::create_dir_all(&mount).unwrap();
        symlink(&victim, mount.join(".metadata_never_index")).unwrap();
        symlink(&victim, mount.join(".Trashes")).unwrap();
        let external_dir = dir.path().join("external");
        fs::create_dir_all(&external_dir).unwrap();
        symlink(&external_dir, mount.join(".fseventsd")).unwrap();

        let root_fd = open_dir_nofollow(&mount).unwrap();
        assert!(write_readonly_marker_at(&root_fd, ".metadata_never_index").is_err());
        assert!(ensure_trashes_blocked_at(&root_fd).is_err());
        assert!(ensure_fseventsd_no_log_at(&root_fd).is_err());

        // Victim file MUST remain untouched and external dir MUST NOT have no_log!
        assert_eq!(fs::read_to_string(&victim).unwrap(), "precious-root-file");
        assert!(!external_dir.join("no_log").exists());
    }

    #[test]
    fn trashes_blocker_replaces_empty_subdirs_but_preserves_files() {
        let dir = tempdir().unwrap();
        let mount = dir.path().join("mount");
        fs::create_dir_all(mount.join(".Trashes/501")).unwrap();
        let root_fd = open_dir_nofollow(&mount).unwrap();

        assert!(ensure_trashes_blocked_at(&root_fd).unwrap());
        assert!(fs::symlink_metadata(mount.join(".Trashes"))
            .unwrap()
            .is_file());

        // Now test with a real file inside .Trashes/501 -> must NOT delete it!
        let mount2 = dir.path().join("mount2");
        fs::create_dir_all(mount2.join(".Trashes/501")).unwrap();
        fs::write(mount2.join(".Trashes/501/keep.txt"), "data").unwrap();
        let root_fd2 = open_dir_nofollow(&mount2).unwrap();

        assert!(!ensure_trashes_blocked_at(&root_fd2).unwrap());
        assert_eq!(
            fs::read_to_string(mount2.join(".Trashes/501/keep.txt")).unwrap(),
            "data"
        );
    }

    #[test]
    fn inspect_shields_detects_local_marker_files() {
        let dir = tempdir().unwrap();
        let before = inspect_shields(dir.path());
        assert!(before.mounted);
        assert!(!before.metadata_never_index);
        assert!(!before.fseventsd_no_log);
        assert!(!before.trashes_blocked);

        let root_fd = open_dir_nofollow(dir.path()).unwrap();
        write_readonly_marker_at(&root_fd, ".metadata_never_index").unwrap();
        ensure_fseventsd_no_log_at(&root_fd).unwrap();
        ensure_trashes_blocked_at(&root_fd).unwrap();

        let after = inspect_shields(dir.path());
        assert!(after.metadata_never_index);
        assert!(after.fseventsd_no_log);
        assert!(!after.fseventsd_no_log_unverified);
        assert!(after.trashes_blocked);

        // A non-root-owned unreadable `.fseventsd` directory must NOT be falsely classified as
        // macOS's root-only `fseventsd_no_log_unverified` state.
        let locked_dir = tempdir().unwrap();
        let fs_dir = locked_dir.path().join(".fseventsd");
        fs::create_dir_all(&fs_dir).unwrap();
        fs::write(fs_dir.join("no_log"), b"").unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&fs_dir, fs::Permissions::from_mode(0o000)).unwrap();
        let locked_status = inspect_shields(locked_dir.path());
        // Restore perms so tempdir cleans up cleanly.
        let _ = fs::set_permissions(&fs_dir, fs::Permissions::from_mode(0o755));
        if !crate::apfs::statfs::is_root() {
            assert!(!locked_status.fseventsd_no_log);
            assert!(!locked_status.fseventsd_no_log_unverified);
        }
    }
}
