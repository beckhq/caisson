use crate::error::{CaissonError, Result};
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// Darwin `clonefile(2)` flag: do not follow symbolic links when cloning.
pub const CLONE_NOFOLLOW: u32 = 0x0001;

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn clonefile(src: *const libc::c_char, dst: *const libc::c_char, flags: u32) -> libc::c_int;
}

/// Perform a native APFS Copy-on-Write clone via Darwin `clonefile(2)` with `CLONE_NOFOLLOW`.
///
/// Works on regular files, symlinks, and entire directory trees in O(1) metadata time on the same
/// APFS volume. Returns `CaissonError::CrossDeviceClone` with actionable guidance on `EXDEV`.
#[cfg(target_os = "macos")]
pub fn clone_path(src: &Path, dst: &Path) -> Result<()> {
    // Verify source exists (without following symlinks so symlinks themselves can be cloned).
    std::fs::symlink_metadata(src)?;

    let c_src = CString::new(src.as_os_str().as_bytes()).map_err(|_| {
        CaissonError::Message(format!(
            "source path contains interior NUL byte: {}",
            src.display()
        ))
    })?;
    let c_dst = CString::new(dst.as_os_str().as_bytes()).map_err(|_| {
        CaissonError::Message(format!(
            "destination path contains interior NUL byte: {}",
            dst.display()
        ))
    })?;

    let rc = unsafe { clonefile(c_src.as_ptr(), c_dst.as_ptr(), CLONE_NOFOLLOW) };
    if rc == 0 {
        return Ok(());
    }

    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::EXDEV) {
        return Err(CaissonError::CrossDeviceClone {
            src: src.to_path_buf(),
            dst: dst.to_path_buf(),
        });
    }
    Err(CaissonError::Io(err))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;
    use tempfile::tempdir;

    #[test]
    fn clone_file_is_copy_on_write_independent() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("original.txt");
        let dst = dir.path().join("cloned.txt");
        fs::write(&src, "hello apfs cow").unwrap();

        clone_path(&src, &dst).unwrap();
        assert_eq!(fs::read_to_string(&dst).unwrap(), "hello apfs cow");

        // Mutating the clone must not alter the source.
        fs::write(&dst, "mutated clone").unwrap();
        assert_eq!(fs::read_to_string(&src).unwrap(), "hello apfs cow");
        assert_eq!(fs::read_to_string(&dst).unwrap(), "mutated clone");
    }

    #[test]
    fn clone_directory_tree_recursively_on_apfs() {
        let dir = tempdir().unwrap();
        let src_dir = dir.path().join("node_modules");
        let nested = src_dir.join("pkg").join("dist");
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("index.js"), "export const x = 42;\n").unwrap();

        let dst_dir = dir.path().join("node_modules_clone");
        clone_path(&src_dir, &dst_dir).unwrap();

        let cloned_file = dst_dir.join("pkg").join("dist").join("index.js");
        assert_eq!(
            fs::read_to_string(&cloned_file).unwrap(),
            "export const x = 42;\n"
        );

        // Mutating cloned file keeps original intact.
        fs::write(&cloned_file, "export const x = 99;\n").unwrap();
        assert_eq!(
            fs::read_to_string(nested.join("index.js")).unwrap(),
            "export const x = 42;\n"
        );
    }

    #[test]
    fn clone_symlink_uses_nofollow() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("target.txt");
        fs::write(&target, "target-content").unwrap();
        let link = dir.path().join("link.txt");
        symlink(&target, &link).unwrap();

        let cloned_link = dir.path().join("cloned_link.txt");
        clone_path(&link, &cloned_link).unwrap();

        let meta = fs::symlink_metadata(&cloned_link).unwrap();
        assert!(meta.file_type().is_symlink());
        assert_eq!(fs::read_link(&cloned_link).unwrap(), target);
    }

    #[test]
    fn clone_fails_when_destination_exists_or_source_missing() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("missing.txt");
        let dst = dir.path().join("dst.txt");
        assert!(clone_path(&missing, &dst).is_err());

        let src = dir.path().join("src.txt");
        fs::write(&src, "data").unwrap();
        fs::write(&dst, "already-there").unwrap();
        let err = clone_path(&src, &dst).unwrap_err();
        assert!(matches!(err, CaissonError::Io(_)));
    }
}
