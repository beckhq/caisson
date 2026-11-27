use crate::config::CaissonConfig;
use crate::error::{CaissonError, Result};
use serde::Serialize;
use std::ffi::{CStr, CString};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Embedded canonical `SKILL.md` so `caisson agent install` works completely offline.
pub const CAISSON_SKILL_MD: &str = include_str!("../skills/caisson/SKILL.md");

/// Crisp drop-in snippet for repo-level `AGENTS.md` or `CLAUDE.md` files.
pub const AGENTS_MD_SNIPPET: &str = r#"## macOS Worktrees (Caisson)
- When you would otherwise run `git worktree add` yourself on macOS (and the host does not manage worktrees for you), prefer `caisson wt add <branch> --json`, `caisson wt ls --json`, and `caisson wt rm <slug>`.
- If the host manages worktrees (T3 Code `workspaceStrategy`, Claude Code worktree isolation, or an IDE), let the host create them and run `caisson wt hydrate --json` before `pnpm`/`npm`/`uv` install when `node_modules` or `.venv` is missing.
- Worktrees live under `/Volumes/caisson/worktrees/...`; in Claude Code, allow edits there with `--add-dir /Volumes/caisson` or `additionalDirectories`.
- Never run privileged `sudo caisson ...` commands (`init`, `vol`, `daemon`) from an agent turn; fall back to `git worktree` if `caisson` is missing or unmounted.
"#;

/// Deterministic 64-bit FNV-1a hex digest for detecting skill updates vs user customizations.
pub fn skill_content_hash(content: &str) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in content.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

#[derive(Debug, Clone, Copy)]
struct AgentSpec {
    id: &'static str,
    aliases: &'static [&'static str],
    display_name: &'static str,
    detect_rel: &'static str,
    home_bins: &'static [&'static str],
    path_bins: &'static [&'static str],
    app_bundles: &'static [&'static str],
    skill_rel: &'static str,
}

const KNOWN_AGENTS: &[AgentSpec] = &[
    AgentSpec {
        id: "claude",
        aliases: &["claude-code"],
        display_name: "Claude Code",
        detect_rel: ".claude",
        home_bins: &[".local/bin/claude"],
        path_bins: &["claude"],
        app_bundles: &[],
        skill_rel: ".claude/skills/caisson/SKILL.md",
    },
    AgentSpec {
        id: "codex",
        aliases: &[],
        display_name: "OpenAI Codex",
        detect_rel: ".codex",
        home_bins: &[".local/bin/codex"],
        path_bins: &["codex"],
        app_bundles: &[],
        skill_rel: ".codex/skills/caisson/SKILL.md",
    },
    AgentSpec {
        id: "antigravity",
        aliases: &["agy"],
        display_name: "Antigravity (agy)",
        detect_rel: ".gemini/config",
        home_bins: &[".local/bin/agy"],
        path_bins: &["agy"],
        app_bundles: &["/Applications/Antigravity.app"],
        skill_rel: ".gemini/config/skills/caisson/SKILL.md",
    },
    AgentSpec {
        id: "cursor",
        aliases: &[],
        display_name: "Cursor",
        detect_rel: ".cursor",
        home_bins: &[],
        path_bins: &[],
        app_bundles: &["/Applications/Cursor.app"],
        skill_rel: ".cursor/skills/caisson/SKILL.md",
    },
    AgentSpec {
        id: "opencode",
        aliases: &[],
        display_name: "OpenCode",
        detect_rel: ".config/opencode",
        home_bins: &[".local/bin/opencode"],
        path_bins: &["opencode"],
        app_bundles: &[],
        skill_rel: ".config/opencode/skills/caisson/SKILL.md",
    },
    AgentSpec {
        id: "windsurf",
        aliases: &[],
        display_name: "Windsurf",
        detect_rel: ".codeium/windsurf",
        home_bins: &[],
        path_bins: &[],
        app_bundles: &["/Applications/Windsurf.app"],
        skill_rel: ".codeium/windsurf/skills/caisson/SKILL.md",
    },
    AgentSpec {
        id: "agents",
        aliases: &[],
        display_name: "Open Agent Skills (~/.agents)",
        detect_rel: ".agents",
        home_bins: &[],
        path_bins: &[],
        app_bundles: &[],
        skill_rel: ".agents/skills/caisson/SKILL.md",
    },
];

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AgentHarnessStatus {
    pub id: String,
    pub display_name: String,
    pub detected: bool,
    pub installed: bool,
    pub up_to_date: bool,
    pub customized: bool,
    pub dismissed: bool,
    pub detect_path: String,
    pub skill_path: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentActionReport {
    pub action: String,
    pub dry_run: bool,
    pub targets: Vec<AgentHarnessStatus>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentSyncReport {
    pub dry_run: bool,
    pub version: String,
    pub updated: Vec<AgentHarnessStatus>,
    pub skipped_customized: Vec<AgentHarnessStatus>,
    pub already_up_to_date: Vec<String>,
}

/// Resolve the developer's real home directory, even when invoked under `sudo` (`SUDO_USER`).
pub fn resolve_user_home() -> PathBuf {
    if let Ok(override_home) = std::env::var("CAISSON_HOME") {
        if !override_home.trim().is_empty() {
            return PathBuf::from(override_home);
        }
    }
    if let Ok(sudo_user) = std::env::var("SUDO_USER") {
        let trimmed = sudo_user.trim();
        if !trimmed.is_empty() && trimmed != "root" {
            if let Some(pw_home) = lookup_user_home(trimmed) {
                return pw_home;
            }
        }
    }
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string()))
}

