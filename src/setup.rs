use crate::agents::{dismiss_agents, inspect_agents, install_agent_skills, resolve_user_home};
use crate::apfs::diskutil::{
    compute_auto_quota, detect_internal_container, diskutil_info, list_apfs, validate_volume_name,
};
use crate::apfs::statfs::{is_root, statfs_path};
use crate::apfs::volume::{
    create_volume, list_registered_volumes, sanitize_display_label, CreateVolumeOptions, QuotaSpec,
    VolumeStatusEntry,
};
use crate::config::{CaissonConfig, DEFAULT_TIER1_MOUNT, DEFAULT_TIER1_VOLUME_NAME};
use crate::daemon::launchd::{daemon_status, install_daemon, LAUNCH_DAEMON_PLIST_PATH};
use crate::doctor::{
    inspect_daemon_binary_version, probe_syspolicyd_exec_latency, SyspolicydProbe,
};
use crate::error::{CaissonError, Result};
use crate::registry::VolumeTier;
use crate::t3::{is_t3_installed, resolve_t3_worktrees_dir, run_t3_setup, T3SetupOptions};
use crate::tui::{
    bar_meter, confirm, format_banner, input_line, is_interactive_tty, multi_select,
    print_rail_line, print_step_done, print_step_header, select_one, Palette, SelectOption,
    ToggleOption,
};
use crate::units::format_bytes;
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const SUDO_BIN: &str = "/usr/bin/sudo";
const DEV_TOOLS_SETTINGS_URL: &str =
    "x-apple.systempreferences:com.apple.PrivacySecurity.Settings?Privacy_DevTools";

#[derive(Debug, Clone, Default)]
pub struct SetupOptions {
    pub dry_run: bool,
    pub no_daemon: bool,
    pub no_shell: bool,
    pub no_agents: bool,
    pub no_t3: bool,
    pub no_probe: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SetupReport {
    pub dry_run: bool,
    pub volume_name: String,
    pub volume_mount: String,
    pub volume_created: bool,
    pub daemon_installed: bool,
    pub shell_configured: Option<String>,
    pub agents_installed: Vec<String>,
    pub t3_configured: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub syspolicyd_probe: Option<SyspolicydProbe>,
    pub planned_commands: Vec<String>,
}

#[derive(Debug, Clone)]
enum DriveChoice {
    KeepExisting(VolumeStatusEntry),
    CreateOnContainer {
        container_ref: String,
        suggested_name: String,
        capacity_ceiling: u64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetectedShell {
    Zsh,
    Bash,
    Fish,
}

impl DetectedShell {
    pub fn from_env() -> Self {
        let shell = std::env::var("SHELL").unwrap_or_default();
        if shell.ends_with("/fish") {
            Self::Fish
        } else if shell.ends_with("/bash") {
            Self::Bash
        } else {
            Self::Zsh
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Zsh => "zsh",
            Self::Bash => "bash",
            Self::Fish => "fish",
        }
    }

    pub fn rc_relative_path(self) -> &'static str {
        match self {
            Self::Zsh => ".zshrc",
            Self::Bash => ".bashrc",
            Self::Fish => ".config/fish/config.fish",
        }
    }

    pub fn init_line(self) -> &'static str {
        match self {
            Self::Zsh => r#"eval "$(caisson shell-init zsh --alias cai)""#,
            Self::Bash => r#"eval "$(caisson shell-init bash --alias cai)""#,
            Self::Fish => "caisson shell-init fish --alias cai | source",
        }
    }
}

/// Sanitize an external volume label into a valid volume name suffix (e.g. `caisson-work-ssd`).
pub fn suggest_volume_name(
    existing_names: &[String],
    default_mount_exists: bool,
    drive_label: Option<&str>,
) -> String {
    let base_taken = existing_names
        .iter()
        .any(|n| n.eq_ignore_ascii_case(DEFAULT_TIER1_VOLUME_NAME))
        || default_mount_exists;
    if !base_taken {
        return DEFAULT_TIER1_VOLUME_NAME.to_string();
    }
    let suffix: String = drive_label
        .unwrap_or("ssd")
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_string();
    let suffix = if suffix.is_empty() {
        "ssd".to_string()
    } else {
        suffix
    };
    let candidate = format!("caisson-{suffix}");
    if !existing_names
        .iter()
        .any(|n| n.eq_ignore_ascii_case(&candidate))
    {
        candidate
    } else {
        format!("{candidate}-2")
    }
}

/// Safely inspect whether `caisson shell-init` is already present in `rc_path` (rejecting symlinks).
pub fn shell_rc_has_caisson_init(home: &Path, rel: &str) -> Result<bool> {
    let path = home.join(rel);
    verify_no_symlink_components(home, Path::new(rel))?;
    match fs::symlink_metadata(&path) {
        Ok(meta) => {
            if !meta.is_file() {
                return Err(CaissonError::Message(format!(
                    "refusing to edit non-regular shell profile '{}'",
                    path.display()
                )));
            }
            if meta.len() > 4 * 1024 * 1024 {
                return Err(CaissonError::Message(format!(
                    "shell profile '{}' exceeds 4 MiB safety limit",
                    path.display()
                )));
            }
            let content = fs::read_to_string(&path)?;
            Ok(content.contains("caisson shell-init"))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(CaissonError::Io(e)),
    }
}

/// Safely append the `caisson shell-init` line to the user's shell RC file via a pinned parent
/// directory descriptor (`O_NOFOLLOW | O_APPEND`), preserving permissions, ACLs, and concurrent edits.
pub fn append_shell_init_line(home: &Path, shell: DetectedShell, dry_run: bool) -> Result<bool> {
    use std::ffi::CString;
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;

    let rel = shell.rc_relative_path();
    if shell_rc_has_caisson_init(home, rel)? {
        return Ok(false);
    }
    if dry_run {
        return Ok(true);
    }
    verify_no_symlink_components(home, Path::new(rel))?;

    let rel_path = Path::new(rel);
    let comps: Vec<&std::ffi::OsStr> = rel_path
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => Some(s),
            _ => None,
        })
        .collect();
    let Some((leaf, parent_comps)) = comps.split_last() else {
        return Err(CaissonError::Message(
            "invalid shell profile path".to_string(),
        ));
    };

