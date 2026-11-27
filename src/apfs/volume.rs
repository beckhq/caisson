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

    let apfs = list_apfs()?;
    let container = match opts.container {
        Some(ref c_ref) => apfs
            .find_container(c_ref)
            .cloned()
            .ok_or_else(|| CaissonError::Message(format!("APFS container '{c_ref}' not found")))?,
        None => detect_internal_container()?,
    };

    if apfs.find_volume_by_name(&opts.name).is_some() {
        return Err(CaissonError::Message(format!(
            "APFS volume '{}' already exists; run `sudo caisson vol shield {}` to re-apply shields or choose another name",
            opts.name, opts.name
        )));
    }

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

    // Re-read APFS plist structurally to capture the new volume's UUID and actual mount point.
    let updated_apfs = list_apfs()?;
    let (_, created_vol) = updated_apfs
        .find_volume_by_name(&opts.name)
        .ok_or_else(|| {
            CaissonError::Message(format!(
                "created volume '{}' was not found in `diskutil apfs list -plist`",
                opts.name
            ))
        })?;

    let actual_mount = diskutil_info(&created_vol.uuid)
        .ok()
        .and_then(|info| info.mount_point)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| mount_point.clone());

    let registered = RegisteredVolume {
        name: opts.name.clone(),
        uuid: created_vol.uuid.clone(),
        container: container.container_reference.clone(),
        mount_point: actual_mount.display().to_string(),
        tier: opts.tier,
        case_sensitive: opts.case_sensitive,
        nobrowse: opts.nobrowse,
        quota_bytes,
        owner: owner.clone(),
        created_at: Utc::now(),
    };

    // Write on-volume ownership marker `.caisson-volume.json`
    VolumeMarker::from_registered(&registered).write_to(&actual_mount)?;

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

    let mut registry = VolumeRegistry::load_from(registry_path)?;
    registry.upsert(registered);
    registry.save_to(registry_path)?;

    Ok(VolumeCreateReport {
        name: opts.name.clone(),
        tier: opts.tier,
        container: container.container_reference,
        container_capacity_bytes: container.capacity_ceiling,
        mount_point: actual_mount.display().to_string(),
        uuid: Some(created_vol.uuid.clone()),
        quota_bytes,
        case_sensitive: opts.case_sensitive,
        nobrowse: opts.nobrowse,
        owner,
        dry_run: false,
        diskutil_command,
        shield_report,
        initialized_dirs: created_dirs,
    })
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

/// Idempotently re-apply shields to a Caisson volume by name, UUID, or explicit mount path.
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
    let (mount_point, nobrowse, owner) = if let Some(vol) = registry
        .find_by_name(target)
        .or_else(|| registry.find_by_uuid(target))
    {
        let live_mount = diskutil_info(&vol.uuid)
            .ok()
            .and_then(|info| info.mount_point)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| vol.mount_point.clone());
        (
            PathBuf::from(live_mount),
            nobrowse_override.unwrap_or(vol.nobrowse),
            vol.owner.clone().or_else(invoking_user),
        )
    } else if Path::new(target).is_absolute() {
        (
            PathBuf::from(target),
            nobrowse_override.unwrap_or(false),
            invoking_user(),
        )
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
pub fn destroy_volume(
    target: &str,
    registry_path: &Path,
    yes: bool,
    dry_run: bool,
) -> Result<VolumeDestroyReport> {
    let mut registry = VolumeRegistry::load_from(registry_path)?;
    let apfs = list_apfs()?;

    // Resolve live mount point if registered
    let live_mount = registry
        .find_by_name(target)
        .or_else(|| registry.find_by_uuid(target))
        .and_then(|reg| diskutil_info(&reg.uuid).ok())
        .and_then(|info| info.mount_point)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from);

    // Unconditional triple-check safety guard (ARCHITECTURE.md §6)
    let verified = verify_destroy_safety(target, &registry, &apfs, live_mount.as_deref())?;
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
    }

    if !is_root() {
        return Err(CaissonError::RequiresRoot {
            operation: "vol destroy".to_string(),
        });
    }

    execute_delete_volume(&verified.registered.uuid)?;
    registry.remove_by_uuid(&verified.registered.uuid);
    registry.save_to(registry_path)?;

    Ok(VolumeDestroyReport {
        name: verified.registered.name,
        uuid: verified.registered.uuid,
        device_identifier: verified.live_volume.device_identifier,
        mount_point: verified.mount_point.display().to_string(),
        dry_run: false,
        safety_verified: true,
        command: cmd,
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
}