fn lookup_user_home(username: &str) -> Option<PathBuf> {
    let c_user = CString::new(username).ok()?;
    let pw = unsafe { libc::getpwnam(c_user.as_ptr()) };
    if pw.is_null() {
        return None;
    }
    let dir_ptr = unsafe { (*pw).pw_dir };
    if dir_ptr.is_null() {
        return None;
    }
    let dir_str = unsafe { CStr::from_ptr(dir_ptr) }.to_str().ok()?;
    Some(PathBuf::from(dir_str))
}

fn find_spec(id: &str) -> Result<&'static AgentSpec> {
    let lower = id.trim().to_ascii_lowercase();
    KNOWN_AGENTS
        .iter()
        .find(|s| s.id == lower || s.aliases.iter().any(|a| *a == lower))
        .ok_or_else(|| {
            let valid = KNOWN_AGENTS
                .iter()
                .map(|s| s.id)
                .collect::<Vec<_>>()
                .join(", ");
            CaissonError::Message(format!(
                "unknown agent harness '{id}'; supported values: {valid} (aliases: agy, claude-code)"
            ))
        })
}

fn find_in_path(bin_name: &str) -> bool {
    let Ok(path_var) = std::env::var("PATH") else {
        return false;
    };
    std::env::split_paths(&path_var).any(|dir| dir.join(bin_name).is_file())
}

fn detect_agent(home: &Path, spec: &AgentSpec) -> bool {
    if home.join(spec.detect_rel).exists() {
        return true;
    }
    if spec.home_bins.iter().any(|rel| home.join(rel).is_file()) {
        return true;
    }
    // Only probe global PATH and /Applications when operating against the real user home
    // (never when `CAISSON_HOME` is overridden in isolated tests).
    let isolated_home = std::env::var("CAISSON_HOME")
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false)
        || cfg!(test);
    if !isolated_home {
        if spec.path_bins.iter().any(|bin| find_in_path(bin)) {
            return true;
        }
        if spec.app_bundles.iter().any(|app| Path::new(app).exists()) {
            return true;
        }
    }
    false
}

/// Exact FNV-1a hashes of previously shipped Caisson `SKILL.md` releases (`v0.1.1`-`v0.1.5`)
/// so legacy installs without `agents.skill_hashes` auto-migrate only when unmodified.
const KNOWN_LEGACY_SKILL_HASHES: &[&str] = &[
    "a2c66fc513635f0b", // v0.1.1 - v0.1.2
    "e39cf0efa466113c", // v0.1.3
    "76fa4582e6b7cfb9", // v0.1.4 - v0.1.5
];

/// Ensure no existing path component from `home` down through `rel` is a symlink.
fn ensure_no_symlink_components_under(home: &Path, rel: &str) -> Result<()> {
    if let Ok(meta) = fs::symlink_metadata(home) {
        if meta.file_type().is_symlink() || !meta.is_dir() {
            return Err(CaissonError::Message(format!(
                "refusing to operate through symlinked home directory '{}'",
                home.display()
            )));
        }
    }
    let mut cur = home.to_path_buf();
    for comp in Path::new(rel).components() {
        cur.push(comp);
        if let Ok(meta) = fs::symlink_metadata(&cur) {
            if meta.file_type().is_symlink() {
                return Err(CaissonError::Message(format!(
                    "refusing to follow symlink component '{}'",
                    cur.display()
                )));
            }
        }
    }
    Ok(())
}

