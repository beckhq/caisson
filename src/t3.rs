use crate::apfs::shield::inspect_shields;
use crate::apfs::statfs::{same_volume, statfs_path};
use crate::apfs::volume::{list_registered_volumes, verify_mount_root_matches_uuid};
use crate::error::{CaissonError, Result};
use serde::Serialize;
use serde_json::{Map, Value};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct T3SetupOptions {
    pub volume: Option<String>,
    pub path: Option<PathBuf>,
    pub dry_run: bool,
    pub yes: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct T3SetupReport {
    pub settings_path: String,
    pub previous_worktrees_directory: Option<String>,
    pub target_worktrees_directory: String,
    pub target_volume_shielded: bool,
    pub backup_path: Option<String>,
    pub changed: bool,
    pub dry_run: bool,
    pub project_script_hint: String,
    pub ui_path_hint: String,
}

pub fn t3_settings_path(home: &Path) -> PathBuf {
    home.join(".t3").join("userdata").join("settings.json")
}

/// Return true if T3 Code appears to be installed on this machine.
pub fn is_t3_installed(home: &Path) -> bool {
    t3_settings_path(home).is_file() || home.join(".t3").join("worktrees").is_dir()
}

/// Resolve T3 Code's configured `worktreesDirectory` from `~/.t3/userdata/settings.json`,
/// falling back to `~/.t3/worktrees` when T3 Code is installed without an explicit override.
pub fn resolve_t3_worktrees_dir(home: &Path) -> Option<PathBuf> {
    let settings_path = t3_settings_path(home);
    if let Ok(raw) = fs::read_to_string(&settings_path) {
        if let Ok(val) = serde_json::from_str::<Value>(&raw) {
            if let Some(dir) = val
                .get("worktreesDirectory")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                let expanded = if let Some(rest) = dir.strip_prefix("~/") {
                    home.join(rest)
                } else {
                    PathBuf::from(dir)
                };
                return Some(expanded);
            }
        }
        return Some(home.join(".t3").join("worktrees"));
    }
    let default_dir = home.join(".t3").join("worktrees");
    if default_dir.exists() {
        Some(default_dir)
    } else {
        None
    }
}

/// Pick the best healthy Caisson volume mount point (preferring the healthy volume with the most
/// available space, falling back to `/Volumes/caisson`).
pub fn recommend_t3_worktrees_path(candidate_mounts: &[&str]) -> PathBuf {
    let mut best: Option<(u64, PathBuf)> = None;
    for mp_str in candidate_mounts {
        let mp = Path::new(mp_str);
        if !mp.exists() {
            continue;
        }
        let avail = statfs_path(mp).map(|s| s.available_bytes).unwrap_or(0);
        match &best {
            Some((best_avail, _)) if avail <= *best_avail => {}
            _ => best = Some((avail, mp.to_path_buf())),
        }
    }
    let base = best
        .map(|(_, p)| p)
        .unwrap_or_else(|| PathBuf::from("/Volumes/caisson"));
    base.join("t3-worktrees")
}

/// Render plain-English next steps for pointing T3 Code at a shielded Caisson volume.
pub fn format_t3_next_steps(recommended_dir: &Path) -> String {
    format!(
        "T3 Code detected:\n\
         1. Open T3 Code -> Settings -> Worktrees -> Worktree location:\n\
            set to `{}` (or run `caisson t3 setup`)\n\
         2. (Optional, for Node/Python repos) Project Settings -> Scripts:\n\
            add `caisson wt hydrate` and enable \"Run when creating a worktree\"",
        recommended_dir.display()
    )
}

/// Verify that `~/.t3`, `~/.t3/userdata`, and `~/.t3/userdata/settings.json` are not symlinks.
fn verify_safe_t3_settings_path(home: &Path) -> Result<PathBuf> {
    let mut cur = home.to_path_buf();
    for seg in [".t3", "userdata", "settings.json"] {
        cur.push(seg);
        let meta = fs::symlink_metadata(&cur).map_err(|e| {
            if e.kind() == io::ErrorKind::NotFound {
                CaissonError::Message(format!(
                    "T3 Code settings file not found at '{}'. Open T3 Code once first.",
                    t3_settings_path(home).display()
                ))
            } else {
                CaissonError::Io(e)
            }
        })?;
        if meta.file_type().is_symlink() {
            return Err(CaissonError::Message(format!(
                "refusing to modify T3 Code settings through symlink '{}'",
                cur.display()
            )));
        }
        if seg == "settings.json" && !meta.is_file() {
            return Err(CaissonError::Message(format!(
                "'{}' is not a regular file",
                cur.display()
            )));
        }
    }
    Ok(cur)
}

