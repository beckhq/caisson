use crate::apfs::statfs::invoking_user;
use crate::config::CaissonConfig;
use crate::error::{CaissonError, Result};
use serde::Serialize;
use std::ffi::{CStr, CString};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Embedded canonical `SKILL.md` so `caisson agent install` works completely offline.
pub const CAISSON_SKILL_MD: &str = include_str!("../skills/caisson/SKILL.md");

/// Crisp drop-in snippet for repo-level `AGENTS.md` or `CLAUDE.md` files.
pub const AGENTS_MD_SNIPPET: &str = r#"## macOS Worktrees (Caisson)
- When creating or cleaning up git worktrees on macOS, prefer `caisson wt add <branch> --json`, `caisson wt ls --json`, and `caisson wt rm <slug>` over raw `git worktree` so worktrees run on the daemon-shielded `/Volumes/caisson` APFS volume with instant CoW dependency cloning.
- Never run privileged `sudo caisson ...` commands (`init`, `vol`, `daemon`) from an agent turn.
- If `caisson wt add` returns a `quota_violation`, surface `suggested_command` to the user rather than forcing past quotas.
"#;

#[derive(Debug, Clone, Copy)]
struct AgentSpec {
    id: &'static str,
    display_name: &'static str,
    detect_rel: &'static str,
    skill_rel: &'static str,
}

const KNOWN_AGENTS: &[AgentSpec] = &[
    AgentSpec {
        id: "claude",
        display_name: "Claude Code",
        detect_rel: ".claude",
        skill_rel: ".claude/skills/caisson/SKILL.md",
    },
    AgentSpec {
        id: "codex",
        display_name: "OpenAI Codex",
        detect_rel: ".codex",
        skill_rel: ".codex/skills/caisson/SKILL.md",
    },
    AgentSpec {
        id: "antigravity",
        display_name: "Antigravity (AGY)",
        detect_rel: ".gemini",
        skill_rel: ".gemini/antigravity/skills/caisson/SKILL.md",
    },
    AgentSpec {
        id: "cursor",
        display_name: "Cursor",
        detect_rel: ".cursor",
        skill_rel: ".cursor/skills/caisson/SKILL.md",
    },
    AgentSpec {
        id: "opencode",
        display_name: "OpenCode",
        detect_rel: ".config/opencode",
        skill_rel: ".config/opencode/skills/caisson/SKILL.md",
    },
    AgentSpec {
        id: "windsurf",
        display_name: "Windsurf",
        detect_rel: ".codeium/windsurf",
        skill_rel: ".codeium/windsurf/skills/caisson/SKILL.md",
    },
    AgentSpec {
        id: "agents",
        display_name: "Open Agent Skills (~/.agents)",
        detect_rel: ".agents",
        skill_rel: ".agents/skills/caisson/SKILL.md",
    },
];

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AgentHarnessStatus {
    pub id: String,
    pub display_name: String,
    pub detected: bool,
    pub installed: bool,
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
    KNOWN_AGENTS.iter().find(|s| s.id == lower).ok_or_else(|| {
        let valid = KNOWN_AGENTS
            .iter()
            .map(|s| s.id)
            .collect::<Vec<_>>()
            .join(", ");
        CaissonError::Message(format!(
            "unknown agent harness '{id}'; supported values: {valid}"
        ))
    })
}

/// Inspect detection, skill installation, and dismissal state for all known coding agents.
pub fn inspect_agents(home: &Path, config: &CaissonConfig) -> Vec<AgentHarnessStatus> {
    KNOWN_AGENTS
        .iter()
        .map(|spec| {
            let detect_path = home.join(spec.detect_rel);
            let skill_path = home.join(spec.skill_rel);
            let detected = detect_path.exists();
            let installed = skill_path.is_file();
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
                dismissed,
                detect_path: detect_path.display().to_string(),
                skill_path: skill_path.display().to_string(),
            }
        })
        .collect()
}