/// Atomically write `content` to `dest` without following symlinks at `dest`.
fn atomic_write_skill_file(dest: &Path, content: &str) -> Result<()> {
    let parent = dest
        .parent()
        .ok_or_else(|| CaissonError::Message(format!("invalid skill path '{}'", dest.display())))?;
    let tmp_name = format!(".SKILL.md.caisson.{}.tmp", uuid::Uuid::new_v4());
    let tmp_path = parent.join(tmp_name);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp_path)?;
    file.write_all(content.as_bytes())?;
    file.sync_all()?;
    drop(file);
    if let Err(e) = fs::rename(&tmp_path, dest) {
        let _ = fs::remove_file(&tmp_path);
        return Err(CaissonError::Io(e));
    }
    Ok(())
}

/// Determine `(up_to_date, customized)` for an existing on-disk `SKILL.md`.
fn classify_installed_skill(
    spec_id: &str,
    skill_path: &Path,
    config: &CaissonConfig,
) -> (bool, bool) {
    let Ok(meta) = fs::symlink_metadata(skill_path) else {
        return (false, true);
    };
    if meta.file_type().is_symlink() || !meta.is_file() {
        return (false, true);
    }
    let Ok(disk_content) = fs::read_to_string(skill_path) else {
        // Never overwrite unreadable or non-UTF-8 files automatically.
        return (false, true);
    };
    let current_hash = skill_content_hash(CAISSON_SKILL_MD);
    let disk_hash = skill_content_hash(&disk_content);
    if disk_hash == current_hash {
        return (true, false);
    }

    // If Caisson previously recorded the hash it wrote for this agent, only treat the file
    // as customized when the user modified it away from that recorded hash.
    if let Some(recorded_hash) = config.agents.skill_hashes.get(spec_id) {
        return (false, disk_hash != *recorded_hash);
    }

    // Backwards-compatible migration for v0.1.1-v0.1.4 installs before `skill_hashes` existed:
    // only auto-migrate if the on-disk file matches an exact shipped Caisson SKILL.md hash.
    if KNOWN_LEGACY_SKILL_HASHES.contains(&disk_hash.as_str()) {
        (false, false)
    } else {
        (false, true)
    }
}

/// Inspect detection, skill installation, freshness, and dismissal state for all known coding agents.
pub fn inspect_agents(home: &Path, config: &CaissonConfig) -> Vec<AgentHarnessStatus> {
    KNOWN_AGENTS
        .iter()
        .map(|spec| {
            let detect_path = home.join(spec.detect_rel);
            let skill_path = home.join(spec.skill_rel);
            let detected = detect_agent(home, spec);
            let installed = skill_path.is_file();
            let (up_to_date, customized) = if installed {
                classify_installed_skill(spec.id, &skill_path, config)
            } else {
                (false, false)
            };
            let dismissed = config
                .agents
                .dismissed
                .iter()
                .any(|d| d.eq_ignore_ascii_case(spec.id));
            AgentHarnessStatus {
                id: spec.id.to_string(),
                display_name: spec.display_name.to_string(),
                detected,
                installed,
                up_to_date,
                customized,
                dismissed,
                detect_path: detect_path.display().to_string(),
                skill_path: skill_path.display().to_string(),
            }
        })
        .collect()
}