/// Guided, zero-sudo setup that previews or updates T3 Code's `worktreesDirectory` in
/// `~/.t3/userdata/settings.json` with an automatic backup, concurrent-edit guard, and atomic write.
pub fn run_t3_setup(
    home: &Path,
    registry_path: &Path,
    opts: &T3SetupOptions,
) -> Result<T3SetupReport> {
    let settings_path = verify_safe_t3_settings_path(home)?;

    let registered = list_registered_volumes(registry_path).unwrap_or_default();
    let mut verified_named_volume: Option<(PathBuf, String)> = None;

    let target_dir = if let Some(ref explicit_path) = opts.path {
        explicit_path.clone()
    } else if let Some(ref vol_name) = opts.volume {
        let found = registered
            .iter()
            .find(|v| v.name == *vol_name || v.uuid.eq_ignore_ascii_case(vol_name))
            .ok_or_else(|| {
                CaissonError::Message(format!(
                    "Caisson volume '{vol_name}' not found in registry (run `caisson vol ls`)"
                ))
            })?;
        if !found.mounted {
            return Err(CaissonError::Message(format!(
                "Caisson volume '{}' ({}) is currently offline/unmounted; mount or reconnect it first so Caisson does not create directories on `/`",
                found.name, found.mount_point
            )));
        }
        let mp = PathBuf::from(&found.mount_point);
        verify_mount_root_matches_uuid(&mp, &found.uuid)?;
        verified_named_volume = Some((mp.clone(), found.uuid.clone()));
        mp.join("t3-worktrees")
    } else {
        let healthy_mounts: Vec<&str> = registered
            .iter()
            .filter(|v| v.mounted && v.shields.all_healthy)
            .map(|v| v.mount_point.as_str())
            .collect();
        if healthy_mounts.is_empty() {
            return Err(CaissonError::Message(
                "No mounted, healthy Caisson APFS volume found. Run `sudo caisson init` (or `sudo caisson vol create <name> --on /Volumes/<ssd>`) first.".to_string(),
            ));
        }
        let chosen = recommend_t3_worktrees_path(&healthy_mounts);
        if let Some(vol) = registered
            .iter()
            .find(|v| v.mounted && chosen.starts_with(Path::new(&v.mount_point)))
        {
            let mp = PathBuf::from(&vol.mount_point);
            verify_mount_root_matches_uuid(&mp, &vol.uuid)?;
            verified_named_volume = Some((mp, vol.uuid.clone()));
        }
        chosen
    };

    if !target_dir.is_absolute() {
        return Err(CaissonError::Message(format!(
            "target worktrees directory '{}' must be an absolute path",
            target_dir.display()
        )));
    }

    // Check whether target directory resides on a healthy shielded APFS volume.
    let target_volume_shielded = registered.iter().any(|v| {
        v.mounted
            && v.shields.all_healthy
            && same_volume(&target_dir, Path::new(&v.mount_point)).unwrap_or(false)
    }) || statfs_path(&target_dir)
        .ok()
        .map(|st| st.mount_point != "/" && inspect_shields(Path::new(&st.mount_point)).all_healthy)
        .unwrap_or(false);

    let raw = fs::read_to_string(&settings_path)?;
    let mut json_val: Value = serde_json::from_str(&raw).map_err(|e| {
        CaissonError::Message(format!(
            "failed to parse '{}': {e}",
            settings_path.display()
        ))
    })?;
    let obj = json_val.as_object_mut().ok_or_else(|| {
        CaissonError::Message(format!(
            "'{}' is not a JSON object",
            settings_path.display()
        ))
    })?;

    let previous = obj
        .get("worktreesDirectory")
        .and_then(|v| v.as_str())
        .map(ToString::to_string);
    let target_str = target_dir.display().to_string();
    let already_set = previous.as_deref() == Some(target_str.as_str());

    let project_script_hint =
        "Project Settings -> Scripts -> command `caisson wt hydrate` -> toggle \"Run when creating a worktree\""
            .to_string();
    let ui_path_hint =
        format!("T3 Code -> Settings -> Worktrees -> Worktree location = `{target_str}`");

    if opts.dry_run || already_set {
        return Ok(T3SetupReport {
            settings_path: settings_path.display().to_string(),
            previous_worktrees_directory: previous,
            target_worktrees_directory: target_str,
            target_volume_shielded,
            backup_path: None,
            changed: false,
            dry_run: opts.dry_run,
            project_script_hint,
            ui_path_hint,
        });
    }

    if !opts.yes {
        confirm_t3_settings_update(&settings_path, previous.as_deref(), &target_str)?;
    }

    // Re-verify live volume identity and settings snapshot after confirmation to prevent races.
    if let Some((ref mp, ref expected_uuid)) = verified_named_volume {
        verify_mount_root_matches_uuid(mp, expected_uuid)?;
    }
    let _ = verify_safe_t3_settings_path(home)?;
    let current_raw = fs::read_to_string(&settings_path)?;
    if current_raw != raw {
        return Err(CaissonError::Message(format!(
            "'{}' changed while awaiting confirmation; re-run `caisson t3 setup` so concurrent T3 Code edits are not lost",
            settings_path.display()
        )));
    }

    // Ensure the target directory exists on the verified volume.
    fs::create_dir_all(&target_dir)?;

    // Exclusive, symlink-safe backup file and atomic temporary replacement file.
    let backup_path = settings_path.with_file_name(format!(
        "settings.json.caisson.{}.bak",
        uuid::Uuid::new_v4()
    ));
    let mut bak_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&backup_path)?;
    bak_file.write_all(raw.as_bytes())?;
    bak_file.sync_all()?;
    drop(bak_file);

    update_worktrees_directory_key(obj, &target_str);
    let serialized = serde_json::to_string_pretty(&json_val)?;

    let tmp_path = settings_path.with_file_name(format!(
        ".settings.json.caisson.{}.tmp",
        uuid::Uuid::new_v4()
    ));
    let mut tmp_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp_path)?;
    tmp_file.write_all(format!("{serialized}\n").as_bytes())?;
    tmp_file.sync_all()?;
    drop(tmp_file);
    if let Err(e) = fs::rename(&tmp_path, &settings_path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(CaissonError::Io(e));
    }

    Ok(T3SetupReport {
        settings_path: settings_path.display().to_string(),
        previous_worktrees_directory: previous,
        target_worktrees_directory: target_str,
        target_volume_shielded,
        backup_path: Some(backup_path.display().to_string()),
        changed: true,
        dry_run: false,
        project_script_hint,
        ui_path_hint,
    })
}