/// Install `SKILL.md` for the requested agents (or all detected agents if `targets` is empty).
/// When executed under `sudo` (e.g. during `sudo caisson init`), hands ownership of created
/// skill directories and files back to the invoking developer so root-owned files are never
/// left inside `$HOME`.
pub fn install_agent_skills(
    home: &Path,
    config: &mut CaissonConfig,
    targets: &[String],
    all_detected: bool,
    dry_run: bool,
) -> Result<AgentActionReport> {
    let specs: Vec<&AgentSpec> = if !targets.is_empty() {
        let mut list = Vec::with_capacity(targets.len());
        for t in targets {
            list.push(find_spec(t)?);
        }
        list
    } else if all_detected {
        KNOWN_AGENTS
            .iter()
            .filter(|s| home.join(s.detect_rel).exists())
            .collect()
    } else {
        KNOWN_AGENTS
            .iter()
            .filter(|s| {
                home.join(s.detect_rel).exists()
                    && !config
                        .agents
                        .dismissed
                        .iter()
                        .any(|d| d.eq_ignore_ascii_case(s.id))
            })
            .collect()
    };

    let owner = if crate::apfs::statfs::is_root() {
        invoking_user()
    } else {
        None
    };

    let mut affected = Vec::new();
    for spec in specs {
        let skill_path = home.join(spec.skill_rel);
        if !dry_run {
            if let Some(parent) = skill_path.parent() {
                fs::create_dir_all(parent)?;
                fs::write(&skill_path, CAISSON_SKILL_MD)?;
                if let Some(ref user) = owner {
                    chown_skill_tree(home, spec, user);
                }
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
                .dismissed
                .retain(|x| !x.eq_ignore_ascii_case(spec.id));
        }

        affected.push(AgentHarnessStatus {
            id: spec.id.to_string(),
            display_name: spec.display_name.to_string(),
            detected: home.join(spec.detect_rel).exists(),
            installed: if dry_run { skill_path.is_file() } else { true },
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

fn chown_skill_tree(home: &Path, spec: &AgentSpec, user: &str) {
    let ownership = format!("{user}:staff");
    let detect_dir = home.join(spec.detect_rel);
    let skill_file = home.join(spec.skill_rel);
    let mut cur = skill_file.as_path();
    while cur != home {
        let _ = Command::new("chown")
            .args([&ownership, &cur.display().to_string()])
            .output();
        if cur == detect_dir {
            break;
        }
        match cur.parent() {
            Some(p) => cur = p,
            None => break,
        }
    }
}

/// Remove `SKILL.md` for the specified agent targets (or all installed if `all` is true).
pub fn uninstall_agent_skills(
    home: &Path,
    config: &mut CaissonConfig,
    targets: &[String],
    all: bool,
    dry_run: bool,
) -> Result<AgentActionReport> {
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
        if !dry_run {
            if skill_path.exists() {
                fs::remove_file(&skill_path)?;
            }
            if let Some(parent) = skill_path.parent() {
                let _ = fs::remove_dir(parent);
            }
            config
                .agents
                .installed
                .retain(|x| !x.eq_ignore_ascii_case(spec.id));
        }

        affected.push(AgentHarnessStatus {
            id: spec.id.to_string(),
            display_name: spec.display_name.to_string(),
            detected: home.join(spec.detect_rel).exists(),
            installed: if dry_run { skill_path.is_file() } else { false },
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
        affected.push(AgentHarnessStatus {
            id: spec.id.to_string(),
            display_name: spec.display_name.to_string(),
            detected: home.join(spec.detect_rel).exists(),
            installed: home.join(spec.skill_rel).is_file(),
            dismissed: true,
            detect_path: home.join(spec.detect_rel).display().to_string(),
            skill_path: home.join(spec.skill_rel).display().to_string(),
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

    // Persist config cleanly (handing ownership back to invoking user if run via sudo).
    let _ = config.save_to(config_path);
    if crate::apfs::statfs::is_root() {
        if let Some(user) = invoking_user() {
            let spec = format!("{user}:staff");
            let _ = Command::new("chown")
                .args([&spec, &config_path.display().to_string()])
                .output();
            if let Some(parent) = config_path.parent() {
                let _ = Command::new("chown")
                    .args([&spec, &parent.display().to_string()])
                    .output();
            }
        }
    }

    Ok(report)
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
}