/// Install `SKILL.md` for the requested agents (or all detected agents if `targets` is empty).
/// Refuses to write into `$HOME` when running as root (`sudo`) to avoid privileged symlink risks.
pub fn install_agent_skills(
    home: &Path,
    config: &mut CaissonConfig,
    targets: &[String],
    all_detected: bool,
    dry_run: bool,
) -> Result<AgentActionReport> {
    if !dry_run && crate::apfs::statfs::is_root() {
        return Err(CaissonError::Message(
            "refusing to write agent skills into `$HOME` as root; run `caisson agent install` without `sudo`".to_string(),
        ));
    }

    let specs: Vec<&AgentSpec> = if !targets.is_empty() {
        let mut list = Vec::with_capacity(targets.len());
        for t in targets {
            list.push(find_spec(t)?);
        }
        list
    } else if all_detected {
        KNOWN_AGENTS
            .iter()
            .filter(|s| detect_agent(home, s))
            .collect()
    } else {
        KNOWN_AGENTS
            .iter()
            .filter(|s| {
                detect_agent(home, s)
                    && !config
                        .agents
                        .dismissed
                        .iter()
                        .any(|d| d.eq_ignore_ascii_case(s.id))
            })
            .collect()
    };

    let current_hash = skill_content_hash(CAISSON_SKILL_MD);
    let mut affected = Vec::new();
    for spec in specs {
        let skill_path = home.join(spec.skill_rel);
        ensure_no_symlink_components_under(home, spec.skill_rel)?;
        if !dry_run {
            if let Some(parent) = skill_path.parent() {
                fs::create_dir_all(parent)?;
                ensure_no_symlink_components_under(home, spec.skill_rel)?;
                atomic_write_skill_file(&skill_path, CAISSON_SKILL_MD)?;
            }
            if !config
                .agents
                .installed
                .iter()
                .any(|x| x.eq_ignore_ascii_case(spec.id))
            {
                config.agents.installed.push(spec.id.to_string());
            }
            config
                .agents
                .skill_hashes
                .insert(spec.id.to_string(), current_hash.clone());
            config
                .agents
                .dismissed
                .retain(|x| !x.eq_ignore_ascii_case(spec.id));
        }

        let (up_to_date, customized) = if dry_run && skill_path.is_file() {
            classify_installed_skill(spec.id, &skill_path, config)
        } else {
            (!dry_run, false)
        };

        affected.push(AgentHarnessStatus {
            id: spec.id.to_string(),
            display_name: spec.display_name.to_string(),
            detected: detect_agent(home, spec),
            installed: if dry_run { skill_path.is_file() } else { true },
            up_to_date,
            customized,
            dismissed: false,
            detect_path: home.join(spec.detect_rel).display().to_string(),
            skill_path: skill_path.display().to_string(),
        });
    }

    Ok(AgentActionReport {
        action: "install".to_string(),
        dry_run,
        targets: affected,
    })
}

/// Remove `SKILL.md` for the specified agent targets (or all installed if `all` is true).
pub fn uninstall_agent_skills(
    home: &Path,
    config: &mut CaissonConfig,
    targets: &[String],
    all: bool,
    dry_run: bool,
) -> Result<AgentActionReport> {
    if !dry_run && crate::apfs::statfs::is_root() {
        return Err(CaissonError::Message(
            "refusing to modify `$HOME` agent skills as root; run `caisson agent uninstall` without `sudo`".to_string(),
        ));
    }

    let specs: Vec<&AgentSpec> = if all {
        KNOWN_AGENTS
            .iter()
            .filter(|s| home.join(s.skill_rel).is_file())
            .collect()
    } else if targets.is_empty() {
        return Err(CaissonError::Message(
            "specify one or more agent IDs or pass `--all`".to_string(),
        ));
    } else {
        let mut list = Vec::with_capacity(targets.len());
        for t in targets {
            list.push(find_spec(t)?);
        }
        list
    };

    let mut affected = Vec::new();
    for spec in specs {
        let skill_path = home.join(spec.skill_rel);
        ensure_no_symlink_components_under(home, spec.skill_rel)?;
        if !dry_run {
            if fs::symlink_metadata(&skill_path).is_ok() {
                fs::remove_file(&skill_path)?;
            }
            if let Some(parent) = skill_path.parent() {
                let _ = fs::remove_dir(parent);
            }
            config
                .agents
                .installed
                .retain(|x| !x.eq_ignore_ascii_case(spec.id));
            config.agents.skill_hashes.remove(spec.id);
        }

        affected.push(AgentHarnessStatus {
            id: spec.id.to_string(),
            display_name: spec.display_name.to_string(),
            detected: home.join(spec.detect_rel).exists(),
            installed: if dry_run { skill_path.is_file() } else { false },
            up_to_date: false,
            customized: false,
            dismissed: config
                .agents
                .dismissed
                .iter()
                .any(|x| x.eq_ignore_ascii_case(spec.id)),
            detect_path: home.join(spec.detect_rel).display().to_string(),
            skill_path: skill_path.display().to_string(),
        });
    }

    Ok(AgentActionReport {
        action: "uninstall".to_string(),
        dry_run,
        targets: affected,
    })
}