    // Pin `home` and walk/create parent components with `openat(O_DIRECTORY | O_NOFOLLOW)`.
    let c_home = CString::new(home.as_os_str().as_bytes())
        .map_err(|_| CaissonError::Message("invalid home path".to_string()))?;
    let home_fd = unsafe {
        libc::open(
            c_home.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if home_fd < 0 {
        return Err(CaissonError::Io(std::io::Error::last_os_error()));
    }
    let mut dir_fd = unsafe { OwnedFd::from_raw_fd(home_fd) };
    for comp in parent_comps {
        let c_comp = CString::new(comp.as_bytes())
            .map_err(|_| CaissonError::Message("invalid path component".to_string()))?;
        let rc = unsafe { libc::mkdirat(dir_fd.as_raw_fd(), c_comp.as_ptr(), 0o700) };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::EEXIST) {
                return Err(CaissonError::Io(err));
            }
        }
        let next = unsafe {
            libc::openat(
                dir_fd.as_raw_fd(),
                c_comp.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if next < 0 {
            return Err(CaissonError::Io(std::io::Error::last_os_error()));
        }
        dir_fd = unsafe { OwnedFd::from_raw_fd(next) };
    }

    let c_leaf = CString::new(leaf.as_bytes())
        .map_err(|_| CaissonError::Message("invalid shell profile filename".to_string()))?;
    let raw_fd = unsafe {
        libc::openat(
            dir_fd.as_raw_fd(),
            c_leaf.as_ptr(),
            libc::O_RDWR | libc::O_CREAT | libc::O_APPEND | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600u32,
        )
    };
    if raw_fd < 0 {
        return Err(CaissonError::Io(std::io::Error::last_os_error()));
    }
    let file_fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(file_fd.as_raw_fd(), &mut st) } != 0 {
        return Err(CaissonError::Io(std::io::Error::last_os_error()));
    }
    if (st.st_mode & libc::S_IFMT) != libc::S_IFREG {
        return Err(CaissonError::Message(format!(
            "refusing to edit non-regular shell profile '{}'",
            home.join(rel).display()
        )));
    }
    if (st.st_size as u64) > 4 * 1024 * 1024 {
        return Err(CaissonError::Message(format!(
            "shell profile '{}' exceeds 4 MiB safety limit",
            home.join(rel).display()
        )));
    }

    let mut file = fs::File::from(file_fd);
    let mut existing = String::new();
    file.seek(SeekFrom::Start(0))?;
    file.read_to_string(&mut existing)?;
    if existing.contains("caisson shell-init") {
        return Ok(false);
    }

    let mut block = String::new();
    if !existing.is_empty() && !existing.ends_with('\n') {
        block.push('\n');
    }
    block.push_str("\n# Caisson shell integration (enables `caisson wt cd` and `cai` alias)\n");
    block.push_str(shell.init_line());
    block.push('\n');

    file.write_all(block.as_bytes())?;
    file.sync_all()?;
    Ok(true)
}

fn verify_no_symlink_components(base: &Path, rel: &Path) -> Result<()> {
    let mut cur = base.to_path_buf();
    for comp in rel.components() {
        cur.push(comp);
        if let Ok(meta) = fs::symlink_metadata(&cur) {
            if meta.file_type().is_symlink() {
                return Err(CaissonError::Message(format!(
                    "refusing to traverse symlinked path component '{}'",
                    cur.display()
                )));
            }
        }
    }
    Ok(())
}

/// Resolve and verify the current executable before elevating with `/usr/bin/sudo`:
/// requires a canonical regular file whose binary and ancestor chain are owned by root or the
/// current effective UID, are never world-writable (unless sticky), and only allow group-write
/// for trusted macOS system/Homebrew groups (`wheel` = 0, `admin` = 80, or the current primary gid).
pub fn verify_elevatable_executable(path: &Path) -> Result<PathBuf> {
    use std::os::unix::fs::MetadataExt;

    let canonical = fs::canonicalize(path)?;
    let my_uid = unsafe { libc::geteuid() };
    let my_gid = unsafe { libc::getegid() };
    let is_trusted_group = |gid: u32| gid == 0 || gid == 80 || gid == my_gid;

    let meta = fs::symlink_metadata(&canonical)?;
    if !meta.is_file() {
        return Err(CaissonError::Message(format!(
            "current executable '{}' is not a regular file",
            canonical.display()
        )));
    }
    let bin_mode = meta.mode();
    let bin_world_w = (bin_mode & 0o002) != 0;
    let bin_untrusted_group_w = (bin_mode & 0o020) != 0 && !is_trusted_group(meta.gid());
    if (meta.uid() != 0 && meta.uid() != my_uid) || bin_world_w || bin_untrusted_group_w {
        return Err(CaissonError::Message(format!(
            "refusing to elevate writable or foreign-owned executable '{}' (uid={}, gid={}, mode={:o})",
            canonical.display(),
            meta.uid(),
            meta.gid(),
            bin_mode & 0o777
        )));
    }
    let mut anc = canonical.parent();
    while let Some(dir) = anc {
        if let Ok(dm) = fs::symlink_metadata(dir) {
            let mode = dm.mode();
            let sticky = (mode & libc::S_ISVTX as u32) != 0;
            let world_w = (mode & 0o002) != 0 && !sticky;
            let untrusted_group_w = (mode & 0o020) != 0 && !sticky && !is_trusted_group(dm.gid());
            if (dm.uid() != 0 && dm.uid() != my_uid) || world_w || untrusted_group_w {
                return Err(CaissonError::Message(format!(
                    "refusing to elevate executable under writable or foreign-owned directory '{}' (uid={}, gid={}, mode={:o})",
                    dir.display(),
                    dm.uid(),
                    dm.gid(),
                    mode & 0o777
                )));
            }
        }
        anc = dir.parent();
    }
    Ok(canonical)
}

fn resolve_self_exe() -> Result<PathBuf> {
    let exe = std::env::current_exe()?;
    verify_elevatable_executable(&exe)
}

fn discover_drive_choices(registered: &[VolumeStatusEntry]) -> Vec<(DriveChoice, SelectOption)> {
    let mut choices = Vec::new();
    let existing_names: Vec<String> = registered.iter().map(|v| v.name.clone()).collect();
    let default_mount_exists = Path::new(DEFAULT_TIER1_MOUNT).exists();

    // Offer existing healthy mounted volumes first so `caisson setup` is idempotent on re-runs.
    for vol in registered {
        if vol.mounted && vol.shields.all_healthy {
            let quota_str = vol
                .quota_bytes
                .map(format_bytes)
                .unwrap_or_else(|| "none".to_string());
            choices.push((
                DriveChoice::KeepExisting(vol.clone()),
                SelectOption {
                    label: format!(
                        "Keep existing '{}' ({}) [HEALTHY]",
                        vol.name, vol.mount_point
                    ),
                    detail: Some(format!(
                        "used {} · hard quota {} · container {}",
                        format_bytes(vol.used_bytes),
                        quota_str,
                        vol.container
                    )),
                    sub_detail: Some(
                        "Skip volume creation and verify daemon, shell, agents & T3".to_string(),
                    ),
                },
            ));
        }
    }

    let has_existing = !choices.is_empty();
    let create_prefix = if has_existing { "Create new on " } else { "" };

    if let Ok(apfs) = list_apfs() {
        // Internal container first (default on fresh install).
        if let Ok(internal) = detect_internal_container() {
            let ceiling = internal.capacity_ceiling.max(1);
            let used = ceiling.saturating_sub(internal.capacity_free);
            let frac = used as f64 / ceiling as f64;
            let pct = (frac * 100.0).round() as u64;
            let suggested_name =
                suggest_volume_name(&existing_names, default_mount_exists, Some("internal"));
            choices.push((
                DriveChoice::CreateOnContainer {
                    container_ref: internal.container_reference.clone(),
                    suggested_name,
                    capacity_ceiling: internal.capacity_ceiling,
                },
                SelectOption {
                    label: format!(
                        "{create_prefix}Internal Mac SSD ({})",
                        internal.container_reference
                    ),
                    detail: Some(format!(
                        "[{}] {}% used · {} free / {}",
                        bar_meter(frac, 18),
                        pct,
                        format_bytes(internal.capacity_free),
                        format_bytes(internal.capacity_ceiling)
                    )),
                    sub_detail: Some("Always attached, fastest internal NVMe".to_string()),
                },
            ));
        }

        // Physical writable external APFS containers mounted under `/Volumes/...`.
        // Explicitly filters out virtual disk images (`.dmg`, macOS cryptexes, Xcode
        // Simulator runtimes) and read-only media so only real user drives appear.
        for c in &apfs.containers {
            let Ok(c_info) = diskutil_info(&c.container_reference) else {
                continue;
            };
            if c_info.internal != Some(false)
                || c_info.is_disk_image()
                || c_info.writable_media == Some(false)
            {
                continue;
            }
            let primary_vol = c
                .volumes
                .iter()
                .find(|v| {
                    let vn = v.name.as_deref().unwrap_or("");
                    !existing_names.iter().any(|n| n.eq_ignore_ascii_case(vn))
                })
                .or_else(|| c.volumes.first());
            let Some(pv) = primary_vol else {
                continue;
            };
            let pv_name = pv.name.as_deref().unwrap_or("ssd");
            let live_mount = pv
                .mount_point
                .clone()
                .filter(|s| !s.is_empty())
                .or_else(|| {
                    diskutil_info(&pv.device_identifier)
                        .ok()
                        .and_then(|info| info.mount_point)
                        .filter(|s| !s.is_empty())
                });
            let Some(mp) = live_mount.filter(|m| m.starts_with("/Volumes/")) else {
                continue;
            };
            let drive_label = Path::new(&mp)
                .file_name()
                .and_then(|s| s.to_str())
                .filter(|s| !s.is_empty())
                .unwrap_or(if pv_name.is_empty() { "ssd" } else { pv_name });
            let ceiling = c.capacity_ceiling.max(1);
            let used = ceiling.saturating_sub(c.capacity_free);
            let frac = used as f64 / ceiling as f64;
            let pct = (frac * 100.0).round() as u64;
            let suggested_name =
                suggest_volume_name(&existing_names, default_mount_exists, Some(drive_label));
            choices.push((
                DriveChoice::CreateOnContainer {
                    container_ref: c.container_reference.clone(),
                    suggested_name,
                    capacity_ceiling: c.capacity_ceiling,
                },
                SelectOption {
                    label: format!(
                        "{create_prefix}External APFS SSD ({} -> {})",
                        sanitize_display_label(&c.container_reference),
                        sanitize_display_label(&mp)
                    ),
                    detail: Some(format!(
                        "[{}] {}% used · {} free / {}",
                        bar_meter(frac, 18),
                        pct,
                        format_bytes(c.capacity_free),
                        format_bytes(c.capacity_ceiling)
                    )),
                    sub_detail: Some(
                        "Offloads heavy worktrees; LaunchDaemon StartOnMount auto-remounts"
                            .to_string(),
                    ),
                },
            ));
        }
    }

    choices
}

/// Run the interactive unprivileged-first Style A inline VT220 `caisson setup` wizard.
pub fn run_setup(
    registry_path: &Path,
    config_path: &Path,
    opts: &SetupOptions,
) -> Result<SetupReport> {
    if is_root() {
        return Err(CaissonError::Message(
            "`caisson setup` must be run without `sudo` (it calls `sudo` only for APFS volume creation and LaunchDaemon installation, never for `$HOME` files).".to_string(),
        ));
    }
    if !is_interactive_tty() {
        return Err(CaissonError::Message(
            "`caisson setup` requires an interactive terminal. For scripted setups, run:\n  sudo caisson init\n  sudo caisson daemon install\n  caisson agent install --all".to_string(),
        ));
    }

    let pal = Palette::auto();
    let home = resolve_user_home();
    let mut cfg = CaissonConfig::load_from(config_path)?;
    let registered = list_registered_volumes(registry_path).unwrap_or_default();
    let mut planned_commands = Vec::new();

    println!(
        "{}",
        format_banner(
            "CAISSON",
            "APFS Workspace Setup",
            &format!(
                "v{}{}",
                env!("CARGO_PKG_VERSION"),
                if opts.dry_run { " · dry-run" } else { "" }
            ),
            &pal
        )
    );
    print_rail_line(
        &pal.dim("Unprivileged wizard: sudo is only invoked once for APFS + LaunchDaemon."),
        &pal,
    );

    // -------------------------------------------------------------------------
    // Step 1: Target APFS container & hard quota
    // -------------------------------------------------------------------------
    print_step_header(1, 5, "Target APFS Container & Hard Quota", &pal);
    let drive_choices = discover_drive_choices(&registered);
    if drive_choices.is_empty() {
        return Err(CaissonError::Message(
            "no eligible APFS containers detected via `diskutil apfs list`".to_string(),
        ));
    }
    let select_opts: Vec<SelectOption> = drive_choices.iter().map(|(_, s)| s.clone()).collect();
    let picked_idx = select_one(
        "Choose an existing Caisson volume or APFS container:",
        &select_opts,
        0,
        &pal,
    )?;
    let chosen_drive = drive_choices[picked_idx].0.clone();

    let (volume_name, mut volume_mount, needs_create, init_args, dry_create_opts) =
        match chosen_drive {
            DriveChoice::KeepExisting(vol) => (vol.name, vol.mount_point, false, Vec::new(), None),
            DriveChoice::CreateOnContainer {
                container_ref,
                suggested_name,
                capacity_ceiling,
            } => {
                let auto_q = compute_auto_quota(capacity_ceiling);
                let auto_q_label = auto_q
                    .map(format_bytes)
                    .unwrap_or_else(|| "none".to_string());
                let vol_name = if suggested_name == DEFAULT_TIER1_VOLUME_NAME {
                    print_rail_line(
                        &format!(
                            "Volume name : {} {}",
                            pal.ok(&suggested_name),
                            pal.dim(&format!("-> /Volumes/{suggested_name} (canonical default)"))
                        ),
                        &pal,
                    );
                    suggested_name
                } else {
                    loop {
                        let candidate = input_line(
                            "Volume name (canonical `/Volumes/caisson` already exists)",
                            &suggested_name,
                            &pal,
                        )?;
                        match validate_volume_name(&candidate) {
                            Ok(()) => break candidate,
                            Err(e) => {
                                print_rail_line(&format!("{} {e}", pal.warn("!")), &pal);
                            }
                        }
                    }
                };

                let quota_opts = vec![
                    SelectOption {
                        label: format!("auto ({auto_q_label})"),
                        detail: Some("Recommended Layer-A hard APFS ceiling".to_string()),
                        sub_detail: None,
                    },
                    SelectOption {
                        label: "500g".to_string(),
                        detail: Some("Fixed 500 GiB hard APFS quota".to_string()),
                        sub_detail: None,
                    },
                    SelectOption {
                        label: "none (unbounded)".to_string(),
                        detail: Some("Share full container free space dynamically".to_string()),
                        sub_detail: None,
                    },
                ];
                let q_idx = select_one("Hard APFS volume quota:", &quota_opts, 0, &pal)?;
                let mut args = vec![
                    "init".to_string(),
                    "--no-agents".to_string(),
                    "--container".to_string(),
                    container_ref.clone(),
                ];
                if vol_name != DEFAULT_TIER1_VOLUME_NAME {
                    args.push("--name".to_string());
                    args.push(vol_name.clone());
                }
                let quota_spec = match q_idx {
                    1 => {
                        args.push("--quota".to_string());
                        args.push("500g".to_string());
                        QuotaSpec::parse("500g")?
                    }
                    2 => {
                        args.push("--no-quota".to_string());
                        QuotaSpec::None
                    }
                    _ => QuotaSpec::Auto,
                };
                let preview_opts = CreateVolumeOptions {
                    name: vol_name.clone(),
                    tier: VolumeTier::Tier1,
                    container: Some(container_ref.clone()),
                    on_volume: None,
                    mount_point: None,
                    quota: quota_spec,
                    case_sensitive: false,
                    nobrowse: false,
                    dry_run: true,
                };
                let mount_path = format!("/Volumes/{vol_name}");
                (vol_name, mount_path, true, args, Some(preview_opts))
            }
        };
    print_step_done(
        &format!(
            "Target: {} ({})",
            volume_mount,
            if needs_create {
                "will create"
            } else {
                "already healthy"
            }
        ),
        &pal,
    );

    // -------------------------------------------------------------------------
    // Step 2: Privileged Seal & LaunchDaemon (single sudo window)
    // -------------------------------------------------------------------------
    print_step_header(2, 5, "Privileged Seal & Boot LaunchDaemon", &pal);
    let d_status = daemon_status(Path::new(LAUNCH_DAEMON_PLIST_PATH));
    let d_ver = inspect_daemon_binary_version(&d_status);
    let daemon_current = d_status.installed
        && d_status.loaded
        && d_ver.as_deref() == Some(env!("CARGO_PKG_VERSION"));
    let want_daemon = if opts.no_daemon || daemon_current {
        false
    } else {
        let prompt = if d_status.installed {
            format!(
                "Refresh LaunchDaemon binary (currently v{} -> v{})?",
                d_ver.as_deref().unwrap_or("unknown"),
                env!("CARGO_PKG_VERSION")
            )
        } else {
            "Install one-shot LaunchDaemon (keeps shields & external SSD mounts active across reboots)?"
                .to_string()
        };
        confirm(&prompt, true, &pal)?
    };

    let mut volume_created = false;
    let mut daemon_installed = daemon_current;
    let self_exe = resolve_self_exe()?;

    if needs_create || want_daemon {
        print_rail_line(
            &pal.dim("Privileged commands (single /usr/bin/sudo window; zero $HOME writes):"),
            &pal,
        );
        if needs_create {
            let cmd_str = format!("sudo caisson {}", init_args.join(" "));
            print_rail_line(&format!("  {}", pal.accent(&cmd_str)), &pal);
            planned_commands.push(cmd_str);
            if opts.dry_run {
                if let Some(ref preview_opts) = dry_create_opts {
                    if let Ok(rep) = create_volume(preview_opts, registry_path) {
                        print_rail_line(&format!("    {}", pal.dim(&rep.diskutil_command)), &pal);
                        planned_commands.push(rep.diskutil_command);
                        for step in &rep.shield_report.steps {
                            if let Some(ref c) = step.command {
                                print_rail_line(&format!("    {}", pal.dim(c)), &pal);
                                planned_commands.push(c.clone());
                            }
                        }
                    }
                }
            }
        }
        if want_daemon {
            let cmd_str = "sudo caisson daemon install".to_string();
            print_rail_line(&format!("  {}", pal.accent(&cmd_str)), &pal);
            planned_commands.push(cmd_str);
            if opts.dry_run {
                if let Ok(d_rep) = install_daemon(None, Path::new(LAUNCH_DAEMON_PLIST_PATH), true) {
                    for c in &d_rep.commands {
                        print_rail_line(&format!("    {}", pal.dim(c)), &pal);
                        planned_commands.push(c.clone());
                    }
                }
            }
        }

        if !opts.dry_run {
            if needs_create {
                let output = Command::new(SUDO_BIN)
                    .arg(&self_exe)
                    .arg("--json")
                    .args(&init_args)
                    .stdin(Stdio::inherit())
                    .stderr(Stdio::inherit())
                    .stdout(Stdio::piped())
                    .output()?;
                if !output.status.success() {
                    return Err(CaissonError::CommandFailed {
                        cmd: format!("sudo caisson {}", init_args.join(" ")),
                        code: output.status.code(),
                        stderr: "volume initialization failed".to_string(),
                    });
                }
                if let Ok(val) = serde_json::from_slice::<serde_json::Value>(&output.stdout) {
                    if let Some(actual_mp) = val["mount_point"].as_str() {
                        if !actual_mp.is_empty() {
                            volume_mount = actual_mp.to_string();
                        }
                    }
                }
                volume_created = true;
            }
            if want_daemon {
                let status = Command::new(SUDO_BIN)
                    .arg(&self_exe)
                    .args(["daemon", "install"])
                    .status()?;
                if !status.success() {
                    return Err(CaissonError::CommandFailed {
                        cmd: "sudo caisson daemon install".to_string(),
                        code: status.code(),
                        stderr: "LaunchDaemon installation failed".to_string(),
                    });
                }
                daemon_installed = true;
            }
        }
    }

    // Update `volume_root` unprivileged for BOTH selections (set custom root or clear back to default).
    let desired_root = if volume_mount == DEFAULT_TIER1_MOUNT {
        None
    } else {
        Some(volume_mount.clone())
    };
    if cfg.volume_root != desired_root {
        if !opts.dry_run {
            cfg.volume_root = desired_root.clone();
            cfg.save_to(config_path)?;
        }
        if let Some(ref r) = desired_root {
            planned_commands.push(format!("caisson config set volume_root {r}"));
        } else {
            planned_commands.push("caisson config set volume_root \"\"".to_string());
        }
    }

    print_step_done(
        &format!(
            "Volume {} · LaunchDaemon {}",
            if needs_create && opts.dry_run {
                "planned"
            } else {
                "ready"
            },
            if daemon_installed || (want_daemon && opts.dry_run) {
                "enabled"
            } else {
                "skipped"
            }
        ),
        &pal,
    );

    // -------------------------------------------------------------------------
    // Step 3: Shell integration & coding agents (unprivileged)
    // -------------------------------------------------------------------------
    print_step_header(
        3,
        5,
        "Shell Integration & Coding Agents (unprivileged)",
        &pal,
    );
    let mut shell_configured = None;
    if !opts.no_shell {
        let shell = DetectedShell::from_env();
        let rc_rel = shell.rc_relative_path();
        match shell_rc_has_caisson_init(&home, rc_rel) {
            Ok(true) => {
                print_rail_line(
                    &format!("{} Shell hook already present in ~/{rc_rel}", pal.ok("✓")),
                    &pal,
                );
                shell_configured = Some(format!("~/{rc_rel} (existing)"));
            }
            Ok(false) => {
                print_rail_line(&format!("Snippet: {}", pal.accent(shell.init_line())), &pal);
                if confirm(
                    &format!("Append shell integration to ~/{rc_rel}?"),
                    true,
                    &pal,
                )? {
                    append_shell_init_line(&home, shell, opts.dry_run)?;
                    shell_configured = Some(format!("~/{rc_rel}"));
                    planned_commands.push(format!(
                        "append shell-init ({}) -> ~/{rc_rel}",
                        shell.name()
                    ));
                }
            }
            Err(e) => {
                let rc_full = home.join(rc_rel);
                if fs::symlink_metadata(&rc_full)
                    .map(|m| m.file_type().is_symlink())
                    .unwrap_or(false)
                {
                    let already_present = fs::metadata(&rc_full)
                        .ok()
                        .filter(|m| m.is_file() && m.len() <= 4 * 1024 * 1024)
                        .and_then(|_| fs::read_to_string(&rc_full).ok())
                        .map(|s| s.contains("caisson shell-init"))
                        .unwrap_or(false);
                    if already_present {
                        print_rail_line(
                            &format!(
                                "{} Shell hook already present in ~/{rc_rel} (dotfiles symlink)",
                                pal.ok("✓")
                            ),
                            &pal,
                        );
                        shell_configured = Some(format!("~/{rc_rel} (existing)"));
                    } else {
                        print_rail_line(
                            &format!(
                                "{} ~/{rc_rel} is a symlink (dotfiles-managed); add this line manually:",
                                pal.warn("!")
                            ),
                            &pal,
                        );
                        print_rail_line(&format!("  {}", pal.accent(shell.init_line())), &pal);
                    }
                } else {
                    print_rail_line(&format!("{} Skipped ~/{rc_rel}: {e}", pal.warn("!")), &pal);
                }
            }
        }
    }

    let mut agents_installed = Vec::new();
    if !opts.no_agents {
        let inspected = inspect_agents(&home, &cfg);
        let unconfigured: Vec<_> = inspected
            .iter()
            .filter(|a| a.detected && !a.installed && !a.dismissed)
            .collect();
        if unconfigured.is_empty() {
            let existing: Vec<String> = inspected
                .iter()
                .filter(|a| a.installed)
                .map(|a| a.id.clone())
                .collect();
            if !existing.is_empty() {
                print_rail_line(
                    &format!(
                        "{} Coding agent skills already installed: {}",
                        pal.ok("✓"),
                        existing.join(", ")
                    ),
                    &pal,
                );
                agents_installed = existing;
            } else {
                print_rail_line(
                    &pal.dim("No unconfigured coding agents detected in $HOME."),
                    &pal,
                );
            }
        } else {
            let mut toggles: Vec<ToggleOption> = unconfigured
                .iter()
                .map(|a| ToggleOption {
                    label: format!("{} ({})", a.display_name, a.id),
                    detail: Some(a.skill_path.clone()),
                    checked: true,
                })
                .collect();
            multi_select(
                "Install official Caisson SKILL.md for detected coding agents:",
                &mut toggles,
                &pal,
            )?;
            let chosen: Vec<String> = unconfigured
                .iter()
                .zip(toggles.iter())
                .filter(|(_, t)| t.checked)
                .map(|(a, _)| a.id.clone())
                .collect();
            let dismissed: Vec<String> = unconfigured
                .iter()
                .zip(toggles.iter())
                .filter(|(_, t)| !t.checked)
                .map(|(a, _)| a.id.clone())
                .collect();
            if !chosen.is_empty() {
                install_agent_skills(&home, &mut cfg, &chosen, false, opts.dry_run)?;
                agents_installed = chosen.clone();
                planned_commands.push(format!("caisson agent install {}", chosen.join(" ")));
            }
            if !dismissed.is_empty() && !opts.dry_run {
                let _ = dismiss_agents(&home, &mut cfg, &dismissed);
            }
            if !opts.dry_run {
                cfg.save_to(config_path)?;
            }
        }
    }
    print_step_done(
        &format!(
            "Shell: {} · Agents: {}",
            shell_configured.as_deref().unwrap_or("skipped"),
            if agents_installed.is_empty() {
                "none".to_string()
            } else {
                agents_installed.join(", ")
            }
        ),
        &pal,
    );

    // -------------------------------------------------------------------------
    // Step 4: Optional T3 Code worktrees & live syspolicyd exec probe (unprivileged)
    // -------------------------------------------------------------------------
    let check_t3 = !opts.no_t3 && is_t3_installed(&home);
    let step4_title = match (check_t3, !opts.no_probe) {
        (true, true) => "T3 Code Worktrees & Live syspolicyd Exec Probe",
        (true, false) => "T3 Code Worktrees",
        (false, true) => "Live syspolicyd Exec Probe",
        (false, false) => "Verification (skipped)",
    };
    print_step_header(4, 5, step4_title, &pal);
    let mut t3_configured = None;
    if check_t3 {
        let already_on_vol = resolve_t3_worktrees_dir(&home)
            .and_then(|d| statfs_path(&d).ok())
            .map(|st| st.mount_point == volume_mount)
            .unwrap_or(false);
        if already_on_vol {
            let target = format!("{volume_mount}/t3-worktrees");
            print_rail_line(
                &format!(
                    "{} T3 Code worktrees already shielded on {volume_mount}",
                    pal.ok("✓")
                ),
                &pal,
            );
            t3_configured = Some(target);
        } else {
            let desired = format!("{volume_mount}/t3-worktrees");
            if confirm(
                &format!("Point T3 Code worktrees to `{desired}`?"),
                true,
                &pal,
            )? {
                if opts.dry_run {
                    planned_commands.push(format!("caisson t3 setup --volume {volume_name} --yes"));
                    t3_configured = Some(format!("{desired} (dry-run)"));
                } else {
                    match run_t3_setup(
                        &home,
                        registry_path,
                        &T3SetupOptions {
                            volume: Some(volume_name.clone()),
                            path: None,
                            dry_run: false,
                            yes: true,
                        },
                    ) {
                        Ok(rep) => {
                            t3_configured = Some(rep.target_worktrees_directory);
                        }
                        Err(e) => {
                            print_rail_line(
                                &format!("{} T3 setup skipped: {e}", pal.warn("!")),
                                &pal,
                            );
                        }
                    }
                }
            }
        }
    }

    let syspolicyd_probe = if opts.no_probe {
        None
    } else {
        print_rail_line(
            &pal.dim("Running live 2-binary syspolicyd exec probe (~150 ms)..."),
            &pal,
        );
        probe_syspolicyd_exec_latency()
    };
    if let Some(ref p) = syspolicyd_probe {
        let max_ms = p.first_exec_ms.max(p.cached_exec_ms).max(1.0);
        let first_bar = bar_meter(p.first_exec_ms / max_ms, 18);
        let cached_bar = bar_meter(p.cached_exec_ms / max_ms, 18);
        if p.stalled {
            print_rail_line(
                &format!(
                    "1st exec [{}] {} {}",
                    pal.warn(&first_bar),
                    pal.warn(&format!("{:.1} ms", p.first_exec_ms)),
                    pal.warn(&format!("[STALLED +{:.1} ms]", p.overhead_ms))
                ),
                &pal,
            );
            print_rail_line(
                &format!(
                    "Cached   [{}] {}",
                    pal.ok(&cached_bar),
                    pal.ok(&format!("{:.1} ms", p.cached_exec_ms))
                ),
                &pal,
            );
            if let Some(ref proc_name) = p.responsible_process {
                print_rail_line(
                    &format!(
                        "{} Ancestor `{}` is not Developer Tools exempt.",
                        pal.warn("!"),
                        proc_name
                    ),
                    &pal,
                );
            }
            let over_ssh = std::env::var_os("SSH_CONNECTION").is_some()
                || std::env::var_os("SSH_TTY").is_some()
                || p.responsible_process
                    .as_deref()
                    .map(|s| s.starts_with("sshd"))
                    .unwrap_or(false);
            if over_ssh {
                print_rail_line(
                    &pal.dim("Running over SSH; enable Developer Tools in System Settings on the Mac desktop if needed."),
                    &pal,
                );
            } else if !opts.dry_run
                && confirm(
                    "Open System Settings -> Privacy & Security -> Developer Tools now?",
                    false,
                    &pal,
                )?
            {
                let _ = Command::new("open").arg(DEV_TOOLS_SETTINGS_URL).status();
            }
        } else {
            print_rail_line(
                &format!(
                    "{} Fresh binary exec fast ({:.1} ms 1st vs {:.1} ms cached; Developer Tools active)",
                    pal.ok("✓"),
                    p.first_exec_ms,
                    p.cached_exec_ms
                ),
                &pal,
            );
        }
    }
    let step4_done = match (check_t3, !opts.no_probe) {
        (true, true) => "T3 & exec probe complete",
        (true, false) => "T3 check complete",
        (false, true) => "Exec probe complete",
        (false, false) => "Skipped",
    };
    print_step_done(step4_done, &pal);

    // -------------------------------------------------------------------------
    // Step 5: Sealed-Chamber Summary Card
    // -------------------------------------------------------------------------
    println!();
    println!(
        "  {}  {}",
        pal.ok("✓"),
        pal.bold(if opts.dry_run {
            "CAISSON SETUP DRY-RUN COMPLETE"
        } else {
            "CAISSON CHAMBER SEALED & READY"
        })
    );
    println!("  ┌──────────────────────────────────────────────────────────────");
    let vol_badge = if needs_create && opts.dry_run {
        pal.accent("● PLANNED")
    } else {
        pal.ok("● READY  ")
    };
    println!("  │  Volume       {vol_badge}  {volume_mount}");
    let (daemon_badge, daemon_desc) = if daemon_installed {
        (
            pal.ok("● LOADED "),
            "dev.caisson.mount (boot + StartOnMount)",
        )
    } else if want_daemon && opts.dry_run {
        (
            pal.accent("● PLANNED"),
            "sudo caisson daemon install (boot + StartOnMount)",
        )
    } else {
        (
            pal.dim("○ SKIPPED"),
            "run `sudo caisson daemon install` later",
        )
    };
    println!("  │  LaunchDaemon {daemon_badge}  {daemon_desc}");
    println!(
        "  │  Shell Hook   {}  {}",
        if shell_configured.is_some() {
            if opts.dry_run
                && !shell_configured
                    .as_deref()
                    .unwrap_or("")
                    .contains("existing")
            {
                pal.accent("● PLANNED")
            } else {
                pal.ok("● ACTIVE ")
            }
        } else {
            pal.dim("○ SKIPPED")
        },
        shell_configured
            .as_deref()
            .unwrap_or("run `caisson shell-init zsh`")
    );
    println!(
        "  │  Agent Skills {}  {}",
        if !agents_installed.is_empty() {
            pal.ok("● SYNCED ")
        } else {
            pal.dim("○ NONE   ")
        },
        if agents_installed.is_empty() {
            "run `caisson agent install` anytime".to_string()
        } else {
            agents_installed.join(", ")
        }
    );
    if let Some(ref t3) = t3_configured {
        let t3_badge = if opts.dry_run && t3.contains("dry-run") {
            pal.accent("● PLANNED ")
        } else {
            pal.ok("● SHIELDED")
        };
        println!("  │  T3 Worktrees {t3_badge} {t3}");
    }
    if let Some(ref p) = syspolicyd_probe {
        let badge = if p.stalled {
            pal.warn("▲ STALLED")
        } else {
            pal.ok("● FAST   ")
        };
        println!(
            "  │  Exec Probe   {}  {:.1} ms 1st vs {:.1} ms cached",
            badge, p.first_exec_ms, p.cached_exec_ms
        );
    }
    println!("  └──────────────────────────────────────────────────────────────");
    println!(
        "  {}  {}  ·  {}  ·  {}",
        pal.dim("Try it now:"),
        pal.accent("caisson status"),
        pal.accent("caisson bench"),
        pal.accent("caisson wt add feat/demo")
    );

    Ok(SetupReport {
        dry_run: opts.dry_run,
        volume_name,
        volume_mount,
        volume_created,
        daemon_installed,
        shell_configured,
        agents_installed,
        t3_configured,
        syspolicyd_probe,
        planned_commands,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use tempfile::tempdir;

    #[test]
    fn suggests_canonical_caisson_name_when_free_and_suffix_on_collision() {
        assert_eq!(suggest_volume_name(&[], false, Some("work-ssd")), "caisson");
        assert_eq!(
            suggest_volume_name(&["caisson".to_string()], true, Some("Work SSD")),
            "caisson-work-ssd"
        );
        assert_eq!(
            suggest_volume_name(
                &["caisson".to_string(), "caisson-work-ssd".to_string()],
                true,
                Some("Work SSD")
            ),
            "caisson-work-ssd-2"
        );
    }

    #[test]
    fn shell_rc_append_is_idempotent_and_rejects_symlinks() {
        let dir = tempdir().unwrap();
        let home = dir.path();

        assert!(!shell_rc_has_caisson_init(home, ".zshrc").unwrap());
        assert!(append_shell_init_line(home, DetectedShell::Zsh, true).unwrap());
        assert!(!home.join(".zshrc").exists());

        assert!(append_shell_init_line(home, DetectedShell::Zsh, false).unwrap());
        assert!(shell_rc_has_caisson_init(home, ".zshrc").unwrap());
        // Second call is a no-op (no duplicate lines).
        assert!(!append_shell_init_line(home, DetectedShell::Zsh, false).unwrap());
        let txt = fs::read_to_string(home.join(".zshrc")).unwrap();
        assert_eq!(txt.matches("caisson shell-init").count(), 1);

        // Symlinked `.bashrc` must be rejected.
        let outside = tempdir().unwrap();
        let victim = outside.path().join("victim");
        fs::write(&victim, "untouched\n").unwrap();
        symlink(&victim, home.join(".bashrc")).unwrap();
        assert!(append_shell_init_line(home, DetectedShell::Bash, false).is_err());
        assert_eq!(fs::read_to_string(&victim).unwrap(), "untouched\n");
    }

    #[test]
    fn verify_elevatable_executable_allows_homebrew_775_and_rejects_world_writable() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        // Simulate Homebrew `0775` directory owned by current user/group.
        let cellar = dir.path().join("Cellar");
        fs::create_dir(&cellar).unwrap();
        fs::set_permissions(&cellar, fs::Permissions::from_mode(0o775)).unwrap();
        let bin = cellar.join("caisson");
        fs::write(&bin, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(verify_elevatable_executable(&bin).is_ok());

        // World-writable non-sticky parent directory must be rejected.
        let world_dir = dir.path().join("world");
        fs::create_dir(&world_dir).unwrap();
        fs::set_permissions(&world_dir, fs::Permissions::from_mode(0o777)).unwrap();
        let bad_bin = world_dir.join("caisson");
        fs::write(&bad_bin, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&bad_bin, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(verify_elevatable_executable(&bad_bin).is_err());

        // If Homebrew caisson is installed on this Mac, verify it passes cleanly.
        let brew_bin = Path::new("/opt/homebrew/bin/caisson");
        if brew_bin.exists() {
            assert!(verify_elevatable_executable(brew_bin).is_ok());
        }
    }
}
