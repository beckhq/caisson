use crate::apfs::diskutil::{diskutil_info, execute_mount_volume, list_apfs};
use crate::apfs::shield::{apply_shields, ShieldApplyReport, ShieldOptions};
use crate::apfs::statfs::is_root;
use crate::error::{CaissonError, Result};
use crate::registry::VolumeRegistry;
use plist::{Dictionary, Value};
use serde::Serialize;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const LAUNCH_DAEMON_LABEL: &str = "dev.caisson.mount";
pub const LAUNCH_DAEMON_PLIST_PATH: &str = "/Library/LaunchDaemons/dev.caisson.mount.plist";
pub const LAUNCH_DAEMON_LOG_PATH: &str = "/var/log/caisson-mount.log";

#[derive(Debug, Clone, Serialize)]
pub struct DaemonActionReport {
    pub action: String,
    pub label: String,
    pub plist_path: String,
    pub dry_run: bool,
    pub commands: Vec<String>,
    pub plist_xml: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DaemonStatus {
    pub label: String,
    pub plist_path: String,
    pub installed: bool,
    pub loaded: bool,
    pub program_arguments: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MountAllVolumeResult {
    pub name: String,
    pub uuid: String,
    pub mount_point: String,
    pub mounted_action: String,
    pub shield_report: Option<ShieldApplyReport>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MountAllReport {
    pub dry_run: bool,
    pub volumes: Vec<MountAllVolumeResult>,
}

/// Generate the auditable one-shot LaunchDaemon plist XML (`ARCHITECTURE.md` §4).
/// `RunAtLoad = true`, `KeepAlive = false`: runs `caisson mount-all` once at boot and exits.
pub fn generate_plist_xml(caisson_bin: &Path) -> Result<String> {
    let mut dict = Dictionary::new();
    dict.insert(
        "Label".to_string(),
        Value::String(LAUNCH_DAEMON_LABEL.to_string()),
    );
    dict.insert(
        "ProgramArguments".to_string(),
        Value::Array(vec![
            Value::String(caisson_bin.display().to_string()),
            Value::String("mount-all".to_string()),
        ]),
    );
    dict.insert("RunAtLoad".to_string(), Value::Boolean(true));
    dict.insert("KeepAlive".to_string(), Value::Boolean(false));
    dict.insert(
        "StandardOutPath".to_string(),
        Value::String(LAUNCH_DAEMON_LOG_PATH.to_string()),
    );
    dict.insert(
        "StandardErrorPath".to_string(),
        Value::String(LAUNCH_DAEMON_LOG_PATH.to_string()),
    );

    let mut buf = Vec::new();
    plist::to_writer_xml(&mut buf, &Value::Dictionary(dict))?;
    let xml = String::from_utf8(buf)
        .map_err(|e| CaissonError::Message(format!("invalid utf-8 plist: {e}")))?;
    Ok(xml)
}

/// Install and bootstrap `/Library/LaunchDaemons/dev.caisson.mount.plist`.
pub fn install_daemon(
    caisson_bin: &Path,
    plist_path: &Path,
    dry_run: bool,
) -> Result<DaemonActionReport> {
    let xml = generate_plist_xml(caisson_bin)?;
    let plist_str = plist_path.display().to_string();
    let commands = vec![
        format!("write {plist_str}"),
        format!("chmod 0644 {plist_str}"),
        format!("chown root:wheel {plist_str}"),
        format!("launchctl bootout system/{LAUNCH_DAEMON_LABEL} || true"),
        format!("launchctl bootstrap system {plist_str}"),
    ];

    if dry_run {
        return Ok(DaemonActionReport {
            action: "install".to_string(),
            label: LAUNCH_DAEMON_LABEL.to_string(),
            plist_path: plist_str,
            dry_run: true,
            commands,
            plist_xml: Some(xml),
        });
    }

    if !is_root() {
        return Err(CaissonError::RequiresRoot {
            operation: "daemon install".to_string(),
        });
    }

    if let Some(parent) = plist_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(plist_path, &xml)?;
    fs::set_permissions(plist_path, fs::Permissions::from_mode(0o644))?;

    let chown = Command::new("chown")
        .args(["root:wheel", &plist_str])
        .output()?;
    if !chown.status.success() {
        return Err(CaissonError::CommandFailed {
            cmd: format!("chown root:wheel {plist_str}"),
            code: chown.status.code(),
            stderr: String::from_utf8_lossy(&chown.stderr).trim().to_string(),
        });
    }

    // Bootout previous registration if present (ignore non-zero if not loaded).
    let _ = Command::new("launchctl")
        .args(["bootout", &format!("system/{LAUNCH_DAEMON_LABEL}")])
        .output();

    let bootstrap = Command::new("launchctl")
        .args(["bootstrap", "system", &plist_str])
        .output()?;
    if !bootstrap.status.success() {
        return Err(CaissonError::CommandFailed {
            cmd: format!("launchctl bootstrap system {plist_str}"),
            code: bootstrap.status.code(),
            stderr: String::from_utf8_lossy(&bootstrap.stderr)
                .trim()
                .to_string(),
        });
    }

    Ok(DaemonActionReport {
        action: "install".to_string(),
        label: LAUNCH_DAEMON_LABEL.to_string(),
        plist_path: plist_str,
        dry_run: false,
        commands,
        plist_xml: Some(xml),
    })
}

/// Bootout and remove `/Library/LaunchDaemons/dev.caisson.mount.plist`.
pub fn uninstall_daemon(plist_path: &Path, dry_run: bool) -> Result<DaemonActionReport> {
    let plist_str = plist_path.display().to_string();
    let commands = vec![
        format!("launchctl bootout system/{LAUNCH_DAEMON_LABEL} || true"),
        format!("rm -f {plist_str}"),
    ];

    if dry_run {
        return Ok(DaemonActionReport {
            action: "uninstall".to_string(),
            label: LAUNCH_DAEMON_LABEL.to_string(),
            plist_path: plist_str,
            dry_run: true,
            commands,
            plist_xml: None,
        });
    }

    if !is_root() {
        return Err(CaissonError::RequiresRoot {
            operation: "daemon uninstall".to_string(),
        });
    }

    let _ = Command::new("launchctl")
        .args(["bootout", &format!("system/{LAUNCH_DAEMON_LABEL}")])
        .output();

    if plist_path.exists() {
        fs::remove_file(plist_path)?;
    }

    Ok(DaemonActionReport {
        action: "uninstall".to_string(),
        label: LAUNCH_DAEMON_LABEL.to_string(),
        plist_path: plist_str,
        dry_run: false,
        commands,
        plist_xml: None,
    })
}

/// Read-only LaunchDaemon status check (unprivileged).
pub fn daemon_status(plist_path: &Path) -> DaemonStatus {
    let installed = plist_path.exists();
    let program_arguments = if installed {
        fs::read(plist_path)
            .ok()
            .and_then(|bytes| Value::from_reader_xml(&bytes[..]).ok())
            .and_then(|val| val.as_dictionary().cloned())
            .and_then(|dict| {
                dict.get("ProgramArguments")
                    .and_then(|v| v.as_array())
                    .cloned()
            })
            .map(|arr| {
                arr.into_iter()
                    .filter_map(|v| v.as_string().map(ToString::to_string))
                    .collect()
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    let loaded = Command::new("launchctl")
        .args(["print", &format!("system/{LAUNCH_DAEMON_LABEL}")])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    DaemonStatus {
        label: LAUNCH_DAEMON_LABEL.to_string(),
        plist_path: plist_path.display().to_string(),
        installed,
        loaded,
        program_arguments,
    }
}

/// Re-mount registered Caisson volumes if unmounted and re-apply `noatime` / `nobrowse` shields.
pub fn mount_all(registry_path: &Path, dry_run: bool) -> Result<MountAllReport> {
    if !dry_run && !is_root() {
        return Err(CaissonError::RequiresRoot {
            operation: "mount-all".to_string(),
        });
    }

    let registry = VolumeRegistry::load_from(registry_path)?;
    let apfs = list_apfs().ok();
    let mut results = Vec::new();

    for vol in &registry.volumes {
        let mut mounted_action = "already-mounted".to_string();
        let mut mount_point = PathBuf::from(&vol.mount_point);

        // Check live mount point via diskutil info -plist <uuid>
        let live_mount = diskutil_info(&vol.uuid)
            .ok()
            .and_then(|info| info.mount_point)
            .filter(|s| !s.is_empty());

        if let Some(mp) = live_mount {
            mount_point = PathBuf::from(mp);
        } else {
            // Check whether volume exists at all in APFS list before attempting mount
            let exists_in_apfs = apfs
                .as_ref()
                .map(|a| a.find_volume_by_uuid(&vol.uuid).is_some())
                .unwrap_or(true);
            if !exists_in_apfs {
                results.push(MountAllVolumeResult {
                    name: vol.name.clone(),
                    uuid: vol.uuid.clone(),
                    mount_point: vol.mount_point.clone(),
                    mounted_action: "missing".to_string(),
                    shield_report: None,
                    error: Some(format!("volume UUID {} not found on system", vol.uuid)),
                });
                continue;
            }

            if dry_run {
                mounted_action = format!("planned: diskutil mount {}", vol.uuid);
            } else {
                match execute_mount_volume(&vol.uuid) {
                    Ok(_) => {
                        mounted_action = "mounted".to_string();
                        if let Ok(info) = diskutil_info(&vol.uuid) {
                            if let Some(mp) = info.mount_point.filter(|s| !s.is_empty()) {
                                mount_point = PathBuf::from(mp);
                            }
                        }
                    }
                    Err(e) => {
                        results.push(MountAllVolumeResult {
                            name: vol.name.clone(),
                            uuid: vol.uuid.clone(),
                            mount_point: vol.mount_point.clone(),
                            mounted_action: "mount-failed".to_string(),
                            shield_report: None,
                            error: Some(e.to_string()),
                        });
                        continue;
                    }
                }
            }
        }

        let shield_opts = ShieldOptions {
            nobrowse: vol.nobrowse,
            owner: vol.owner.clone(),
            dry_run,
        };
        match apply_shields(&mount_point, &shield_opts) {
            Ok(report) => results.push(MountAllVolumeResult {
                name: vol.name.clone(),
                uuid: vol.uuid.clone(),
                mount_point: mount_point.display().to_string(),
                mounted_action,
                shield_report: Some(report),
                error: None,
            }),
            Err(e) => results.push(MountAllVolumeResult {
                name: vol.name.clone(),
                uuid: vol.uuid.clone(),
                mount_point: mount_point.display().to_string(),
                mounted_action,
                shield_report: None,
                error: Some(e.to_string()),
            }),
        }
    }

    Ok(MountAllReport {
        dry_run,
        volumes: results,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn generates_valid_one_shot_launchd_plist() {
        let xml = generate_plist_xml(Path::new("/usr/local/bin/caisson")).unwrap();
        let val = Value::from_reader_xml(xml.as_bytes()).unwrap();
        let dict = val.as_dictionary().unwrap();

        assert_eq!(
            dict.get("Label").and_then(|v| v.as_string()),
            Some(LAUNCH_DAEMON_LABEL)
        );
        assert_eq!(
            dict.get("RunAtLoad").and_then(|v| v.as_boolean()),
            Some(true)
        );
        assert_eq!(
            dict.get("KeepAlive").and_then(|v| v.as_boolean()),
            Some(false)
        );
        let args: Vec<&str> = dict
            .get("ProgramArguments")
            .and_then(|v| v.as_array())
            .unwrap()
            .iter()
            .filter_map(|v| v.as_string())
            .collect();
        assert_eq!(args, vec!["/usr/local/bin/caisson", "mount-all"]);
    }

    #[test]
    fn dry_run_install_and_uninstall_do_not_mutate_disk() {
        let dir = tempdir().unwrap();
        let plist_path = dir.path().join("dev.caisson.mount.plist");

        let install =
            install_daemon(Path::new("/usr/local/bin/caisson"), &plist_path, true).unwrap();
        assert!(install.dry_run);
        assert!(install.plist_xml.is_some());
        assert!(!plist_path.exists());

        let uninstall = uninstall_daemon(&plist_path, true).unwrap();
        assert!(uninstall.dry_run);
        assert_eq!(uninstall.commands.len(), 2);
    }

    #[test]
    fn daemon_status_reads_installed_plist() {
        let dir = tempdir().unwrap();
        let plist_path = dir.path().join("dev.caisson.mount.plist");

        let missing = daemon_status(&plist_path);
        assert!(!missing.installed);
        assert!(missing.program_arguments.is_empty());

        let xml = generate_plist_xml(Path::new("/opt/homebrew/bin/caisson")).unwrap();
        fs::write(&plist_path, xml).unwrap();

        let present = daemon_status(&plist_path);
        assert!(present.installed);
        assert_eq!(
            present.program_arguments,
            vec!["/opt/homebrew/bin/caisson", "mount-all"]
        );
    }
}
