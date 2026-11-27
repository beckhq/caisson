use crate::apfs::diskutil::{
    diskutil_info, execute_mount_volume, execute_unmount_volume, is_custom_mount_point, list_apfs,
    validate_safe_custom_mount_dir,
};
use crate::apfs::shield::{apply_shields, ShieldApplyReport, ShieldOptions};
use crate::apfs::statfs::is_root;
use crate::apfs::volume::verify_mount_root_matches_uuid;
use crate::error::{CaissonError, Result};
use crate::registry::{VolumeMarker, VolumeRegistry};
use plist::{Dictionary, Value};
use serde::Serialize;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

pub const LAUNCH_DAEMON_LABEL: &str = "dev.caisson.mount";
pub const LAUNCH_DAEMON_PLIST_PATH: &str = "/Library/LaunchDaemons/dev.caisson.mount.plist";
pub const LAUNCH_DAEMON_LOG_PATH: &str = "/var/log/caisson-mount.log";
pub const LAUNCH_DAEMON_BIN_PATH: &str = "/Library/Application Support/caisson/bin/caisson";

#[derive(Debug, Clone, Serialize)]
pub struct DaemonActionReport {
    pub action: String,
    pub label: String,
    pub plist_path: String,
    pub executable_path: Option<String>,
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

impl MountAllReport {
    pub fn has_errors(&self) -> bool {
        self.volumes.iter().any(|v| v.error.is_some())
    }
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
    dict.insert("StartOnMount".to_string(), Value::Boolean(true));
    dict.insert("ThrottleInterval".to_string(), Value::Integer(5.into()));
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

/// Validate that `bin_path` is an absolute, canonical regular file owned by root (`uid == 0`),
/// not writable by group/other, and that every ancestor directory up to `/` is also owned by root
/// and not writable by group/other (preventing non-root privilege escalation via boot launchd).
pub fn validate_root_owned_executable(bin_path: &Path) -> Result<PathBuf> {
    if !bin_path.is_absolute() {
        return Err(CaissonError::Message(format!(
            "LaunchDaemon executable path must be absolute: '{}'",
            bin_path.display()
        )));
    }
    let canonical = fs::canonicalize(bin_path)?;
    let meta = fs::symlink_metadata(&canonical)?;
    if !meta.is_file() {
        return Err(CaissonError::Message(format!(
            "LaunchDaemon executable '{}' is not a regular file",
            canonical.display()
        )));
    }
    if meta.uid() != 0 {
        return Err(CaissonError::Message(format!(
            "refusing to run non-root-owned executable '{}' as a root LaunchDaemon (owner uid={}); omit `--bin` so Caisson installs a root-owned copy to `{LAUNCH_DAEMON_BIN_PATH}`",
            canonical.display(),
            meta.uid()
        )));
    }
    if (meta.mode() & 0o022) != 0 {
        return Err(CaissonError::Message(format!(
            "refusing to run group/world-writable executable '{}' as a root LaunchDaemon (mode {:o})",
            canonical.display(),
            meta.mode() & 0o7777
        )));
    }

    // Walk every ancestor directory up to `/` and confirm root ownership & no group/world write.
    let mut cur = canonical.parent();
    while let Some(dir) = cur {
        let dir_meta = fs::symlink_metadata(dir)?;
        if dir_meta.uid() != 0 {
            return Err(CaissonError::Message(format!(
                "refusing to run LaunchDaemon executable beneath non-root-owned directory '{}' (uid={})",
                dir.display(),
                dir_meta.uid()
            )));
        }
        // Note: on macOS `/Library` and `/Library/Application Support` have `root:admin` mode 0755
        // (or sticky bit), so reject group/world write (`0o022`) unless sticky is NOT sufficient.
        if (dir_meta.mode() & 0o022) != 0 {
            return Err(CaissonError::Message(format!(
                "refusing to run LaunchDaemon executable beneath group/world-writable directory '{}' (mode {:o})",
                dir.display(),
                dir_meta.mode() & 0o7777
            )));
        }
        cur = dir.parent();
    }

    Ok(canonical)
}

/// Install a dedicated root-owned copy of `source_bin` into `dest_bin` (`root:wheel`, `0755`)
/// with root-owned `0755` parent directories.
fn install_root_binary_copy(source_bin: &Path, dest_bin: &Path) -> Result<PathBuf> {
    let canonical_src = fs::canonicalize(source_bin)?;
    let parent = dest_bin.parent().ok_or_else(|| {
        CaissonError::Message(format!(
            "invalid daemon binary path: {}",
            dest_bin.display()
        ))
    })?;
    fs::create_dir_all(parent)?;
    fs::set_permissions(parent, fs::Permissions::from_mode(0o755))?;
    let parent_str = parent.display().to_string();
    let _ = Command::new("chown")
        .args(["root:wheel", &parent_str])
        .output();

    // Also harden `/Library/Application Support/caisson` itself.
    if let Some(grandparent) = parent.parent() {
        if grandparent.file_name().and_then(|s| s.to_str()) == Some("caisson") {
            let _ = fs::set_permissions(grandparent, fs::Permissions::from_mode(0o755));
            let _ = Command::new("chown")
                .args(["root:wheel", &grandparent.display().to_string()])
                .output();
        }
    }

    let tmp = parent.join(format!(".caisson.{}.tmp", std::process::id()));
    fs::copy(&canonical_src, &tmp)?;
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755))?;
    let tmp_str = tmp.display().to_string();
    let chown = Command::new("chown")
        .args(["root:wheel", &tmp_str])
        .output()?;
    if !chown.status.success() {
        let _ = fs::remove_file(&tmp);
        return Err(CaissonError::CommandFailed {
            cmd: format!("chown root:wheel {tmp_str}"),
            code: chown.status.code(),
            stderr: String::from_utf8_lossy(&chown.stderr).trim().to_string(),
        });
    }
    fs::rename(&tmp, dest_bin)?;
    validate_root_owned_executable(dest_bin)
}

