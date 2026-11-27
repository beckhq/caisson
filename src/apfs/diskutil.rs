use crate::error::{CaissonError, Result};
use crate::registry::{RegisteredVolume, VolumeMarker, VolumeRegistry};
use crate::units::{format_diskutil_size, GIB};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;

/// APFS roles that must never be destroyed by Caisson (`ARCHITECTURE.md` §6).
pub const PROTECTED_APFS_ROLES: &[&str] = &[
    "System",
    "Data",
    "VM",
    "Preboot",
    "Recovery",
    "Update",
    "Hardware",
    "xART",
    "iSCPreboot",
    "Installer",
    "Backup",
    "Baseband",
    "Enterprise",
];

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ApfsListPlist {
    #[serde(rename = "Containers", default)]
    pub containers: Vec<ApfsContainer>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ApfsContainer {
    #[serde(rename = "APFSContainerUUID", default)]
    pub uuid: String,
    #[serde(rename = "ContainerReference")]
    pub container_reference: String,
    #[serde(rename = "CapacityCeiling", default)]
    pub capacity_ceiling: u64,
    #[serde(rename = "CapacityFree", default)]
    pub capacity_free: u64,
    #[serde(rename = "DesignatedPhysicalStore", default)]
    pub designated_physical_store: Option<String>,
    #[serde(rename = "PhysicalStores", default)]
    pub physical_stores: Vec<ApfsPhysicalStore>,
    #[serde(rename = "Volumes", default)]
    pub volumes: Vec<ApfsVolume>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ApfsPhysicalStore {
    #[serde(rename = "DeviceIdentifier")]
    pub device_identifier: String,
    #[serde(rename = "DiskUUID", default)]
    pub disk_uuid: Option<String>,
    #[serde(rename = "Size", default)]
    pub size: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ApfsVolume {
    #[serde(rename = "APFSVolumeUUID")]
    pub uuid: String,
    #[serde(rename = "DeviceIdentifier")]
    pub device_identifier: String,
    #[serde(rename = "Name", default)]
    pub name: Option<String>,
    #[serde(rename = "MountPoint", default)]
    pub mount_point: Option<String>,
    #[serde(rename = "CapacityInUse", default)]
    pub capacity_in_use: u64,
    #[serde(rename = "CapacityQuota", default)]
    pub capacity_quota: u64,
    #[serde(rename = "CapacityReserve", default)]
    pub capacity_reserve: u64,
    #[serde(rename = "Roles", default)]
    pub roles: Vec<String>,
    #[serde(rename = "Encryption", default)]
    pub encryption: bool,
    #[serde(rename = "FileVault", default)]
    pub file_vault: bool,
    #[serde(rename = "Locked", default)]
    pub locked: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct DiskutilInfoPlist {
    #[serde(rename = "DeviceIdentifier", default)]
    pub device_identifier: Option<String>,
    #[serde(rename = "DeviceNode", default)]
    pub device_node: Option<String>,
    #[serde(rename = "VolumeUUID", default)]
    pub volume_uuid: Option<String>,
    #[serde(rename = "DiskUUID", default)]
    pub disk_uuid: Option<String>,
    #[serde(rename = "VolumeName", default)]
    pub volume_name: Option<String>,
    #[serde(rename = "MountPoint", default)]
    pub mount_point: Option<String>,
    #[serde(rename = "APFSContainerReference", default)]
    pub apfs_container_reference: Option<String>,
    #[serde(rename = "APFSContainerSize", default)]
    pub apfs_container_size: Option<u64>,
    #[serde(rename = "APFSContainerFree", default)]
    pub apfs_container_free: Option<u64>,
    #[serde(rename = "CapacityInUse", default)]
    pub capacity_in_use: Option<u64>,
    #[serde(rename = "Internal", default)]
    pub internal: Option<bool>,
    #[serde(rename = "FilesystemType", default)]
    pub filesystem_type: Option<String>,
    #[serde(rename = "FilesystemUserVisibleName", default)]
    pub filesystem_user_visible_name: Option<String>,
    #[serde(rename = "WritableVolume", default)]
    pub writable_volume: Option<bool>,
}

impl ApfsVolume {
    pub fn has_protected_role(&self) -> Option<&str> {
        self.roles.iter().find_map(|role| {
            PROTECTED_APFS_ROLES
                .iter()
                .find(|&&protected| role.eq_ignore_ascii_case(protected))
                .copied()
        })
    }

    pub fn display_name(&self) -> &str {
        self.name.as_deref().unwrap_or("<unnamed>")
    }
}

impl ApfsListPlist {
    pub fn find_volume_by_uuid(&self, uuid: &str) -> Option<(&ApfsContainer, &ApfsVolume)> {
        for container in &self.containers {
            for vol in &container.volumes {
                if vol.uuid.eq_ignore_ascii_case(uuid) {
                    return Some((container, vol));
                }
            }
        }
        None
    }

    pub fn find_volume_by_name(&self, name: &str) -> Option<(&ApfsContainer, &ApfsVolume)> {
        for container in &self.containers {
            for vol in &container.volumes {
                if vol.name.as_deref() == Some(name) {
                    return Some((container, vol));
                }
            }
        }
        None
    }

    pub fn find_container(&self, reference: &str) -> Option<&ApfsContainer> {
        self.containers
            .iter()
            .find(|c| c.container_reference == reference)
    }

    /// Find the primary macOS internal APFS container (holding both `System` and `Data` roles).
    pub fn detect_primary_internal_container(&self) -> Option<&ApfsContainer> {
        self.containers.iter().find(|c| {
            let has_system = c
                .volumes
                .iter()
                .any(|v| v.roles.iter().any(|r| r.eq_ignore_ascii_case("System")));
            let has_data = c
                .volumes
                .iter()
                .any(|v| v.roles.iter().any(|r| r.eq_ignore_ascii_case("Data")));
            has_system && has_data
        })
    }
}

/// Parse `diskutil apfs list -plist` output structurally using the `plist` crate.
pub fn parse_apfs_list_plist(bytes: &[u8]) -> Result<ApfsListPlist> {
    let parsed: ApfsListPlist = plist::from_bytes(bytes)?;
    Ok(parsed)
}

/// Parse `diskutil info -plist <target>` output structurally using the `plist` crate.
pub fn parse_diskutil_info_plist(bytes: &[u8]) -> Result<DiskutilInfoPlist> {
    let parsed: DiskutilInfoPlist = plist::from_bytes(bytes)?;
    Ok(parsed)
}

/// Execute `diskutil apfs list -plist` (read-only, unprivileged).
pub fn list_apfs() -> Result<ApfsListPlist> {
    let output = Command::new("diskutil")
        .args(["apfs", "list", "-plist"])
        .output()?;
    if !output.status.success() {
        return Err(CaissonError::CommandFailed {
            cmd: "diskutil apfs list -plist".to_string(),
            code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    parse_apfs_list_plist(&output.stdout)
}

/// Execute `diskutil info -plist <target>` (read-only, unprivileged).
pub fn diskutil_info(target: &str) -> Result<DiskutilInfoPlist> {
    let output = Command::new("diskutil")
        .args(["info", "-plist", target])
        .output()?;
    if !output.status.success() {
        return Err(CaissonError::CommandFailed {
            cmd: format!("diskutil info -plist {target}"),
            code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    parse_diskutil_info_plist(&output.stdout)
}

/// Resolve the APFS container reference (e.g. `disk3` or `disk5`) backing an existing mount path
/// (such as `/` or `/Volumes/external-ssd`).
pub fn resolve_container_for_path(path: &Path) -> Result<String> {
    if !path.exists() {
        return Err(CaissonError::Message(format!(
            "path '{}' does not exist",
            path.display()
        )));
    }
    let canon = std::fs::canonicalize(path)?;
    let info = diskutil_info(&canon.display().to_string())?;
    info.apfs_container_reference
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| {
            CaissonError::Message(format!(
                "'{}' is not backed by an APFS container",
                canon.display()
            ))
        })
}

/// Return true if the APFS container (e.g. `disk5`) resides on an external drive.
pub fn is_external_container(container_ref: &str) -> bool {
    diskutil_info(container_ref)
        .ok()
        .and_then(|info| info.internal)
        .map(|internal| !internal)
        .unwrap_or(false)
}

/// Auto-detect the internal APFS container hosting macOS (`/`).
pub fn detect_internal_container() -> Result<ApfsContainer> {
    let apfs = list_apfs()?;
    if let Ok(root_info) = diskutil_info("/") {
        if root_info.internal == Some(true) {
            if let Some(ref container_ref) = root_info.apfs_container_reference {
                if let Some(container) = apfs.find_container(container_ref) {
                    return Ok(container.clone());
                }
            }
        }
    }
    apfs.detect_primary_internal_container()
        .cloned()
        .ok_or_else(|| {
            CaissonError::Message(
                "could not auto-detect internal macOS APFS container (pass `--container diskN`)"
                    .to_string(),
            )
        })
}

/// Validate a Caisson volume name against `[a-z0-9][a-z0-9._-]*` (`ARCHITECTURE.md` §6).
pub fn validate_volume_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(CaissonError::InvalidVolumeName {
            name: name.to_string(),
            reason: "name cannot be empty".to_string(),
        });
    }
    if name.len() > 64 {
        return Err(CaissonError::InvalidVolumeName {
            name: name.to_string(),
            reason: "name must be 64 characters or fewer".to_string(),
        });
    }
    if name.chars().any(|c| c.is_whitespace()) {
        return Err(CaissonError::InvalidVolumeName {
            name: name.to_string(),
            reason: "spaces are rejected to prevent toolchain path failures".to_string(),
        });
    }
    let mut chars = name.chars();
    let first = chars.next().unwrap();
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return Err(CaissonError::InvalidVolumeName {
            name: name.to_string(),
            reason: "must start with a lowercase ASCII letter or digit ([a-z0-9])".to_string(),
        });
    }
    if !chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
    {
        return Err(CaissonError::InvalidVolumeName {
            name: name.to_string(),
            reason: "must match [a-z0-9][a-z0-9._-]* (lowercase letters, digits, '.', '_', '-')"
                .to_string(),
        });
    }
    Ok(())
}

/// Compute the default Layer-A hard APFS ceiling for Tier-1 `caisson init` (`ARCHITECTURE.md` §3):
/// `min(70% of container capacity, container capacity - 100 GiB)`, minimum `50 GiB`,
/// or `None` (disabled) if the container is too small.
pub fn compute_auto_quota(container_capacity_bytes: u64) -> Option<u64> {
    let seventy_pct = ((container_capacity_bytes as u128) * 70 / 100) as u64;
    let minus_100g = container_capacity_bytes.saturating_sub(100 * GIB);
    let candidate = seventy_pct.min(minus_100g);
    let rounded = (candidate / GIB) * GIB;
    if rounded < 50 * GIB {
        None
    } else {
        Some(rounded)
    }
}

/// Build the `diskutil apfs addVolume` argument list cleanly for execution or `--dry-run`.
pub fn build_add_volume_args(
    container: &str,
    name: &str,
    case_sensitive: bool,
    mount_point: Option<&Path>,
    quota_bytes: Option<u64>,
) -> Vec<String> {
    let fs_format = if case_sensitive {
        "Case-sensitive APFS"
    } else {
        "APFS"
    };
    let mut args = vec![
        "apfs".to_string(),
        "addVolume".to_string(),
        container.to_string(),
        fs_format.to_string(),
        name.to_string(),
    ];
    if let Some(mp) = mount_point {
        args.push("-mountpoint".to_string());
        args.push(mp.display().to_string());
    }
    if let Some(quota) = quota_bytes {
        if quota > 0 {
            args.push("-quota".to_string());
            args.push(format_diskutil_size(quota));
        }
    }
    args
}

/// Result of passing all 3 safety checks prior to `diskutil apfs deleteVolume`.
#[derive(Debug, Clone)]
pub struct VerifiedDestroyTarget {
    pub registered: RegisteredVolume,
    pub live_volume: ApfsVolume,
    pub mount_point: PathBuf,
}

/// Enforce the unconditional triple-check `destroy` safety invariant (`ARCHITECTURE.md` §6):
/// 1. Target volume UUID must exist in Caisson's registry.
/// 2. Live APFS volume must exist and MUST NOT carry any system/protected APFS role
///    (`System`, `Data`, `VM`, `Preboot`, `Recovery`, etc.) or mount at `/` or `/System/*`.
/// 3. The volume MUST be currently mounted (never falls back to a stale stored path), and when
///    `verify_live_mount_identity` is true, its mount path MUST be the exact canonical mount root
///    whose live `diskutil info` UUID matches `registered.uuid`. Its on-volume `.caisson-volume.json`
///    marker (read strictly with `O_NOFOLLOW`) MUST have `managed_by == "caisson"` and an exact UUID match.
pub fn verify_destroy_safety(
    target: &str,
    registry: &VolumeRegistry,
    apfs_list: &ApfsListPlist,
    live_mount_point: Option<&Path>,
    verify_live_mount_identity: bool,
) -> Result<VerifiedDestroyTarget> {
    // Check 1: Registry presence
    let registered = registry
        .find_by_name(target)
        .or_else(|| registry.find_by_uuid(target))
        .ok_or_else(|| CaissonError::DestroySafetyViolation {
            volume: target.to_string(),
            reason: "volume is not present in Caisson's registry; refusing to destroy unmanaged APFS volume"
                .to_string(),
        })?
        .clone();

    // Check 2: Live APFS volume & non-system role guard
    let (_, live_volume) = apfs_list
        .find_volume_by_uuid(&registered.uuid)
        .ok_or_else(|| CaissonError::DestroySafetyViolation {
            volume: target.to_string(),
            reason: format!(
                "registered UUID '{}' was not found in live `diskutil apfs list -plist` output",
                registered.uuid
            ),
        })?;

    if let Some(protected_role) = live_volume.has_protected_role() {
        return Err(CaissonError::DestroySafetyViolation {
            volume: target.to_string(),
            reason: format!(
                "APFS volume '{}' has protected system role '{}'; destruction is unconditionally forbidden",
                live_volume.device_identifier, protected_role
            ),
        });
    }

    // Check 3a: Require a live mounted volume root (never fall back to a stale stored directory).
    let mount_point = live_mount_point
        .map(Path::to_path_buf)
        .or_else(|| live_volume.mount_point.as_ref().map(PathBuf::from))
        .ok_or_else(|| CaissonError::DestroySafetyViolation {
            volume: target.to_string(),
            reason: format!(
                "volume '{}' ({}) is not currently mounted; mount it first (`diskutil mount {}`) so Caisson can verify its on-volume ownership marker",
                registered.name, registered.uuid, registered.uuid
            ),
        })?;

    let mp_str = mount_point.to_string_lossy();
    if mp_str == "/" || mp_str.starts_with("/System") {
        return Err(CaissonError::DestroySafetyViolation {
            volume: target.to_string(),
            reason: format!(
                "mount point '{mp_str}' is a protected macOS system mount; destruction is forbidden"
            ),
        });
    }

    if verify_live_mount_identity {
        let canonical_mp = std::fs::canonicalize(&mount_point).map_err(|e| {
            CaissonError::DestroySafetyViolation {
                volume: target.to_string(),
                reason: format!(
                    "failed to canonicalize live mount point '{}': {e}",
                    mount_point.display()
                ),
            }
        })?;
        let st = crate::apfs::statfs::statfs_path(&canonical_mp).map_err(|e| {
            CaissonError::DestroySafetyViolation {
                volume: target.to_string(),
                reason: format!("statfs failed on '{}': {e}", canonical_mp.display()),
            }
        })?;
        let canonical_statfs_mp = std::fs::canonicalize(&st.mount_point)
            .unwrap_or_else(|_| PathBuf::from(&st.mount_point));
        if canonical_mp != canonical_statfs_mp {
            return Err(CaissonError::DestroySafetyViolation {
                volume: target.to_string(),
                reason: format!(
                    "path '{}' is not a mounted volume root (actual mount root is '{}')",
                    canonical_mp.display(),
                    canonical_statfs_mp.display()
                ),
            });
        }
        let mp_info = diskutil_info(&canonical_mp.to_string_lossy()).map_err(|e| {
            CaissonError::DestroySafetyViolation {
                volume: target.to_string(),
                reason: format!(
                    "could not verify live volume identity at '{}': {e}",
                    canonical_mp.display()
                ),
            }
        })?;
        let mounted_uuid = mp_info
            .volume_uuid
            .as_deref()
            .or(mp_info.disk_uuid.as_deref())
            .unwrap_or("");
        if !mounted_uuid.eq_ignore_ascii_case(&registered.uuid) {
            return Err(CaissonError::DestroySafetyViolation {
                volume: target.to_string(),
                reason: format!(
                    "volume mounted at '{}' has UUID '{}', expected registered UUID '{}'",
                    canonical_mp.display(),
                    mounted_uuid,
                    registered.uuid
                ),
            });
        }
    }

    // Check 3b: On-volume `.caisson-volume.json` UUID ownership marker (read strictly with O_NOFOLLOW)
    let marker_path = VolumeMarker::path_for(&mount_point);
    if std::fs::symlink_metadata(&marker_path).is_err() {
        return Err(CaissonError::DestroySafetyViolation {
            volume: target.to_string(),
            reason: format!(
                "missing on-volume ownership marker '{}'; mount the volume or verify it is managed by Caisson",
                marker_path.display()
            ),
        });
    }

    let marker = VolumeMarker::read_from(&mount_point).map_err(|e| {
        CaissonError::DestroySafetyViolation {
            volume: target.to_string(),
            reason: format!(
                "failed to verify ownership marker '{}': {e}",
                marker_path.display()
            ),
        }
    })?;

    if marker.managed_by != "caisson" {
        return Err(CaissonError::DestroySafetyViolation {
            volume: target.to_string(),
            reason: format!(
                "ownership marker managed_by is '{}', expected 'caisson'",
                marker.managed_by
            ),
        });
    }

    if !marker.uuid.eq_ignore_ascii_case(&registered.uuid) {
        return Err(CaissonError::DestroySafetyViolation {
            volume: target.to_string(),
            reason: format!(
                "ownership marker UUID '{}' does not match registered UUID '{}'",
                marker.uuid, registered.uuid
            ),
        });
    }

    Ok(VerifiedDestroyTarget {
        registered,
        live_volume: live_volume.clone(),
        mount_point,
    })
}

/// Execute `diskutil apfs addVolume` (privileged).
pub fn execute_add_volume(args: &[String]) -> Result<String> {
    let output = Command::new("diskutil").args(args).output()?;
    if !output.status.success() {
        return Err(CaissonError::CommandFailed {
            cmd: format!("diskutil {}", args.join(" ")),
            code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Execute `diskutil apfs deleteVolume <uuid>` (privileged).
pub fn execute_delete_volume(uuid: &str) -> Result<String> {
    let output = Command::new("diskutil")
        .args(["apfs", "deleteVolume", uuid])
        .output()?;
    if !output.status.success() {
        return Err(CaissonError::CommandFailed {
            cmd: format!("diskutil apfs deleteVolume {uuid}"),
            code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Return true when `mount_point` differs from macOS's default `/Volumes/<name>` mount path.
pub fn is_custom_mount_point(name: &str, mount_point: &Path) -> bool {
    let default_mp = PathBuf::from(format!("/Volumes/{name}"));
    mount_point != default_mp.as_path()
}

/// Validate that a custom mount point directory is safe to mount over (absolute, non-system,
/// non-symlink, and empty if it already exists). Optionally creates the directory if missing.
pub fn validate_safe_custom_mount_dir(mount_point: &Path, create_if_missing: bool) -> Result<()> {
    if !mount_point.is_absolute() {
        return Err(CaissonError::Message(format!(
            "custom mount point '{}' must be an absolute path",
            mount_point.display()
        )));
    }
    let mp_str = mount_point.to_string_lossy();
    let trimmed = mp_str.trim_end_matches('/');
    if trimmed.is_empty()
        || trimmed == "/System"
        || trimmed.starts_with("/System/")
        || matches!(
            trimmed,
            "/Volumes" | "/Applications" | "/Library" | "/Users" | "/bin" | "/sbin" | "/usr"
        )
    {
        return Err(CaissonError::Message(format!(
            "refusing to use protected system directory '{}' as a custom mount point",
            mount_point.display()
        )));
    }

    match std::fs::symlink_metadata(mount_point) {
        Ok(meta) => {
            if meta.file_type().is_symlink() || !meta.is_dir() {
                return Err(CaissonError::Message(format!(
                    "refusing to mount over symlink or non-directory '{}'",
                    mount_point.display()
                )));
            }
            if std::fs::read_dir(mount_point)?.next().is_some() {
                return Err(CaissonError::Message(format!(
                    "refusing to mount over non-empty directory '{}'; move or back up existing contents first so they are not hidden beneath the mounted volume",
                    mount_point.display()
                )));
            }
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if create_if_missing {
                std::fs::create_dir_all(mount_point)?;
            }
            Ok(())
        }
        Err(e) => Err(CaissonError::Io(e)),
    }
}

/// Mount an APFS volume by UUID or device identifier, optionally at a custom `-mountPoint`.
pub fn execute_mount_volume(target: &str, custom_mount_point: Option<&Path>) -> Result<String> {
    let mut args: Vec<String> = vec!["mount".to_string()];
    if let Some(mp) = custom_mount_point {
        args.push("-mountPoint".to_string());
        args.push(mp.display().to_string());
    }
    args.push(target.to_string());

    let output = Command::new("diskutil").args(&args).output()?;
    if !output.status.success() {
        return Err(CaissonError::CommandFailed {
            cmd: format!("diskutil {}", args.join(" ")),
            code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Unmount an APFS volume by UUID or device identifier without forcing busy handles.
pub fn execute_unmount_volume(target: &str) -> Result<String> {
    let output = Command::new("diskutil")
        .args(["unmount", target])
        .output()?;
    if !output.status.success() {
        return Err(CaissonError::CommandFailed {
            cmd: format!("diskutil unmount {target}"),
            code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::VolumeTier;
    use chrono::Utc;
    use std::fs;
    use tempfile::tempdir;

    const SAMPLE_APFS_LIST_PLIST: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Containers</key>
  <array>
    <dict>
      <key>APFSContainerUUID</key>
      <string>AAAAAAAA-AAAA-AAAA-AAAA-AAAAAAAAAAAA</string>
      <key>CapacityCeiling</key>
      <integer>1099511627776</integer>
      <key>CapacityFree</key>
      <integer>429496729600</integer>
      <key>ContainerReference</key>
      <string>disk3</string>
      <key>DesignatedPhysicalStore</key>
      <string>disk0s2</string>
      <key>PhysicalStores</key>
      <array>
        <dict>
          <key>DeviceIdentifier</key>
          <string>disk0s2</string>
          <key>DiskUUID</key>
          <string>BBBBBBBB-BBBB-BBBB-BBBB-BBBBBBBBBBBB</string>
          <key>Size</key>
          <integer>1099511627776</integer>
        </dict>
      </array>
      <key>Volumes</key>
      <array>
        <dict>
          <key>APFSVolumeUUID</key>
          <string>11111111-1111-1111-1111-111111111111</string>
          <key>CapacityInUse</key>
          <integer>13653807104</integer>
          <key>CapacityQuota</key>
          <integer>0</integer>
          <key>CapacityReserve</key>
          <integer>0</integer>
          <key>DeviceIdentifier</key>
          <string>disk3s1</string>
          <key>Encryption</key>
          <true/>
          <key>FileVault</key>
          <true/>
          <key>Locked</key>
          <false/>
          <key>Name</key>
          <string>Macintosh HD</string>
          <key>Roles</key>
          <array>
            <string>System</string>
          </array>
        </dict>
        <dict>
          <key>APFSVolumeUUID</key>
          <string>22222222-2222-2222-2222-222222222222</string>
          <key>CapacityInUse</key>
          <integer>154357633024</integer>
          <key>CapacityQuota</key>
          <integer>0</integer>
          <key>CapacityReserve</key>
          <integer>0</integer>
          <key>DeviceIdentifier</key>
          <string>disk3s5</string>
          <key>Encryption</key>
          <true/>
          <key>FileVault</key>
          <true/>
          <key>Locked</key>
          <false/>
          <key>Name</key>
          <string>Data</string>
          <key>Roles</key>
          <array>
            <string>Data</string>
          </array>
        </dict>
        <dict>
          <key>APFSVolumeUUID</key>
          <string>33333333-3333-3333-3333-333333333333</string>
          <key>CapacityInUse</key>
          <integer>1073741824</integer>
          <key>CapacityQuota</key>
          <integer>268435456000</integer>
          <key>CapacityReserve</key>
          <integer>0</integer>
          <key>DeviceIdentifier</key>
          <string>disk3s7</string>
          <key>Encryption</key>
          <false/>
          <key>FileVault</key>
          <false/>
          <key>Locked</key>
          <false/>
          <key>Name</key>
          <string>caisson</string>
          <key>Roles</key>
          <array/>
        </dict>
      </array>
    </dict>
  </array>
</dict>
</plist>"#;

    const SAMPLE_DISKUTIL_INFO_PLIST: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>APFSContainerReference</key>
  <string>disk3</string>
  <key>APFSContainerSize</key>
  <integer>1099511627776</integer>
  <key>APFSContainerFree</key>
  <integer>429496729600</integer>
  <key>CapacityInUse</key>
  <integer>1073741824</integer>
  <key>DeviceIdentifier</key>
  <string>disk3s7</string>
  <key>DeviceNode</key>
  <string>/dev/disk3s7</string>
  <key>FilesystemType</key>
  <string>apfs</string>
  <key>FilesystemUserVisibleName</key>
  <string>Case-sensitive APFS</string>
  <key>Internal</key>
  <true/>
  <key>MountPoint</key>
  <string>/Volumes/caisson</string>
  <key>VolumeName</key>
  <string>caisson</string>
  <key>VolumeUUID</key>
  <string>33333333-3333-3333-3333-333333333333</string>
  <key>WritableVolume</key>
  <true/>
</dict>
</plist>"#;

    fn sample_registered(uuid: &str, name: &str, mount_point: &Path) -> RegisteredVolume {
        RegisteredVolume {
            name: name.to_string(),
            uuid: uuid.to_string(),
            container: "disk3".to_string(),
            mount_point: mount_point.display().to_string(),
            tier: VolumeTier::Tier1,
            case_sensitive: false,
            nobrowse: false,
            is_external: false,
            quota_bytes: Some(250 * GIB),
            owner: Some("dev".to_string()),
            created_at: Utc::now(),
        }
    }

    #[test]
    fn parses_apfs_list_and_info_plists_structurally() {
        let apfs = parse_apfs_list_plist(SAMPLE_APFS_LIST_PLIST.as_bytes()).unwrap();
        assert_eq!(apfs.containers.len(), 1);
        let primary = apfs.detect_primary_internal_container().unwrap();
        assert_eq!(primary.container_reference, "disk3");
        assert_eq!(primary.capacity_ceiling, 1024 * GIB);
        assert_eq!(primary.volumes.len(), 3);

        let (_, caisson_vol) = apfs
            .find_volume_by_uuid("33333333-3333-3333-3333-333333333333")
            .unwrap();
        assert_eq!(caisson_vol.display_name(), "caisson");
        assert_eq!(caisson_vol.capacity_quota, 250 * GIB);
        assert!(caisson_vol.has_protected_role().is_none());

        let (_, sys_vol) = apfs.find_volume_by_name("Macintosh HD").unwrap();
        assert_eq!(sys_vol.has_protected_role(), Some("System"));

        let info = parse_diskutil_info_plist(SAMPLE_DISKUTIL_INFO_PLIST.as_bytes()).unwrap();
        assert_eq!(info.apfs_container_reference.as_deref(), Some("disk3"));
        assert_eq!(info.mount_point.as_deref(), Some("/Volumes/caisson"));
        assert_eq!(info.internal, Some(true));
        assert_eq!(
            info.filesystem_user_visible_name.as_deref(),
            Some("Case-sensitive APFS")
        );
    }

    #[test]
    fn validates_volume_names_strictly() {
        assert!(validate_volume_name("caisson").is_ok());
        assert!(validate_volume_name("chromium-bench-158").is_ok());
        assert!(validate_volume_name("build_v2.1").is_ok());
        assert!(validate_volume_name("0dev").is_ok());

        assert!(validate_volume_name("").is_err());
        assert!(validate_volume_name("my volume").is_err());
        assert!(validate_volume_name("Caisson").is_err());
        assert!(validate_volume_name("-leading-dash").is_err());
        assert!(validate_volume_name(".hidden").is_err());
        assert!(validate_volume_name("bad/slash").is_err());
        assert!(validate_volume_name(&"a".repeat(65)).is_err());
    }

    #[test]
    fn computes_auto_quota_per_architecture_spec() {
        // 1 TiB: min(70% = 716 GiB, 1024 - 100 = 924 GiB) -> 716 GiB
        assert_eq!(compute_auto_quota(1024 * GIB), Some(716 * GIB));
        // 256 GiB: min(70% = 179 GiB, 256 - 100 = 156 GiB) -> 156 GiB
        assert_eq!(compute_auto_quota(256 * GIB), Some(156 * GIB));
        // 150 GiB: min(105 GiB, 50 GiB) -> 50 GiB (exact minimum)
        assert_eq!(compute_auto_quota(150 * GIB), Some(50 * GIB));
        // 140 GiB: min(98 GiB, 40 GiB) -> 40 GiB < 50 GiB -> None (disabled)
        assert_eq!(compute_auto_quota(140 * GIB), None);
        // 80 GiB: too small -> None
        assert_eq!(compute_auto_quota(80 * GIB), None);
    }

    #[test]
    fn builds_diskutil_add_volume_args() {
        let args = build_add_volume_args(
            "disk3",
            "chromium-bench",
            true,
            Some(Path::new("/Volumes/chromium-bench")),
            Some(300 * GIB),
        );
        assert_eq!(
            args,
            vec![
                "apfs",
                "addVolume",
                "disk3",
                "Case-sensitive APFS",
                "chromium-bench",
                "-mountpoint",
                "/Volumes/chromium-bench",
                "-quota",
                "322122547200B",
            ]
        );
    }

    #[test]
    fn destroy_triple_check_passes_only_when_all_three_checks_match() {
        let apfs = parse_apfs_list_plist(SAMPLE_APFS_LIST_PLIST.as_bytes()).unwrap();
        let mount_dir = tempdir().unwrap();
        let vol = sample_registered(
            "33333333-3333-3333-3333-333333333333",
            "caisson",
            mount_dir.path(),
        );
        let mut reg = VolumeRegistry::default();
        reg.upsert(vol.clone());
        VolumeMarker::from_registered(&vol)
            .write_to(mount_dir.path())
            .unwrap();

        let verified = verify_destroy_safety("caisson", &reg, &apfs, Some(mount_dir.path()), false)
            .expect("all three safety checks should pass");
        assert_eq!(verified.registered.uuid, vol.uuid);
        assert_eq!(verified.live_volume.device_identifier, "disk3s7");

        // Unmounted volume MUST be refused even if stored mount_point still has a marker file.
        let unmounted = verify_destroy_safety("caisson", &reg, &apfs, None, false).unwrap_err();
        assert!(matches!(
            unmounted,
            CaissonError::DestroySafetyViolation { .. }
        ));
        assert!(unmounted.to_string().contains("not currently mounted"));

        // A non-mount-root directory MUST be refused when live mount verification is enabled.
        let non_root = verify_destroy_safety("caisson", &reg, &apfs, Some(mount_dir.path()), true)
            .unwrap_err();
        assert!(matches!(
            non_root,
            CaissonError::DestroySafetyViolation { .. }
        ));
        assert!(non_root.to_string().contains("not a mounted volume root"));
    }

    #[test]
    fn destroy_refuses_unregistered_volume() {
        let apfs = parse_apfs_list_plist(SAMPLE_APFS_LIST_PLIST.as_bytes()).unwrap();
        let mount_dir = tempdir().unwrap();
        let reg = VolumeRegistry::default();

        let err = verify_destroy_safety("caisson", &reg, &apfs, Some(mount_dir.path()), false)
            .unwrap_err();
        assert!(matches!(err, CaissonError::DestroySafetyViolation { .. }));
        assert!(err
            .to_string()
            .contains("not present in Caisson's registry"));
    }

    #[test]
    fn destroy_refuses_missing_marker_file() {
        let apfs = parse_apfs_list_plist(SAMPLE_APFS_LIST_PLIST.as_bytes()).unwrap();
        let mount_dir = tempdir().unwrap();
        let vol = sample_registered(
            "33333333-3333-3333-3333-333333333333",
            "caisson",
            mount_dir.path(),
        );
        let mut reg = VolumeRegistry::default();
        reg.upsert(vol);

        let err = verify_destroy_safety("caisson", &reg, &apfs, Some(mount_dir.path()), false)
            .unwrap_err();
        assert!(matches!(err, CaissonError::DestroySafetyViolation { .. }));
        assert!(err
            .to_string()
            .contains("missing on-volume ownership marker"));
    }

    #[test]
    fn destroy_refuses_mismatched_marker_uuid() {
        let apfs = parse_apfs_list_plist(SAMPLE_APFS_LIST_PLIST.as_bytes()).unwrap();
        let mount_dir = tempdir().unwrap();
        let vol = sample_registered(
            "33333333-3333-3333-3333-333333333333",
            "caisson",
            mount_dir.path(),
        );
        let mut reg = VolumeRegistry::default();
        reg.upsert(vol.clone());

        let mut wrong_marker = VolumeMarker::from_registered(&vol);
        wrong_marker.uuid = "99999999-9999-9999-9999-999999999999".to_string();
        wrong_marker.write_to(mount_dir.path()).unwrap();

        let err = verify_destroy_safety("caisson", &reg, &apfs, Some(mount_dir.path()), false)
            .unwrap_err();
        assert!(matches!(err, CaissonError::DestroySafetyViolation { .. }));
        assert!(err.to_string().contains("does not match registered UUID"));
    }

    #[test]
    fn destroy_refuses_corrupt_or_foreign_marker() {
        let apfs = parse_apfs_list_plist(SAMPLE_APFS_LIST_PLIST.as_bytes()).unwrap();
        let mount_dir = tempdir().unwrap();
        let vol = sample_registered(
            "33333333-3333-3333-3333-333333333333",
            "caisson",
            mount_dir.path(),
        );
        let mut reg = VolumeRegistry::default();
        reg.upsert(vol.clone());

        // Corrupt JSON
        fs::write(VolumeMarker::path_for(mount_dir.path()), "not-json").unwrap();
        let err = verify_destroy_safety("caisson", &reg, &apfs, Some(mount_dir.path()), false)
            .unwrap_err();
        assert!(matches!(err, CaissonError::DestroySafetyViolation { .. }));

        // Wrong managed_by
        let mut foreign = VolumeMarker::from_registered(&vol);
        foreign.managed_by = "other-tool".to_string();
        foreign.write_to(mount_dir.path()).unwrap();
        let err = verify_destroy_safety("caisson", &reg, &apfs, Some(mount_dir.path()), false)
            .unwrap_err();
        assert!(matches!(err, CaissonError::DestroySafetyViolation { .. }));
        assert!(err.to_string().contains("managed_by"));
    }

    #[test]
    fn destroy_refuses_system_and_data_roles_even_if_registered_with_marker() {
        let apfs = parse_apfs_list_plist(SAMPLE_APFS_LIST_PLIST.as_bytes()).unwrap();
        let mount_dir = tempdir().unwrap();

        for (uuid, name, expected_role) in [
            (
                "11111111-1111-1111-1111-111111111111",
                "Macintosh HD",
                "System",
            ),
            ("22222222-2222-2222-2222-222222222222", "Data", "Data"),
        ] {
            let vol = sample_registered(uuid, name, mount_dir.path());
            let mut reg = VolumeRegistry::default();
            reg.upsert(vol.clone());
            VolumeMarker::from_registered(&vol)
                .write_to(mount_dir.path())
                .unwrap();

            let err = verify_destroy_safety(name, &reg, &apfs, Some(mount_dir.path()), false)
                .unwrap_err();
            assert!(matches!(err, CaissonError::DestroySafetyViolation { .. }));
            assert!(
                err.to_string().contains(expected_role),
                "expected role {expected_role} in error: {err}"
            );
        }
    }

    #[test]
    fn destroy_refuses_all_protected_roles() {
        let mount_dir = tempdir().unwrap();
        for role in ["System", "Data", "VM", "Preboot", "Recovery"] {
            let plist_xml = format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict><key>Containers</key><array><dict>
<key>ContainerReference</key><string>disk3</string>
<key>Volumes</key><array><dict>
<key>APFSVolumeUUID</key><string>44444444-4444-4444-4444-444444444444</string>
<key>DeviceIdentifier</key><string>disk3s9</string>
<key>Name</key><string>protected-vol</string>
<key>Roles</key><array><string>{role}</string></array>
</dict></array></dict></array></dict></plist>"#
            );
            let apfs = parse_apfs_list_plist(plist_xml.as_bytes()).unwrap();
            let vol = sample_registered(
                "44444444-4444-4444-4444-444444444444",
                "protected-vol",
                mount_dir.path(),
            );
            let mut reg = VolumeRegistry::default();
            reg.upsert(vol.clone());
            VolumeMarker::from_registered(&vol)
                .write_to(mount_dir.path())
                .unwrap();

            let err =
                verify_destroy_safety("protected-vol", &reg, &apfs, Some(mount_dir.path()), false)
                    .unwrap_err();
            assert!(matches!(err, CaissonError::DestroySafetyViolation { .. }));
            assert!(err.to_string().contains(role));
        }
    }

    #[test]
    fn live_diskutil_apfs_list_parses_cleanly() {
        // Read-only unprivileged smoke check on real macOS host.
        let apfs = list_apfs().unwrap();
        assert!(!apfs.containers.is_empty());
        let primary = detect_internal_container().unwrap();
        assert!(primary.capacity_ceiling > 0);
        let root_container = resolve_container_for_path(Path::new("/")).unwrap();
        assert_eq!(root_container, primary.container_reference);
    }

    #[test]
    fn custom_mount_point_detection_and_validation() {
        use std::os::unix::fs::symlink;

        assert!(!is_custom_mount_point(
            "caisson",
            Path::new("/Volumes/caisson")
        ));
        assert!(is_custom_mount_point(
            "t3-worktrees",
            Path::new("/var/tmp/t3-worktrees")
        ));

        // Reject relative and protected system mount points.
        assert!(validate_safe_custom_mount_dir(Path::new("relative/dir"), false).is_err());
        assert!(validate_safe_custom_mount_dir(Path::new("/"), false).is_err());
        assert!(validate_safe_custom_mount_dir(Path::new("/System/Library"), false).is_err());
        assert!(validate_safe_custom_mount_dir(Path::new("/Volumes"), false).is_err());

        let dir = tempdir().unwrap();
        let empty = dir.path().join("empty-mp");
        // Missing dir succeeds when create_if_missing=false (dry-run) and creates when true.
        assert!(validate_safe_custom_mount_dir(&empty, false).is_ok());
        assert!(!empty.exists());
        assert!(validate_safe_custom_mount_dir(&empty, true).is_ok());
        assert!(empty.is_dir());

        // Non-empty dir must be refused so existing files are never hidden beneath a mount.
        fs::write(empty.join("existing.txt"), "keep").unwrap();
        assert!(validate_safe_custom_mount_dir(&empty, false).is_err());

        // Symlink mount point must be refused.
        let link = dir.path().join("symlink-mp");
        let target = dir.path().join("real-target");
        fs::create_dir(&target).unwrap();
        symlink(&target, &link).unwrap();
        assert!(validate_safe_custom_mount_dir(&link, false).is_err());
    }
}