/// Silence passive new-agent discovery tips for the specified agent IDs.
pub fn dismiss_agents(
    home: &Path,
    config: &mut CaissonConfig,
    targets: &[String],
) -> Result<AgentActionReport> {
    if targets.is_empty() {
        return Err(CaissonError::Message(
            "specify one or more agent IDs to dismiss (e.g. `caisson agent dismiss codex`)"
                .to_string(),
        ));
    }
    let mut affected = Vec::new();
    for t in targets {
        let spec = find_spec(t)?;
        if !config
            .agents
            .dismissed
            .iter()
            .any(|x| x.eq_ignore_ascii_case(spec.id))
        {
            config.agents.dismissed.push(spec.id.to_string());
        }
        let skill_path = home.join(spec.skill_rel);
        let installed = skill_path.is_file();
        let (up_to_date, customized) = if installed {
            classify_installed_skill(spec.id, &skill_path, config)
        } else {
            (false, false)
        };
        affected.push(AgentHarnessStatus {
            id: spec.id.to_string(),
            display_name: spec.display_name.to_string(),
            detected: home.join(spec.detect_rel).exists(),
            installed,
            up_to_date,
            customized,
            dismissed: true,
            detect_path: home.join(spec.detect_rel).display().to_string(),
            skill_path: skill_path.display().to_string(),
        });
    }
    Ok(AgentActionReport {
        action: "dismiss".to_string(),
        dry_run: false,
        targets: affected,
    })
}

/// Return detected agents that do not yet have the Caisson skill installed and have not been dismissed.
pub fn find_unconfigured_agents(home: &Path, config: &CaissonConfig) -> Vec<AgentHarnessStatus> {
    inspect_agents(home, config)
        .into_iter()
        .filter(|a| a.detected && !a.installed && !a.dismissed)
        .collect()
}

/// Parse an interactive opt-out selection line.
/// - Empty line or `y`/`yes` -> select all `detected_ids`.
/// - `n`/`no`/`none` -> select nothing.
/// - Comma/space-separated tokens:
///   - If any token starts with `-` (e.g. `-cursor`), start with all `detected_ids` and remove excluded IDs.
///   - Otherwise treat tokens as an explicit allow-list.
pub fn parse_opt_out_selection(input: &str, detected_ids: &[String]) -> Result<Vec<String>> {
    let trimmed = input.trim();
    if trimmed.is_empty()
        || trimmed.eq_ignore_ascii_case("y")
        || trimmed.eq_ignore_ascii_case("yes")
    {
        return Ok(detected_ids.to_vec());
    }
    if trimmed.eq_ignore_ascii_case("n")
        || trimmed.eq_ignore_ascii_case("no")
        || trimmed.eq_ignore_ascii_case("none")
    {
        return Ok(Vec::new());
    }

    let tokens: Vec<&str> = trimmed
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|t| !t.is_empty())
        .collect();

    if tokens.iter().any(|t| t.starts_with('-')) {
        let mut excluded = Vec::new();
        for tok in tokens {
            let id = tok.trim_start_matches('-');
            let spec = find_spec(id)?;
            excluded.push(spec.id);
        }
        Ok(detected_ids
            .iter()
            .filter(|id| !excluded.contains(&id.as_str()))
            .cloned()
            .collect())
    } else {
        let mut selected = Vec::new();
        for tok in tokens {
            let spec = find_spec(tok)?;
            if !selected.iter().any(|x: &String| x == spec.id) {
                selected.push(spec.id.to_string());
            }
        }
        Ok(selected)
    }
}

/// Interactive agent skill opt-out prompt run at the end of `caisson init` (TTY only).
pub fn prompt_interactive_agent_setup(
    home: &Path,
    config_path: &Path,
    config: &mut CaissonConfig,
) -> Result<Option<AgentActionReport>> {
    let is_tty =
        unsafe { libc::isatty(libc::STDIN_FILENO) != 0 && libc::isatty(libc::STDOUT_FILENO) != 0 };
    if !is_tty {
        return Ok(None);
    }

    let pending = find_unconfigured_agents(home, config);
    if pending.is_empty() {
        return Ok(None);
    }

    // Never write into `$HOME` when running as root (e.g. `sudo caisson init`).
    if crate::apfs::statfs::is_root() {
        let ids: Vec<&str> = pending.iter().map(|a| a.id.as_str()).collect();
        println!(
            "\nDetected coding agents without the Caisson skill: {}.",
            ids.join(", ")
        );
        println!("Run `caisson agent install` (without sudo) to install the skill.");
        return Ok(None);
    }

    let ids: Vec<String> = pending.iter().map(|a| a.id.clone()).collect();
    println!("\nDetected coding agents without the Caisson skill:");
    for a in &pending {
        println!("  - {} ({}) -> {}", a.id, a.display_name, a.skill_path);
    }
    print!(
        "Install Caisson skill for [{}]? [Y/n, or opt out like `-cursor`]: ",
        ids.join(", ")
    );
    let _ = io::stdout().flush();

    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    let chosen = parse_opt_out_selection(&line, &ids)?;

    // Record explicitly opted-out agents as dismissed so passive checks stay quiet for them.
    let opted_out: Vec<String> = ids
        .iter()
        .filter(|id| !chosen.contains(id))
        .cloned()
        .collect();
    for id in opted_out {
        if !config
            .agents
            .dismissed
            .iter()
            .any(|d| d.eq_ignore_ascii_case(&id))
        {
            config.agents.dismissed.push(id);
        }
    }

    let report = if chosen.is_empty() {
        None
    } else {
        Some(install_agent_skills(home, config, &chosen, false, false)?)
    };

    let _ = config.save_to(config_path);
    Ok(report)
}