/// Install and bootstrap `/Library/LaunchDaemons/dev.caisson.mount.plist`.
///
/// When `explicit_bin` is `None`, installs a dedicated root-owned copy of the current binary at
/// `/Library/Application Support/caisson/bin/caisson` so launchd never executes a user-replaceable
/// binary (e.g. under Homebrew or `~/.cargo/bin`). When `explicit_bin` is `Some(path)`, validates
/// that `path` and every ancestor directory are root-owned and not writable by unprivileged users.
pub fn install_daemon(
    explicit_bin: Option<&Path>,
    plist_path: &Path,
    dry_run: bool,
) -> Result<DaemonActionReport> {
    let plist_str = plist_path.display().to_string();
    let (target_bin, mut commands) = match explicit_bin {
        Some(p) => {
            if !dry_run {
                let validated = validate_root_owned_executable(p)?;
                (validated, Vec::new())
            } else {
                let canonical = fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
                (
                    canonical.clone(),
                    vec![format!(
                        "verify root-owned non-writable executable {}",
                        canonical.display()
                    )],
                )
            }
        }
        None => {
            let current = std::env::current_exe()?;
            let dest = PathBuf::from(LAUNCH_DAEMON_BIN_PATH);
            let cmds = vec![format!(
                "install -o root -g wheel -m 0755 {} {}",
                current.display(),
                dest.display()
            )];
            (dest, cmds)
        }
    };

    let xml = generate_plist_xml(&target_bin)?;
    commands.extend([
        format!("write {plist_str}"),
        format!("chmod 0644 {plist_str}"),
        format!("chown root:wheel {plist_str}"),
        format!("launchctl bootout system/{LAUNCH_DAEMON_LABEL} || true"),
        format!("launchctl bootstrap system {plist_str}"),
    ]);

    if dry_run {
        return Ok(DaemonActionReport {
            action: "install".to_string(),
            label: LAUNCH_DAEMON_LABEL.to_string(),
            plist_path: plist_str,
            executable_path: Some(target_bin.display().to_string()),
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

    let final_bin = match explicit_bin {
        Some(p) => validate_root_owned_executable(p)?,
        None => {
            let current = std::env::current_exe()?;
            install_root_binary_copy(&current, Path::new(LAUNCH_DAEMON_BIN_PATH))?
        }
    };
    let final_xml = generate_plist_xml(&final_bin)?;

    if let Some(parent) = plist_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(plist_path, &final_xml)?;
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
        executable_path: Some(final_bin.display().to_string()),
        dry_run: false,
        commands,
        plist_xml: Some(final_xml),
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
            executable_path: None,
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
        executable_path: None,
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
///
/// Validates live mount identity, protected roles, and `.caisson-volume.json` UUID marker before
/// applying shields, and never falls back to a stale stored mount path.
pub fn mount_all(registry_path: &Path, dry_run: bool) -> Result<MountAllReport> {
    if !dry_run && !is_root() {
        return Err(CaissonError::RequiresRoot {
            operation: "mount-all".to_string(),
        });
    }

    let registry = VolumeRegistry::load_from(registry_path)?;
    let apfs = list_apfs()?;
    let mut results = Vec::new();

    for vol in &registry.volumes {
        let Some((_, live_vol)) = apfs.find_volume_by_uuid(&vol.uuid) else {
            if vol.is_external {
                results.push(MountAllVolumeResult {
                    name: vol.name.clone(),
                    uuid: vol.uuid.clone(),
                    mount_point: vol.mount_point.clone(),
                    mounted_action: "offline".to_string(),
                    shield_report: None,
                    error: None,
                });
            } else {
                results.push(MountAllVolumeResult {
                    name: vol.name.clone(),
                    uuid: vol.uuid.clone(),
                    mount_point: vol.mount_point.clone(),
                    mounted_action: "missing".to_string(),
                    shield_report: None,
                    error: Some(format!("volume UUID {} not found on system", vol.uuid)),
                });
            }
            continue;
        };

        if let Some(role) = live_vol.has_protected_role() {
            results.push(MountAllVolumeResult {
                name: vol.name.clone(),
                uuid: vol.uuid.clone(),
                mount_point: vol.mount_point.clone(),
                mounted_action: "rejected".to_string(),
                shield_report: None,
                error: Some(format!(
                    "refusing to mount/shield volume with protected APFS role '{role}'"
                )),
            });
            continue;
        }

        let mut mounted_action = "already-mounted".to_string();
        let expected_mp = PathBuf::from(&vol.mount_point);
        let custom_mp = is_custom_mount_point(&vol.name, &expected_mp);

        let live_mount = diskutil_info(&vol.uuid)
            .ok()
            .and_then(|info| info.mount_point)
            .filter(|s| !s.trim().is_empty());

        let mount_point = match live_mount {
            Some(mp) => {
                let live_path = PathBuf::from(&mp);
                let canon_live = fs::canonicalize(&live_path).unwrap_or_else(|_| live_path.clone());
                let canon_expected =
                    fs::canonicalize(&expected_mp).unwrap_or_else(|_| expected_mp.clone());

                if custom_mp && canon_live != canon_expected {
                    // macOS auto-mounted a custom-mountpoint volume at `/Volumes/<name>` after reboot.
                    // Verify identity & marker first, ensure the custom target dir is empty and safe,
                    // then cleanly unmount and remount at the registered custom mount point.
                    if let Err(e) = validate_safe_custom_mount_dir(&expected_mp, !dry_run) {
                        results.push(MountAllVolumeResult {
                            name: vol.name.clone(),
                            uuid: vol.uuid.clone(),
                            mount_point: vol.mount_point.clone(),
                            mounted_action: "custom-mountpoint-unsafe".to_string(),
                            shield_report: None,
                            error: Some(e.to_string()),
                        });
                        continue;
                    }
                    if dry_run {
                        mounted_action = format!(
                            "planned: diskutil unmount {} && diskutil mount -mountPoint {} {}",
                            vol.uuid,
                            expected_mp.display(),
                            vol.uuid
                        );
                        expected_mp.clone()
                    } else {
                        if let Err(e) = verify_mount_root_matches_uuid(&live_path, &vol.uuid) {
                            results.push(MountAllVolumeResult {
                                name: vol.name.clone(),
                                uuid: vol.uuid.clone(),
                                mount_point: live_path.display().to_string(),
                                mounted_action: "remount-rejected".to_string(),
                                shield_report: None,
                                error: Some(e.to_string()),
                            });
                            continue;
                        }
                        match VolumeMarker::read_from(&live_path) {
                            Ok(marker)
                                if marker.managed_by == "caisson"
                                    && marker.uuid.eq_ignore_ascii_case(&vol.uuid) => {}
                            Ok(marker) => {
                                results.push(MountAllVolumeResult {
                                    name: vol.name.clone(),
                                    uuid: vol.uuid.clone(),
                                    mount_point: live_path.display().to_string(),
                                    mounted_action: "remount-rejected".to_string(),
                                    shield_report: None,
                                    error: Some(format!(
                                        "on-volume marker mismatch (managed_by={}, uuid={})",
                                        marker.managed_by, marker.uuid
                                    )),
                                });
                                continue;
                            }
                            Err(e) => {
                                results.push(MountAllVolumeResult {
                                    name: vol.name.clone(),
                                    uuid: vol.uuid.clone(),
                                    mount_point: live_path.display().to_string(),
                                    mounted_action: "remount-rejected".to_string(),
                                    shield_report: None,
                                    error: Some(format!("failed to verify on-volume marker: {e}")),
                                });
                                continue;
                            }
                        }

                        if let Err(e) = execute_unmount_volume(&vol.uuid) {
                            results.push(MountAllVolumeResult {
                                name: vol.name.clone(),
                                uuid: vol.uuid.clone(),
                                mount_point: live_path.display().to_string(),
                                mounted_action: "unmount-failed".to_string(),
                                shield_report: None,
                                error: Some(e.to_string()),
                            });
                            continue;
                        }
                        match execute_mount_volume(&vol.uuid, Some(&expected_mp)) {
                            Ok(_) => {
                                mounted_action = "remounted-custom".to_string();
                                match diskutil_info(&vol.uuid)
                                    .ok()
                                    .and_then(|info| info.mount_point)
                                    .filter(|s| !s.trim().is_empty())
                                {
                                    Some(remounted) => PathBuf::from(remounted),
                                    None => {
                                        results.push(MountAllVolumeResult {
                                            name: vol.name.clone(),
                                            uuid: vol.uuid.clone(),
                                            mount_point: vol.mount_point.clone(),
                                            mounted_action: "mount-unverified".to_string(),
                                            shield_report: None,
                                            error: Some(format!(
                                                "diskutil mount -mountPoint succeeded for {} but no live mount point was reported",
                                                vol.uuid
                                            )),
                                        });
                                        continue;
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
                } else {
                    live_path
                }
            }
            None => {
                if custom_mp {
                    if let Err(e) = validate_safe_custom_mount_dir(&expected_mp, !dry_run) {
                        results.push(MountAllVolumeResult {
                            name: vol.name.clone(),
                            uuid: vol.uuid.clone(),
                            mount_point: vol.mount_point.clone(),
                            mounted_action: "custom-mountpoint-unsafe".to_string(),
                            shield_report: None,
                            error: Some(e.to_string()),
                        });
                        continue;
                    }
                }
                if dry_run {
                    mounted_action = if custom_mp {
                        format!(
                            "planned: diskutil mount -mountPoint {} {}",
                            expected_mp.display(),
                            vol.uuid
                        )
                    } else {
                        format!("planned: diskutil mount {}", vol.uuid)
                    };
                    expected_mp.clone()
                } else {
                    let target_mp = custom_mp.then_some(expected_mp.as_path());
                    match execute_mount_volume(&vol.uuid, target_mp) {
                        Ok(_) => {
                            mounted_action = if custom_mp {
                                "mounted-custom".to_string()
                            } else {
                                "mounted".to_string()
                            };
                            match diskutil_info(&vol.uuid)
                                .ok()
                                .and_then(|info| info.mount_point)
                                .filter(|s| !s.trim().is_empty())
                            {
                                Some(mp) => PathBuf::from(mp),
                                None => {
                                    results.push(MountAllVolumeResult {
                                        name: vol.name.clone(),
                                        uuid: vol.uuid.clone(),
                                        mount_point: vol.mount_point.clone(),
                                        mounted_action: "mount-unverified".to_string(),
                                        shield_report: None,
                                        error: Some(format!(
                                            "diskutil mount succeeded for {} but no live mount point was reported",
                                            vol.uuid
                                        )),
                                    });
                                    continue;
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
        };

        if !dry_run {
            if let Err(e) = verify_mount_root_matches_uuid(&mount_point, &vol.uuid) {
                results.push(MountAllVolumeResult {
                    name: vol.name.clone(),
                    uuid: vol.uuid.clone(),
                    mount_point: mount_point.display().to_string(),
                    mounted_action,
                    shield_report: None,
                    error: Some(e.to_string()),
                });
                continue;
            }
            match VolumeMarker::read_from(&mount_point) {
                Ok(marker)
                    if marker.managed_by == "caisson"
                        && marker.uuid.eq_ignore_ascii_case(&vol.uuid) => {}
                Ok(marker) => {
                    results.push(MountAllVolumeResult {
                        name: vol.name.clone(),
                        uuid: vol.uuid.clone(),
                        mount_point: mount_point.display().to_string(),
                        mounted_action,
                        shield_report: None,
                        error: Some(format!(
                            "on-volume marker mismatch (managed_by={}, uuid={})",
                            marker.managed_by, marker.uuid
                        )),
                    });
                    continue;
                }
                Err(e) => {
                    results.push(MountAllVolumeResult {
                        name: vol.name.clone(),
                        uuid: vol.uuid.clone(),
                        mount_point: mount_point.display().to_string(),
                        mounted_action,
                        shield_report: None,
                        error: Some(format!("failed to verify on-volume marker: {e}")),
                    });
                    continue;
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
        let xml = generate_plist_xml(Path::new(LAUNCH_DAEMON_BIN_PATH)).unwrap();
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
            dict.get("StartOnMount").and_then(|v| v.as_boolean()),
            Some(true)
        );
        assert_eq!(
            dict.get("ThrottleInterval")
                .and_then(|v| v.as_signed_integer()),
            Some(5)
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
        assert_eq!(args, vec![LAUNCH_DAEMON_BIN_PATH, "mount-all"]);
    }

    #[test]
    fn rejects_non_root_owned_executable_for_launch_daemon() {
        let dir = tempdir().unwrap();
        let fake_bin = dir.path().join("caisson");
        fs::write(&fake_bin, "#!/bin/sh\n").unwrap();
        let err = validate_root_owned_executable(&fake_bin).unwrap_err();
        assert!(err.to_string().contains("non-root-owned"));
    }

    #[test]
    fn dry_run_install_and_uninstall_do_not_mutate_disk() {
        let dir = tempdir().unwrap();
        let plist_path = dir.path().join("dev.caisson.mount.plist");

        let install = install_daemon(None, &plist_path, true).unwrap();
        assert!(install.dry_run);
        assert_eq!(
            install.executable_path.as_deref(),
            Some(LAUNCH_DAEMON_BIN_PATH)
        );
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

        let xml = generate_plist_xml(Path::new(LAUNCH_DAEMON_BIN_PATH)).unwrap();
        fs::write(&plist_path, xml).unwrap();

        let present = daemon_status(&plist_path);
        assert!(present.installed);
        assert_eq!(
            present.program_arguments,
            vec![LAUNCH_DAEMON_BIN_PATH, "mount-all"]
        );
    }

    #[test]
    fn mount_all_treats_unplugged_external_volume_as_offline_without_error() {
        let dir = tempdir().unwrap();
        let reg_path = dir.path().join("registry.json");
        let ext = crate::registry::RegisteredVolume {
            name: "ext-ssd".to_string(),
            uuid: "99999999-8888-7777-6666-555555555555".to_string(),
            container: "disk99".to_string(),
            mount_point: "/Volumes/ext-ssd".to_string(),
            tier: crate::registry::VolumeTier::Tier2,
            case_sensitive: false,
            nobrowse: true,
            is_external: true,
            quota_bytes: None,
            owner: None,
            created_at: chrono::Utc::now(),
        };
        VolumeRegistry::with_locked_update(&reg_path, |r| {
            r.upsert(ext);
            Ok(())
        })
        .unwrap();

        let report = mount_all(&reg_path, true).unwrap();
        assert!(!report.has_errors());
        assert_eq!(report.volumes.len(), 1);
        assert_eq!(report.volumes[0].mounted_action, "offline");
        assert!(report.volumes[0].error.is_none());
    }
}
