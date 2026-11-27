use crate::error::{CaissonError, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use uuid::Uuid;

pub const DEFAULT_REGISTRY_PATH: &str = "/Library/Application Support/caisson/registry.json";
pub const VOLUME_MARKER_FILENAME: &str = ".caisson-volume.json";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum VolumeTier {
    Tier1,
    Tier2,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RegisteredVolume {
    pub name: String,
    pub uuid: String,
    pub container: String,
    pub mount_point: String,
    pub tier: VolumeTier,
    pub case_sensitive: bool,
    pub nobrowse: bool,
    pub quota_bytes: Option<u64>,
    pub owner: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// On-volume ownership marker written at `<mountpoint>/.caisson-volume.json`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VolumeMarker {
    pub schema_version: u32,
    pub managed_by: String,
    pub name: String,
    pub uuid: String,
    pub container: String,
    pub tier: VolumeTier,
    pub created_at: DateTime<Utc>,
}

impl VolumeMarker {
    pub fn from_registered(vol: &RegisteredVolume) -> Self {
        Self {
            schema_version: 1,
            managed_by: "caisson".to_string(),
            name: vol.name.clone(),
            uuid: vol.uuid.clone(),
            container: vol.container.clone(),
            tier: vol.tier,
            created_at: vol.created_at,
        }
    }

    pub fn path_for(mount_point: &Path) -> PathBuf {
        mount_point.join(VOLUME_MARKER_FILENAME)
    }

    /// Read `.caisson-volume.json` strictly without following symlinks on either the mount root
    /// or the marker file itself, ensuring the marker is a regular file on that directory.
    pub fn read_from(mount_point: &Path) -> Result<Self> {
        let dir_fd = open_dir_nofollow(mount_point)?;
        let c_name = CString::new(VOLUME_MARKER_FILENAME)
            .map_err(|_| CaissonError::Message("invalid marker filename".to_string()))?;

        let raw_fd = unsafe {
            libc::openat(
                dir_fd.as_raw_fd(),
                c_name.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        if raw_fd < 0 {
            return Err(CaissonError::Io(std::io::Error::last_os_error()));
        }
        let owned = unsafe { OwnedFd::from_raw_fd(raw_fd) };
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(owned.as_raw_fd(), &mut st) } != 0 {
            return Err(CaissonError::Io(std::io::Error::last_os_error()));
        }
        if (st.st_mode & libc::S_IFMT) != libc::S_IFREG {
            return Err(CaissonError::Message(format!(
                "ownership marker '{}' is not a regular file",
                Self::path_for(mount_point).display()
            )));
        }

        let mut file = File::from(owned);
        let mut raw = String::new();
        file.read_to_string(&mut raw)?;
        let marker: Self = serde_json::from_str(&raw)?;
        Ok(marker)
    }

    /// Write `.caisson-volume.json` inside `mount_point` without following symlinks.
    pub fn write_to(&self, mount_point: &Path) -> Result<PathBuf> {
        let dir_fd = open_dir_nofollow(mount_point)?;
        let c_name = CString::new(VOLUME_MARKER_FILENAME)
            .map_err(|_| CaissonError::Message("invalid marker filename".to_string()))?;

        // Refuse if existing entry is a symlink or non-regular file.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe {
            libc::fstatat(
                dir_fd.as_raw_fd(),
                c_name.as_ptr(),
                &mut st,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } == 0
            && (st.st_mode & libc::S_IFMT) != libc::S_IFREG
        {
            return Err(CaissonError::Message(format!(
                "refusing to write marker over symlink or non-regular file at '{}'",
                Self::path_for(mount_point).display()
            )));
        }

        let raw_fd = unsafe {
            libc::openat(
                dir_fd.as_raw_fd(),
                c_name.as_ptr(),
                libc::O_WRONLY
                    | libc::O_CREAT
                    | libc::O_TRUNC
                    | libc::O_NOFOLLOW
                    | libc::O_NONBLOCK
                    | libc::O_CLOEXEC,
                0o644 as libc::c_uint,
            )
        };
        if raw_fd < 0 {
            return Err(CaissonError::Io(std::io::Error::last_os_error()));
        }
        let owned = unsafe { OwnedFd::from_raw_fd(raw_fd) };
        let json = serde_json::to_string_pretty(self)?;
        let mut file = File::from(owned);
        file.write_all(format!("{json}\n").as_bytes())?;
        file.sync_all()?;
        let _ = unsafe { libc::fchmod(file.as_raw_fd(), 0o644) };
        Ok(Self::path_for(mount_point))
    }
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VolumeRegistry {
    pub schema_version: u32,
    pub volumes: Vec<RegisteredVolume>,
}

impl Default for VolumeRegistry {
    fn default() -> Self {
        Self {
            schema_version: 1,
            volumes: Vec::new(),
        }
    }
}

struct AdvisoryLock {
    _file: File,
}

impl AdvisoryLock {
    fn acquire(lock_path: &Path) -> Result<Self> {
        if let Some(parent) = lock_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(lock_path)?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(CaissonError::Io(std::io::Error::last_os_error()));
        }
        Ok(Self { _file: file })
    }
}

impl Drop for AdvisoryLock {
    fn drop(&mut self) {
        let _ = unsafe { libc::flock(self._file.as_raw_fd(), libc::LOCK_UN) };
    }
}

impl VolumeRegistry {
    pub fn default_path() -> PathBuf {
        if let Ok(override_path) = std::env::var("CAISSON_REGISTRY") {
            if !override_path.trim().is_empty() {
                return PathBuf::from(override_path);
            }
        }
        PathBuf::from(DEFAULT_REGISTRY_PATH)
    }

    pub fn load_default() -> Result<Self> {
        Self::load_from(&Self::default_path())
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = fs::read_to_string(path)?;
        let reg: Self = serde_json::from_str(&raw)?;
        Ok(reg)
    }

    /// Execute a locked load -> modify -> atomic save transaction on `path` so concurrent
    /// create/destroy operations cannot overwrite each other's updates.
    pub fn with_locked_update<T>(
        path: &Path,
        mutate: impl FnOnce(&mut Self) -> Result<T>,
    ) -> Result<T> {
        let lock_path = path.with_extension("json.lock");
        let _lock = AdvisoryLock::acquire(&lock_path)?;
        let mut reg = Self::load_from(path)?;
        let result = mutate(&mut reg)?;
        reg.save_to(path)?;
        Ok(result)
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        let parent = path.parent().ok_or_else(|| {
            CaissonError::Message(format!("invalid registry path: {}", path.display()))
        })?;
        fs::create_dir_all(parent)?;

        let json = serde_json::to_string_pretty(self)?;
        let unique_name = format!(
            ".registry.{}.{}.tmp",
            std::process::id(),
            Uuid::new_v4().simple()
        );
        let tmp_path = parent.join(unique_name);

        let mut tmp_file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .custom_flags(libc::O_NOFOLLOW)
            .mode(0o644)
            .open(&tmp_path)?;

        let write_result = (|| -> Result<()> {
            tmp_file.write_all(format!("{json}\n").as_bytes())?;
            tmp_file.sync_all()?;
            fs::set_permissions(&tmp_path, fs::Permissions::from_mode(0o644))?;
            Ok(())
        })();

        if let Err(err) = write_result {
            let _ = fs::remove_file(&tmp_path);
            return Err(err);
        }
        drop(tmp_file);

        if let Err(err) = fs::rename(&tmp_path, path) {
            let _ = fs::remove_file(&tmp_path);
            return Err(CaissonError::Io(err));
        }

        // Sync parent directory to persist rename metadata.
        if let Ok(dir) = File::open(parent) {
            let _ = dir.sync_all();
        }
        Ok(())
    }

    pub fn find_by_name(&self, name: &str) -> Option<&RegisteredVolume> {
        self.volumes.iter().find(|v| v.name == name)
    }

    pub fn find_by_uuid(&self, uuid: &str) -> Option<&RegisteredVolume> {
        self.volumes
            .iter()
            .find(|v| v.uuid.eq_ignore_ascii_case(uuid))
    }

    pub fn upsert(&mut self, vol: RegisteredVolume) {
        if let Some(existing) = self
            .volumes
            .iter_mut()
            .find(|v| v.uuid.eq_ignore_ascii_case(&vol.uuid) || v.name == vol.name)
        {
            *existing = vol;
        } else {
            self.volumes.push(vol);
        }
    }

    pub fn remove_by_uuid(&mut self, uuid: &str) -> Option<RegisteredVolume> {
        if let Some(pos) = self
            .volumes
            .iter()
            .position(|v| v.uuid.eq_ignore_ascii_case(uuid))
        {
            Some(self.volumes.remove(pos))
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use tempfile::tempdir;

    #[test]
    fn registry_and_marker_roundtrip() {
        let dir = tempdir().unwrap();
        let reg_path = dir.path().join("registry.json");
        let mount_dir = dir.path().join("vol");
        fs::create_dir_all(&mount_dir).unwrap();

        let vol = RegisteredVolume {
            name: "caisson".to_string(),
            uuid: "11111111-2222-3333-4444-555555555555".to_string(),
            container: "disk3".to_string(),
            mount_point: mount_dir.display().to_string(),
            tier: VolumeTier::Tier1,
            case_sensitive: false,
            nobrowse: false,
            quota_bytes: Some(250 * 1024 * 1024 * 1024),
            owner: Some("dev".to_string()),
            created_at: Utc::now(),
        };

        VolumeRegistry::with_locked_update(&reg_path, |reg| {
            reg.upsert(vol.clone());
            Ok(())
        })
        .unwrap();

        let loaded = VolumeRegistry::load_from(&reg_path).unwrap();
        assert_eq!(loaded.volumes.len(), 1);
        assert_eq!(loaded.find_by_name("caisson").unwrap().uuid, vol.uuid);
        assert!(loaded
            .find_by_uuid("11111111-2222-3333-4444-555555555555")
            .is_some());

        let marker = VolumeMarker::from_registered(&vol);
        marker.write_to(&mount_dir).unwrap();
        let read_marker = VolumeMarker::read_from(&mount_dir).unwrap();
        assert_eq!(read_marker.uuid, vol.uuid);
        assert_eq!(read_marker.managed_by, "caisson");
    }

    #[test]
    fn marker_read_and_write_reject_symlinks() {
        let dir = tempdir().unwrap();
        let mount_dir = dir.path().join("vol");
        fs::create_dir_all(&mount_dir).unwrap();
        let decoy = dir.path().join("decoy.json");
        fs::write(&decoy, r#"{"schema_version":1,"managed_by":"caisson","name":"x","uuid":"1","container":"disk3","tier":"tier1","created_at":"2026-01-01T00:00:00Z"}"#).unwrap();
        symlink(&decoy, mount_dir.join(VOLUME_MARKER_FILENAME)).unwrap();

        assert!(VolumeMarker::read_from(&mount_dir).is_err());
    }
}