/// Refresh all already-installed Caisson agent skills from this binary's embedded `SKILL.md`.
///
/// Never installs skills for agents the user did not opt into, refuses to run as root or
/// overwrite symlinks, and preserves hand-edited user skills unless `force` is true.
pub fn sync_installed_agent_skills(
    home: &Path,
    config: &mut CaissonConfig,
    force: bool,
    dry_run: bool,
) -> Result<AgentSyncReport> {
    if !dry_run && crate::apfs::statfs::is_root() {
        return Err(CaissonError::Message(
            "refusing to update `$HOME` agent skills as root; run `caisson agent update` without `sudo`".to_string(),
        ));
    }

    let current_hash = skill_content_hash(CAISSON_SKILL_MD);

    let mut updated = Vec::new();
    let mut skipped_customized = Vec::new();
    let mut already_up_to_date = Vec::new();

    for spec in KNOWN_AGENTS {
        let skill_path = home.join(spec.skill_rel);
        if ensure_no_symlink_components_under(home, spec.skill_rel).is_err() {
            continue;
        }
        let Ok(meta) = fs::symlink_metadata(&skill_path) else {
            continue;
        };
        if meta.file_type().is_symlink() || !meta.is_file() {
            continue;
        }

        let (up_to_date, customized) = classify_installed_skill(spec.id, &skill_path, config);
        if up_to_date {
            if !dry_run {
                config
                    .agents
                    .skill_hashes
                    .entry(spec.id.to_string())
                    .or_insert_with(|| current_hash.clone());
            }
            already_up_to_date.push(spec.id.to_string());
            continue;
        }

        let status_entry = AgentHarnessStatus {
            id: spec.id.to_string(),
            display_name: spec.display_name.to_string(),
            detected: detect_agent(home, spec),
            installed: true,
            up_to_date: !dry_run && (!customized || force),
            customized: customized && !force,
            dismissed: config
                .agents
                .dismissed
                .iter()
                .any(|d| d.eq_ignore_ascii_case(spec.id)),
            detect_path: home.join(spec.detect_rel).display().to_string(),
            skill_path: skill_path.display().to_string(),
        };

        if customized && !force {
            skipped_customized.push(status_entry);
            continue;
        }

        if !dry_run {
            atomic_write_skill_file(&skill_path, CAISSON_SKILL_MD)?;
            if !config
                .agents
                .installed
                .iter()
                .any(|x| x.eq_ignore_ascii_case(spec.id))
            {
                config.agents.installed.push(spec.id.to_string());
            }
            config
                .agents
                .skill_hashes
                .insert(spec.id.to_string(), current_hash.clone());
        }
        updated.push(status_entry);
    }

    Ok(AgentSyncReport {
        dry_run,
        version: env!("CARGO_PKG_VERSION").to_string(),
        updated,
        skipped_customized,
        already_up_to_date,
    })
}

/// Automatically refresh already-installed Caisson agent skills after a binary upgrade
/// (e.g. `brew upgrade beckhq/tap/caisson`).
///
/// Runs only when non-root, touches only unmodified already-installed Caisson skill files,
/// and never writes to stdout so `--json` pipelines stay clean.
pub fn auto_sync_installed_skills(config_path: &Path, json: bool) {
    if crate::apfs::statfs::is_root() {
        return;
    }
    let Ok(mut cfg) = CaissonConfig::load_from(config_path) else {
        return;
    };
    let home = resolve_user_home();
    let prev_hashes = cfg.agents.skill_hashes.clone();
    let Ok(report) = sync_installed_agent_skills(&home, &mut cfg, false, false) else {
        return;
    };

    if !report.updated.is_empty() || cfg.agents.skill_hashes != prev_hashes {
        let _ = cfg.save_to(config_path);
    }

    if json {
        return;
    }
    if !report.updated.is_empty() {
        let ids: Vec<&str> = report.updated.iter().map(|t| t.id.as_str()).collect();
        eprintln!(
            "note: auto-updated caisson skill for {} (v{})",
            ids.join(", "),
            report.version
        );
    }
    let interactive = unsafe { libc::isatty(libc::STDERR_FILENO) != 0 };
    if interactive && !report.skipped_customized.is_empty() {
        let ids: Vec<&str> = report
            .skipped_customized
            .iter()
            .map(|t| t.id.as_str())
            .collect();
        eprintln!(
            "tip: customized caisson skill for {} has an update (run `caisson agent update --force` to overwrite).",
            ids.join(", ")
        );
    }
}