fn update_worktrees_directory_key(obj: &mut Map<String, Value>, target: &str) {
    obj.insert(
        "worktreesDirectory".to_string(),
        Value::String(target.to_string()),
    );
}

fn confirm_t3_settings_update(
    settings_path: &Path,
    previous: Option<&str>,
    target: &str,
) -> Result<()> {
    let is_tty = unsafe { libc::isatty(libc::STDIN_FILENO) != 0 };
    if !is_tty {
        return Err(CaissonError::Message(format!(
            "refusing to edit '{}' non-interactively without `--yes` (preview first with `caisson t3 setup --dry-run`)",
            settings_path.display()
        )));
    }
    eprintln!("T3 Code settings: {}", settings_path.display());
    eprintln!(
        "  worktreesDirectory: {} -> {}",
        previous.unwrap_or("<default ~/.t3/worktrees>"),
        target
    );
    eprint!("Apply this change (creates settings.json.caisson.bak)? [y/N]: ");
    let _ = io::stderr().flush();
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let ans = input.trim().to_ascii_lowercase();
    if ans != "y" && ans != "yes" {
        return Err(CaissonError::Message(
            "cancelled by user; no files modified".to_string(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn t3_setup_dry_run_and_atomic_update_preserve_other_settings() {
        let dir = tempdir().unwrap();
        let home = dir.path().join("home");
        let target = dir.path().join("caisson-vol/t3-worktrees");
        let reg_path = dir.path().join("registry.json");

        let settings_file = t3_settings_path(&home);
        fs::create_dir_all(settings_file.parent().unwrap()).unwrap();
        fs::write(
            &settings_file,
            r#"{
  "theme": "dark",
  "worktreesDirectory": "/old/path",
  "nested": { "keep": true }
}"#,
        )
        .unwrap();

        // 1. Dry-run previews without modifying settings or creating target dir.
        let dry = run_t3_setup(
            &home,
            &reg_path,
            &T3SetupOptions {
                volume: None,
                path: Some(target.clone()),
                dry_run: true,
                yes: false,
            },
        )
        .unwrap();
        assert!(dry.dry_run);
        assert!(!dry.changed);
        assert_eq!(
            dry.previous_worktrees_directory.as_deref(),
            Some("/old/path")
        );
        assert!(!target.exists());

        // 2. Apply with `--yes` creates target dir, writes backup, updates only worktreesDirectory.
        let applied = run_t3_setup(
            &home,
            &reg_path,
            &T3SetupOptions {
                volume: None,
                path: Some(target.clone()),
                dry_run: false,
                yes: true,
            },
        )
        .unwrap();
        assert!(applied.changed);
        assert!(target.is_dir());
        assert!(Path::new(applied.backup_path.as_deref().unwrap()).is_file());

        let updated: Value =
            serde_json::from_str(&fs::read_to_string(&settings_file).unwrap()).unwrap();
        assert_eq!(updated["theme"], "dark");
        assert_eq!(updated["nested"]["keep"], true);
        assert_eq!(updated["worktreesDirectory"], target.display().to_string());

        // 3. Running again is an idempotent no-op.
        let repeat = run_t3_setup(
            &home,
            &reg_path,
            &T3SetupOptions {
                volume: None,
                path: Some(target),
                dry_run: false,
                yes: true,
            },
        )
        .unwrap();
        assert!(!repeat.changed);
    }
}
