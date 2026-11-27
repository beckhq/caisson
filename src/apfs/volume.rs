use crate::apfs::diskutil::{
    build_add_volume_args, compute_auto_quota, detect_internal_container, diskutil_info,
    execute_add_volume, execute_delete_volume, list_apfs, validate_volume_name,
    verify_destroy_safety, ApfsContainer,
};
use crate::apfs::shield::{
    apply_shields, inspect_shields, ShieldApplyReport, ShieldOptions, ShieldStatus,
};
use crate::apfs::statfs::{invoking_user, is_root, statfs_path};
use crate::config::{CaissonConfig, DEFAULT_TIER1_VOLUME_NAME};
use crate::daemon::launchd::{daemon_status, DaemonStatus, LAUNCH_DAEMON_PLIST_PATH};
use crate::error::{CaissonError, Result};
use crate::registry::{RegisteredVolume, VolumeMarker, VolumeRegistry, VolumeTier};
use crate::units::parse_size;
use chrono::Utc;
use serde::Serialize;
use std::collections::HashSet;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone)]
pub enum QuotaSpec {
    Auto,
    None,
    Bytes(u64),
}

impl QuotaSpec {
    pub fn parse(raw: &str) -> Result<Self> {
        let lower = raw.trim().to_ascii_lowercase();
        match lower.as_str() {
            "auto" => Ok(Self::Auto),
            "none" | "off" | "0" => Ok(Self::None),
            _ => Ok(Self::Bytes(parse_size(raw)?)),
        }
    }
}