/// Passive, `<0.2 ms` new-agent discovery hint printed to stderr ONLY on human-interactive TTY
/// runs (never in `--json` mode, never when stdout/stderr is piped).
pub fn maybe_notify_new_agents(config: &CaissonConfig, json: bool) {
    if json {
        return;
    }
    let interactive =
        unsafe { libc::isatty(libc::STDOUT_FILENO) != 0 && libc::isatty(libc::STDERR_FILENO) != 0 };
    if !interactive {
        return;
    }
    let home = resolve_user_home();
    let pending = find_unconfigured_agents(&home, config);
    if pending.is_empty() {
        return;
    }

    let names: Vec<String> = pending
        .iter()
        .map(|a| format!("{} ({})", a.display_name, a.id))
        .collect();
    let ids: Vec<&str> = pending.iter().map(|a| a.id.as_str()).collect();
    let ids_joined = ids.join(" ");
    eprintln!(
        "\ntip: detected coding agent(s) without the caisson skill: {}.",
        names.join(", ")
    );
    eprintln!(
        "     run `caisson agent install {ids_joined}` (or `caisson agent dismiss {ids_joined}` to silence)."
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn detects_installs_dismisses_and_uninstalls_agent_skills() {
        let dir = tempdir().unwrap();
        let home = dir.path();
        fs::create_dir_all(home.join(".claude")).unwrap();
        fs::create_dir_all(home.join(".codex")).unwrap();

        let mut cfg = CaissonConfig::default();
        let unconfigured = find_unconfigured_agents(home, &cfg);
        let ids: Vec<&str> = unconfigured.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, vec!["claude", "codex"]);

        // Install only claude
        let report =
            install_agent_skills(home, &mut cfg, &["claude".to_string()], false, false).unwrap();
        assert_eq!(report.targets.len(), 1);
        assert!(home.join(".claude/skills/caisson/SKILL.md").is_file());
        assert!(
            fs::read_to_string(home.join(".claude/skills/caisson/SKILL.md"))
                .unwrap()
                .contains("caisson wt add")
        );

        // Only codex remains unconfigured
        let remaining = find_unconfigured_agents(home, &cfg);
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].id, "codex");

        // Dismiss codex -> zero unconfigured agents left!
        dismiss_agents(home, &mut cfg, &["codex".to_string()]).unwrap();
        assert!(find_unconfigured_agents(home, &cfg).is_empty());

        // Uninstall claude removes the skill file
        uninstall_agent_skills(home, &mut cfg, &["claude".to_string()], false, false).unwrap();
        assert!(!home.join(".claude/skills/caisson/SKILL.md").exists());
    }

    #[test]
    fn detects_agy_binary_and_installs_to_gemini_config_skills() {
        let dir = tempdir().unwrap();
        let home = dir.path();
        fs::create_dir_all(home.join(".local/bin")).unwrap();
        fs::write(home.join(".local/bin/agy"), "#!/bin/sh\n").unwrap();

        let mut cfg = CaissonConfig::default();
        let unconfigured = find_unconfigured_agents(home, &cfg);
        assert_eq!(unconfigured.len(), 1);
        assert_eq!(unconfigured[0].id, "antigravity");

        // `agy` alias resolves to `antigravity` and writes `~/.gemini/config/skills/caisson/SKILL.md`.
        install_agent_skills(home, &mut cfg, &["agy".to_string()], false, false).unwrap();
        assert!(home
            .join(".gemini/config/skills/caisson/SKILL.md")
            .is_file());
        assert!(find_unconfigured_agents(home, &cfg).is_empty());
    }

    #[test]
    fn parses_interactive_opt_out_selections() {
        let detected = vec![
            "claude".to_string(),
            "codex".to_string(),
            "cursor".to_string(),
        ];
        assert_eq!(parse_opt_out_selection("", &detected).unwrap(), detected);
        assert_eq!(parse_opt_out_selection("y", &detected).unwrap(), detected);
        assert!(parse_opt_out_selection("n", &detected).unwrap().is_empty());
        assert_eq!(
            parse_opt_out_selection("-cursor", &detected).unwrap(),
            vec!["claude", "codex"]
        );
        assert_eq!(
            parse_opt_out_selection("claude, codex", &detected).unwrap(),
            vec!["claude", "codex"]
        );
        assert!(parse_opt_out_selection("-unknown", &detected).is_err());
    }

    #[test]
    fn sync_auto_updates_unmodified_skills_and_preserves_custom_edits() {
        let dir = tempdir().unwrap();
        let home = dir.path();
        fs::create_dir_all(home.join(".claude")).unwrap();
        fs::create_dir_all(home.join(".codex")).unwrap();

        let mut cfg = CaissonConfig::default();
        install_agent_skills(
            home,
            &mut cfg,
            &["claude".to_string(), "codex".to_string()],
            false,
            false,
        )
        .unwrap();

        // Simulate a previous release's unmodified skill on claude (recorded hash matches disk)
        // and a user-customized skill on codex (disk differs from recorded hash).
        let old_skill = "---\nname: caisson\n---\n# old version\n";
        fs::write(home.join(".claude/skills/caisson/SKILL.md"), old_skill).unwrap();
        cfg.agents
            .skill_hashes
            .insert("claude".to_string(), skill_content_hash(old_skill));

        let custom_skill = "---\nname: caisson\n---\n# user custom rules\n";
        fs::write(home.join(".codex/skills/caisson/SKILL.md"), custom_skill).unwrap();

        let inspected = inspect_agents(home, &cfg);
        let claude = inspected.iter().find(|a| a.id == "claude").unwrap();
        let codex = inspected.iter().find(|a| a.id == "codex").unwrap();
        assert!(!claude.up_to_date && !claude.customized);
        assert!(!codex.up_to_date && codex.customized);

        // Auto-sync updates claude seamlessly and leaves codex untouched.
        let sync = sync_installed_agent_skills(home, &mut cfg, false, false).unwrap();
        assert_eq!(sync.updated.len(), 1);
        assert_eq!(sync.updated[0].id, "claude");
        assert_eq!(sync.skipped_customized.len(), 1);
        assert_eq!(sync.skipped_customized[0].id, "codex");
        assert_eq!(
            fs::read_to_string(home.join(".claude/skills/caisson/SKILL.md")).unwrap(),
            CAISSON_SKILL_MD
        );
        assert_eq!(
            fs::read_to_string(home.join(".codex/skills/caisson/SKILL.md")).unwrap(),
            custom_skill
        );

        // Explicit `--force` overwrites customized skill too.
        let forced = sync_installed_agent_skills(home, &mut cfg, true, false).unwrap();
        assert_eq!(forced.updated.len(), 1);
        assert_eq!(forced.updated[0].id, "codex");
        assert_eq!(
            fs::read_to_string(home.join(".codex/skills/caisson/SKILL.md")).unwrap(),
            CAISSON_SKILL_MD
        );
    }

    #[test]
    fn rejects_symlinked_skill_paths_and_preserves_unreadable_skills() {
        use std::os::unix::fs::symlink;

        let dir = tempdir().unwrap();
        let home = dir.path();
        let outside = tempdir().unwrap();
        symlink(outside.path(), home.join(".claude")).unwrap();

        let mut cfg = CaissonConfig::default();
        let err = install_agent_skills(home, &mut cfg, &["claude".to_string()], false, false)
            .unwrap_err();
        assert!(
            err.to_string().contains("symlink"),
            "expected symlink rejection, got: {}",
            err
        );
        assert!(!outside.path().join("skills/caisson/SKILL.md").exists());

        // Non-UTF8 skill file must be treated as customized and never silently overwritten by sync.
        let codex_skill = home.join(".codex/skills/caisson/SKILL.md");
        fs::create_dir_all(codex_skill.parent().unwrap()).unwrap();
        fs::write(&codex_skill, [0xff, 0xfe, 0x00, 0x01]).unwrap();
        let inspected = inspect_agents(home, &cfg);
        let codex = inspected.iter().find(|a| a.id == "codex").unwrap();
        assert!(codex.installed);
        assert!(codex.customized);
        assert!(!codex.up_to_date);

        let sync = sync_installed_agent_skills(home, &mut cfg, false, false).unwrap();
        assert_eq!(sync.updated.len(), 0);
        assert_eq!(sync.skipped_customized.len(), 1);
        assert_eq!(
            fs::read(&codex_skill).unwrap(),
            vec![0xff, 0xfe, 0x00, 0x01]
        );
    }
}
