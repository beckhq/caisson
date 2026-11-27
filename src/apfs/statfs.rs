use crate::error::{CaissonError, Result};
use serde::Serialize;
use std::ffi::{CStr, CString};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct VolumeMountInfo {
    pub mount_point: String,
    pub device: String,
    pub fs_type: String,
    pub total_bytes: u64,
    pub free_bytes: u64,
    pub available_bytes: u64,
    pub used_bytes: u64,
    pub noatime: bool,
    pub nobrowse: bool,
    pub readonly: bool,
    pub dev_id: u64,
}

/// Inspect filesystem usage and mount flags for `path` via `statfs(2)`.
#[cfg(target_os = "macos")]
pub fn statfs_path(path: &Path) -> Result<VolumeMountInfo> {
    let c_path = CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        CaissonError::Message(format!(
            "path contains interior NUL byte: {}",
            path.display()
        ))
    })?;

    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statfs(c_path.as_ptr(), &mut st) };
    if rc != 0 {
        return Err(CaissonError::Io(std::io::Error::last_os_error()));
    }

    let bsize = st.f_bsize as u64;
    let total_bytes = st.f_blocks.saturating_mul(bsize);
    let free_bytes = st.f_bfree.saturating_mul(bsize);
    let available_bytes = st.f_bavail.saturating_mul(bsize);
    let used_bytes = total_bytes.saturating_sub(free_bytes);

    let flags = st.f_flags as i32;
    let noatime = (flags & libc::MNT_NOATIME) != 0;
    let nobrowse = (flags & libc::MNT_DONTBROWSE) != 0;
    let readonly = (flags & libc::MNT_RDONLY) != 0;

    let fs_type = unsafe { CStr::from_ptr(st.f_fstypename.as_ptr()) }
        .to_string_lossy()
        .into_owned();
    let mount_point = unsafe { CStr::from_ptr(st.f_mntonname.as_ptr()) }
        .to_string_lossy()
        .into_owned();
    let device = unsafe { CStr::from_ptr(st.f_mntfromname.as_ptr()) }
        .to_string_lossy()
        .into_owned();

    let dev_id = std::fs::symlink_metadata(path)?.dev();

    Ok(VolumeMountInfo {
        mount_point,
        device,
        fs_type,
        total_bytes,
        free_bytes,
        available_bytes,
        used_bytes,
        noatime,
        nobrowse,
        readonly,
        dev_id,
    })
}

/// Check whether two paths (or their nearest existing ancestors) reside on the same volume.
pub fn same_volume(a: &Path, b: &Path) -> Result<bool> {
    let a_existing = nearest_existing_ancestor(a)?;
    let b_existing = nearest_existing_ancestor(b)?;
    let dev_a = std::fs::symlink_metadata(&a_existing)?.dev();
    let dev_b = std::fs::symlink_metadata(&b_existing)?.dev();
    Ok(dev_a == dev_b)
}

fn nearest_existing_ancestor(path: &Path) -> Result<std::path::PathBuf> {
    let mut cur = path.to_path_buf();
    loop {
        if cur.exists() {
            return Ok(cur);
        }
        if !cur.pop() {
            return Err(CaissonError::Message(format!(
                "no existing ancestor found for {}",
                path.display()
            )));
        }
    }
}

/// Return true if the current process has effective UID 0 (root).
pub fn is_root() -> bool {
    unsafe { libc::geteuid() == 0 }
}

/// Determine the non-root invoking developer username (prefers `SUDO_USER` over `USER`).
pub fn invoking_user() -> Option<String> {
    if let Ok(sudo_user) = std::env::var("SUDO_USER") {
        let trimmed = sudo_user.trim();
        if !trimmed.is_empty() && trimmed != "root" {
            return Some(trimmed.to_string());
        }
    }
    if let Ok(user) = std::env::var("USER") {
        let trimmed = user.trim();
        if !trimmed.is_empty() && trimmed != "root" {
            return Some(trimmed.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn statfs_root_reports_apfs_and_nonzero_capacity() {
        let info = statfs_path(Path::new("/")).unwrap();
        assert_eq!(info.fs_type, "apfs");
        assert!(info.total_bytes > 0);
        assert_eq!(info.mount_point, "/");
    }

    #[test]
    fn same_volume_detects_sibling_tempdirs() {
        let a = tempdir().unwrap();
        let b = tempdir().unwrap();
        assert!(same_volume(a.path(), b.path()).unwrap());
        // Non-existent child under existing parent resolves cleanly.
        assert!(same_volume(&a.path().join("does/not/exist"), b.path()).unwrap());
    }
}