#[derive(Debug, Clone)]
pub struct CreateVolumeOptions {
    pub name: String,
    pub tier: VolumeTier,
    pub container: Option<String>,
    pub mount_point: Option<PathBuf>,
    pub quota: QuotaSpec,
    pub case_sensitive: bool,
    pub nobrowse: bool,
    pub dry_run: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct VolumeCreateReport {
    pub name: String,
    pub tier: VolumeTier,
    pub container: String,
    pub container_capacity_bytes: u64,
    pub mount_point: String,
    pub uuid: Option<String>,
    pub quota_bytes: Option<u64>,
    pub case_sensitive: bool,
    pub nobrowse: bool,
    pub owner: Option<String>,
    pub dry_run: bool,
    pub diskutil_command: String,
    pub shield_report: ShieldApplyReport,
    pub initialized_dirs: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct VolumeStatusEntry {
    pub name: String,
    pub uuid: String,
    pub tier: VolumeTier,
    pub container: String,
    pub device_identifier: Option<String>,
    pub mount_point: String,
    pub mounted: bool,
    pub used_bytes: u64,
    pub quota_bytes: Option<u64>,
    pub available_bytes: Option<u64>,
    pub case_sensitive: bool,
    pub nobrowse: bool,
    pub shields: ShieldStatus,
}

#[derive(Debug, Clone, Serialize)]
pub struct VolumeDestroyReport {
    pub name: String,
    pub uuid: String,
    pub device_identifier: String,
    pub mount_point: String,
    pub dry_run: bool,
    pub safety_verified: bool,
    pub command: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct StatusReport {
    pub internal_container: Option<ContainerSummary>,
    pub tier1_root: String,
    pub tier1_root_exists: bool,
    pub volumes: Vec<VolumeStatusEntry>,
    pub daemon: DaemonStatus,
    pub config: CaissonConfig,
}

#[derive(Debug, Clone, Serialize)]
pub struct ContainerSummary {
    pub container_reference: String,
    pub uuid: String,
    pub capacity_ceiling: u64,
    pub capacity_free: u64,
    pub volume_count: usize,
}

impl From<&ApfsContainer> for ContainerSummary {
    fn from(c: &ApfsContainer) -> Self {
        Self {
            container_reference: c.container_reference.clone(),
            uuid: c.uuid.clone(),
            capacity_ceiling: c.capacity_ceiling,
            capacity_free: c.capacity_free,
            volume_count: c.volumes.len(),
        }
    }
}

/// Create and shield either the Tier-1 (`caisson init`) or Tier-2 (`caisson vol create`) APFS volume.
pub fn create_volume(
    opts: &CreateVolumeOptions,
    registry_path: &Path,
) -> Result<VolumeCreateReport> {
    validate_volume_name(&opts.name)?;

    let apfs_before = list_apfs()?;
    let container = match opts.container {
        Some(ref c_ref) => apfs_before
            .find_container(c_ref)
            .cloned()
            .ok_or_else(|| CaissonError::Message(format!("APFS container '{c_ref}' not found")))?,
        None => detect_internal_container()?,
    };

    if apfs_before.find_volume_by_name(&opts.name).is_some() {
        return Err(CaissonError::Message(format!(
            "APFS volume '{}' already exists; run `sudo caisson vol shield {}` to re-apply shields or choose another name",
            opts.name, opts.name
        )));
    }

    let before_uuids: HashSet<String> = container
        .volumes
        .iter()
        .map(|v| v.uuid.to_ascii_uppercase())
        .collect();

    let quota_bytes = match opts.quota {
        QuotaSpec::Auto => compute_auto_quota(container.capacity_ceiling),
        QuotaSpec::None => None,
        QuotaSpec::Bytes(b) if b > 0 => Some(b),
        QuotaSpec::Bytes(_) => None,
    };

    let mount_point = opts
        .mount_point
        .clone()
        .unwrap_or_else(|| PathBuf::from(format!("/Volumes/{}", opts.name)));

    let args = build_add_volume_args(
        &container.container_reference,
        &opts.name,
        opts.case_sensitive,
        opts.mount_point.as_deref(),
        quota_bytes,
    );
    let diskutil_command = format!("diskutil {}", args.join(" "));
    let owner = invoking_user();

    let tier1_dirs = if opts.tier == VolumeTier::Tier1 {
        vec![
            mount_point.join("worktrees").display().to_string(),
            mount_point.join("cache").display().to_string(),
            mount_point.join(".trash").display().to_string(),
        ]
    } else {
        Vec::new()
    };

    if opts.dry_run {
        let shield_report = apply_shields(
            &mount_point,
            &ShieldOptions {
                nobrowse: opts.nobrowse,
                owner: owner.clone(),
                dry_run: true,
            },
        )?;
        return Ok(VolumeCreateReport {
            name: opts.name.clone(),
            tier: opts.tier,
            container: container.container_reference,
            container_capacity_bytes: container.capacity_ceiling,
            mount_point: mount_point.display().to_string(),
            uuid: None,
            quota_bytes,
            case_sensitive: opts.case_sensitive,
            nobrowse: opts.nobrowse,
            owner,
            dry_run: true,
            diskutil_command,
            shield_report,
            initialized_dirs: tier1_dirs,
        });
    }

    if !is_root() {
        return Err(CaissonError::RequiresRoot {
            operation: if opts.tier == VolumeTier::Tier1 {
                "init".to_string()
            } else {
                "vol create".to_string()
            },
        });
    }

    execute_add_volume(&args)?;

    // Identify the newly created volume unambiguously via container-scoped before/after UUID diff.
    let updated_apfs = list_apfs()?;
    let updated_container = updated_apfs
        .find_container(&container.container_reference)
        .ok_or_else(|| {
            CaissonError::Message(format!(
                "APFS container '{}' disappeared after addVolume",
                container.container_reference
            ))
        })?;

    let new_matches: Vec<_> = updated_container
        .volumes
        .iter()
        .filter(|v| {
            !before_uuids.contains(&v.uuid.to_ascii_uppercase())
                && v.name.as_deref() == Some(opts.name.as_str())
        })
        .collect();

    let created_vol = match new_matches.as_slice() {
        [only] => *only,
        [] => {
            return Err(CaissonError::Message(format!(
                "newly created volume '{}' was not found in container '{}'",
                opts.name, container.container_reference
            )));
        }
        many => {
            return Err(CaissonError::Message(format!(
                "ambiguous creation result: {} new volumes named '{}' appeared in container '{}'",
                many.len(),
                opts.name,
                container.container_reference
            )));
        }
    };

    // Require live mount confirmation before writing the on-volume ownership marker.
    let live_info = diskutil_info(&created_vol.uuid)?;
    let actual_mount_str = live_info
        .mount_point
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| {
            CaissonError::Message(format!(
                "created volume '{}' ({}) did not report a live mount point; refusing to write marker to an unverified path",
                opts.name, created_vol.uuid
            ))
        })?;
    let actual_mount = fs::canonicalize(&actual_mount_str)?;
    verify_mount_root_matches_uuid(&actual_mount, &created_vol.uuid)?;

    let effective_quota = if created_vol.capacity_quota > 0 {
        Some(created_vol.capacity_quota)
    } else {
        quota_bytes
    };

    let registered = RegisteredVolume {
        name: opts.name.clone(),
        uuid: created_vol.uuid.clone(),
        container: container.container_reference.clone(),
        mount_point: actual_mount.display().to_string(),
        tier: opts.tier,
        case_sensitive: opts.case_sensitive,
        nobrowse: opts.nobrowse,
        quota_bytes: effective_quota,
        owner: owner.clone(),
        created_at: Utc::now(),
    };

    // Write on-volume ownership marker `.caisson-volume.json` and register under advisory lock
    // immediately so even if a subsequent shield step warns or fails, the volume is tracked.
    VolumeMarker::from_registered(&registered).write_to(&actual_mount)?;
    VolumeRegistry::with_locked_update(registry_path, |reg| {
        reg.upsert(registered);
        Ok(())
    })?;

    // Initialize Tier-1 directories (`worktrees/`, `cache/`, `.trash/`)
    let mut created_dirs = Vec::new();
    if opts.tier == VolumeTier::Tier1 {
        for sub in ["worktrees", "cache", ".trash"] {
            let dir = actual_mount.join(sub);
            fs::create_dir_all(&dir)?;
            if let Some(ref user) = owner {
                let _ = Command::new("chown")
                    .args([&format!("{user}:staff"), &dir.display().to_string()])
                    .output();
            }
            created_dirs.push(dir.display().to_string());
        }
    }

    let shield_report = apply_shields(
        &actual_mount,
        &ShieldOptions {
            nobrowse: opts.nobrowse,
            owner: owner.clone(),
            dry_run: false,
        },
    )?;

    Ok(VolumeCreateReport {
        name: opts.name.clone(),
        tier: opts.tier,
        container: container.container_reference,
        container_capacity_bytes: container.capacity_ceiling,
        mount_point: actual_mount.display().to_string(),
        uuid: Some(created_vol.uuid.clone()),
        quota_bytes: effective_quota,
        case_sensitive: opts.case_sensitive,
        nobrowse: opts.nobrowse,
        owner,
        dry_run: false,
        diskutil_command,
        shield_report,
        initialized_dirs: created_dirs,
    })
}

/// Verify that `mount_point` is an exact mounted APFS volume root matching `expected_uuid`.
pub fn verify_mount_root_matches_uuid(mount_point: &Path, expected_uuid: &str) -> Result<()> {
    let canonical_mp = fs::canonicalize(mount_point)?;
    let mp_str = canonical_mp.to_string_lossy();
    if mp_str == "/" || mp_str.starts_with("/System") {
        return Err(CaissonError::Message(format!(
            "refusing to operate on protected macOS system mount '{mp_str}'"
        )));
    }
    let st = statfs_path(&canonical_mp)?;
    let canonical_st =
        fs::canonicalize(&st.mount_point).unwrap_or_else(|_| PathBuf::from(&st.mount_point));
    if canonical_mp != canonical_st {
        return Err(CaissonError::Message(format!(
            "path '{}' is not a mounted volume root (mount root is '{}')",
            canonical_mp.display(),
            canonical_st.display()
        )));
    }
    let info = diskutil_info(&mp_str)?;
    let mounted_uuid = info
        .volume_uuid
        .as_deref()
        .or(info.disk_uuid.as_deref())
        .unwrap_or("");
    if !mounted_uuid.eq_ignore_ascii_case(expected_uuid) {
        return Err(CaissonError::Message(format!(
            "mounted volume at '{}' has UUID '{}', expected '{}'",
            canonical_mp.display(),
            mounted_uuid,
            expected_uuid
        )));
    }
    Ok(())
}

/// List all Caisson-registered volumes enriched with live APFS & shield inspection data.
pub fn list_registered_volumes(registry_path: &Path) -> Result<Vec<VolumeStatusEntry>> {
    let registry = VolumeRegistry::load_from(registry_path)?;
    let apfs = list_apfs().ok();
    let mut entries = Vec::with_capacity(registry.volumes.len());

    for vol in &registry.volumes {
        let live_vol = apfs
            .as_ref()
            .and_then(|a| a.find_volume_by_uuid(&vol.uuid))
            .map(|(_, v)| v);

        let live_mount = diskutil_info(&vol.uuid)
            .ok()
            .and_then(|info| info.mount_point)
            .filter(|s| !s.is_empty());

        let mount_point = live_mount
            .clone()
            .unwrap_or_else(|| vol.mount_point.clone());
        let mp_path = Path::new(&mount_point);
        let mounted = live_mount.is_some() && mp_path.exists();

        let stat = if mounted {
            statfs_path(mp_path).ok()
        } else {
            None
        };

        let used_bytes = live_vol
            .map(|v| v.capacity_in_use)
            .or_else(|| stat.as_ref().map(|s| s.used_bytes))
            .unwrap_or(0);
        let quota_bytes = live_vol
            .and_then(|v| (v.capacity_quota > 0).then_some(v.capacity_quota))
            .or(vol.quota_bytes);
        let available_bytes = stat.as_ref().map(|s| s.available_bytes);
        let shields = inspect_shields(mp_path);

        entries.push(VolumeStatusEntry {
            name: vol.name.clone(),
            uuid: vol.uuid.clone(),
            tier: vol.tier,
            container: vol.container.clone(),
            device_identifier: live_vol.map(|v| v.device_identifier.clone()),
            mount_point,
            mounted,
            used_bytes,
            quota_bytes,
            available_bytes,
            case_sensitive: vol.case_sensitive,
            nobrowse: vol.nobrowse,
            shields,
        });
    }

    Ok(entries)
}

/// Idempotently re-apply shields to a Caisson volume by name, UUID, or verified APFS mount path.
///
/// Always validates live APFS mount identity and unconditionally rejects protected system roles
/// (`System`, `Data`, `VM`, `Preboot`, `Recovery`, etc.) and `/` or `/System/*`.
pub fn shield_volume(
    target: &str,
    registry_path: &Path,
    nobrowse_override: Option<bool>,
    dry_run: bool,
) -> Result<ShieldApplyReport> {
    if !dry_run && !is_root() {
        return Err(CaissonError::RequiresRoot {
            operation: "vol shield".to_string(),
        });
    }

    let registry = VolumeRegistry::load_from(registry_path)?;
    let apfs = list_apfs()?;

    let (mount_point, nobrowse, owner) = if let Some(vol) = registry
        .find_by_name(target)
        .or_else(|| registry.find_by_uuid(target))
    {
        let (_, live_vol) = apfs.find_volume_by_uuid(&vol.uuid).ok_or_else(|| {
            CaissonError::Message(format!(
                "registered volume '{}' ({}) not found in live APFS list",
                vol.name, vol.uuid
            ))
        })?;
        if let Some(role) = live_vol.has_protected_role() {
            return Err(CaissonError::Message(format!(
                "refusing to shield volume '{}' with protected APFS role '{role}'",
                vol.name
            )));
        }
        let live_mount = diskutil_info(&vol.uuid)?
            .mount_point
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| {
                CaissonError::Message(format!(
                    "volume '{}' ({}) is not currently mounted; mount it before shielding",
                    vol.name, vol.uuid
                ))
            })?;
        let mp = fs::canonicalize(&live_mount)?;
        verify_mount_root_matches_uuid(&mp, &vol.uuid)?;

        // Verify on-volume ownership marker matches registered UUID.
        let marker = VolumeMarker::read_from(&mp)?;
        if marker.managed_by != "caisson" || !marker.uuid.eq_ignore_ascii_case(&vol.uuid) {
            return Err(CaissonError::Message(format!(
                "on-volume marker at '{}' does not match registered UUID '{}'",
                mp.display(),
                vol.uuid
            )));
        }

        (
            mp,
            nobrowse_override.unwrap_or(vol.nobrowse),
            vol.owner.clone().or_else(invoking_user),
        )
    } else if Path::new(target).is_absolute() {
        let mp = fs::canonicalize(target)?;
        let mp_str = mp.to_string_lossy();
        if mp_str == "/" || mp_str.starts_with("/System") {
            return Err(CaissonError::Message(format!(
                "refusing to shield protected macOS system mount '{mp_str}'"
            )));
        }
        let info = diskutil_info(&mp_str)?;
        let uuid = info
            .volume_uuid
            .as_deref()
            .or(info.disk_uuid.as_deref())
            .ok_or_else(|| {
                CaissonError::Message(format!("could not resolve APFS volume UUID for '{mp_str}'"))
            })?;
        verify_mount_root_matches_uuid(&mp, uuid)?;
        let (_, live_vol) = apfs.find_volume_by_uuid(uuid).ok_or_else(|| {
            CaissonError::Message(format!(
                "path '{mp_str}' is not an APFS volume listed by `diskutil apfs list`"
            ))
        })?;
        if let Some(role) = live_vol.has_protected_role() {
            return Err(CaissonError::Message(format!(
                "refusing to shield APFS volume '{mp_str}' with protected system role '{role}'"
            )));
        }
        (mp, nobrowse_override.unwrap_or(false), invoking_user())
    } else {
        return Err(CaissonError::VolumeNotFound(target.to_string()));
    };

    apply_shields(
        &mount_point,
        &ShieldOptions {
            nobrowse,
            owner,
            dry_run,
        },
    )
}

/// Destroy a Caisson-managed volume after unconditionally enforcing the triple-check safety guard.
///
/// When interactive confirmation is used, re-verifies live APFS state, mount identity, protected
/// role, and on-volume marker immediately AFTER confirmation to prevent swap attacks during prompt.
pub fn destroy_volume(
    target: &str,
    registry_path: &Path,
    yes: bool,
    dry_run: bool,
) -> Result<VolumeDestroyReport> {
    let run_live_verification = || -> Result<crate::apfs::diskutil::VerifiedDestroyTarget> {
        let registry = VolumeRegistry::load_from(registry_path)?;
        let apfs = list_apfs()?;
        let live_mount = registry
            .find_by_name(target)
            .or_else(|| registry.find_by_uuid(target))
            .and_then(|reg| diskutil_info(&reg.uuid).ok())
            .and_then(|info| info.mount_point)
            .filter(|s| !s.trim().is_empty())
            .map(PathBuf::from);
        verify_destroy_safety(target, &registry, &apfs, live_mount.as_deref(), true)
    };

    // Pre-confirmation triple-check safety guard (ARCHITECTURE.md §6)
    let mut verified = run_live_verification()?;
    let cmd = format!("diskutil apfs deleteVolume {}", verified.registered.uuid);

    if dry_run {
        return Ok(VolumeDestroyReport {
            name: verified.registered.name,
            uuid: verified.registered.uuid,
            device_identifier: verified.live_volume.device_identifier,
            mount_point: verified.mount_point.display().to_string(),
            dry_run: true,
            safety_verified: true,
            command: cmd,
        });
    }

    if !yes {
        confirm_destroy_interactive(&verified.registered.name, &verified.registered.uuid)?;
        // Re-verify live state, mount root identity, role, and marker immediately after prompt.
        let rechecked = run_live_verification()?;
        if !rechecked
            .registered
            .uuid
            .eq_ignore_ascii_case(&verified.registered.uuid)
        {
            return Err(CaissonError::DestroySafetyViolation {
                volume: target.to_string(),
                reason: "target volume identity changed during confirmation prompt".to_string(),
            });
        }
        verified = rechecked;
    }

    if !is_root() {
        return Err(CaissonError::RequiresRoot {
            operation: "vol destroy".to_string(),
        });
    }

    let target_uuid = verified.registered.uuid.clone();
    execute_delete_volume(&target_uuid)?;
    VolumeRegistry::with_locked_update(registry_path, |reg| {
        reg.remove_by_uuid(&target_uuid);
        Ok(())
    })?;

    Ok(VolumeDestroyReport {
        name: verified.registered.name,
        uuid: verified.registered.uuid,
        device_identifier: verified.live_volume.device_identifier,
        mount_point: verified.mount_point.display().to_string(),
        dry_run: false,
        safety_verified: true,
        command: format!("diskutil apfs deleteVolume {target_uuid}"),
    })
}

fn confirm_destroy_interactive(volume_name: &str, uuid: &str) -> Result<()> {
    let is_tty = unsafe { libc::isatty(libc::STDIN_FILENO) != 0 };
    if !is_tty {
        return Err(CaissonError::ConfirmationRequired(volume_name.to_string()));
    }
    eprint!(
        "WARNING: Permanently destroying APFS volume '{volume_name}' ({uuid}).\nType '{volume_name}' to confirm: "
    );
    let _ = io::stderr().flush();
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    if input.trim() != volume_name {
        return Err(CaissonError::ConfirmationRequired(volume_name.to_string()));
    }
    Ok(())
}

/// Build the full unprivileged `caisson status` report.
pub fn build_status_report(registry_path: &Path, config: &CaissonConfig) -> Result<StatusReport> {
    let internal_container = detect_internal_container()
        .ok()
        .map(|c| ContainerSummary::from(&c));
    let volumes = list_registered_volumes(registry_path)?;
    let tier1_root = config.resolve_root();
    let tier1_root_exists = tier1_root.exists();
    let daemon = daemon_status(Path::new(LAUNCH_DAEMON_PLIST_PATH));

    Ok(StatusReport {
        internal_container,
        tier1_root: tier1_root.display().to_string(),
        tier1_root_exists,
        volumes,
        daemon,
        config: config.clone(),
    })
}

pub fn default_tier1_name() -> String {
    DEFAULT_TIER1_VOLUME_NAME.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::units::GIB;
    use tempfile::tempdir;

    #[test]
    fn parses_quota_specs() {
        assert!(matches!(QuotaSpec::parse("auto").unwrap(), QuotaSpec::Auto));
        assert!(matches!(QuotaSpec::parse("none").unwrap(), QuotaSpec::None));
        assert!(matches!(QuotaSpec::parse("off").unwrap(), QuotaSpec::None));
        assert!(matches!(
            QuotaSpec::parse("300g").unwrap(),
            QuotaSpec::Bytes(b) if b == 300 * GIB
        ));
    }

    #[test]
    fn dry_run_init_succeeds_unprivileged() {
        let dir = tempdir().unwrap();
        let reg_path = dir.path().join("registry.json");
        let opts = CreateVolumeOptions {
            name: "caisson-dryrun-test".to_string(),
            tier: VolumeTier::Tier1,
            container: None,
            mount_point: None,
            quota: QuotaSpec::Auto,
            case_sensitive: false,
            nobrowse: false,
            dry_run: true,
        };
        let report = create_volume(&opts, &reg_path).unwrap();
        assert!(report.dry_run);
        assert!(report.diskutil_command.contains("diskutil apfs addVolume"));
        assert!(report.diskutil_command.contains("caisson-dryrun-test"));
        assert_eq!(report.initialized_dirs.len(), 3);
        assert!(!reg_path.exists());
    }

    #[test]
    fn shield_volume_rejects_system_mounts_even_in_dry_run() {
        let dir = tempdir().unwrap();
        let reg_path = dir.path().join("registry.json");
        assert!(shield_volume("/", &reg_path, None, true).is_err());
        assert!(shield_volume("/System/Volumes/Data", &reg_path, None, true).is_err());
    }
}
