use caisson::agents::{
    dismiss_agents, inspect_agents, install_agent_skills, maybe_notify_new_agents,
    prompt_interactive_agent_setup, resolve_user_home, uninstall_agent_skills, AGENTS_MD_SNIPPET,
};
use caisson::apfs::clonefile::clone_path;
use caisson::apfs::volume::{
    build_status_report, create_volume, destroy_volume, list_registered_volumes, shield_volume,
    CreateVolumeOptions, QuotaSpec,
};
use caisson::cli::{
    AgentCommands, Cli, Commands, ConfigCommands, DaemonCommands, ShellKind, VolCommands,
    WtCommands,
};
use caisson::config::CaissonConfig;
use caisson::daemon::launchd::{
    daemon_status, install_daemon, mount_all, uninstall_daemon, LAUNCH_DAEMON_PLIST_PATH,
};
use caisson::doctor::{format_doctor_human, run_doctor};
use caisson::error::{CaissonError, Result};
use caisson::registry::{VolumeRegistry, VolumeTier};
use caisson::units::format_bytes;
use caisson::watch::{run_watch, WatchOptions};
use caisson::worktree::manager::{
    run_gc, wt_add, wt_hydrate, wt_ls, wt_path, wt_prune, wt_rm, WtAddOptions, WtHydrateOptions,
    WtPruneOptions,
};
use clap::{CommandFactory, Parser};
use clap_complete::{generate, Shell};
use serde::Serialize;
use std::io;
use std::path::Path;
use std::process::ExitCode;

#[derive(Serialize)]
struct ErrorPayload<'a> {
    error: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    quota_violation: Option<&'a caisson::error::QuotaViolation>,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let json = cli.json;

    match dispatch(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            if json {
                let payload = match &err {
                    CaissonError::QuotaExceeded { violation, .. } => ErrorPayload {
                        error: err.to_string(),
                        quota_violation: Some(violation),
                    },
                    _ => ErrorPayload {
                        error: err.to_string(),
                        quota_violation: None,
                    },
                };
                if let Ok(rendered) = serde_json::to_string_pretty(&payload) {
                    println!("{rendered}");
                } else {
                    eprintln!("error: {err}");
                }
            } else {
                eprintln!("error: {err}");
                if let CaissonError::QuotaExceeded { violation, .. } = &err {
                    eprintln!(
                        "  current: {} | limit: {}",
                        violation.current, violation.limit
                    );
                    if violation.reclaimable_worktrees > 0 {
                        eprintln!(
                            "  reclaimable inactive worktrees: {}",
                            violation.reclaimable_worktrees
                        );
                    }
                    eprintln!("  hint: run `{}`", violation.suggested_command);
                }
            }
            ExitCode::FAILURE
        }
    }
}

fn print_json<T: Serialize>(value: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn dispatch(cli: Cli) -> Result<()> {
    let json = cli.json;
    let registry_path = VolumeRegistry::default_path();
    let config_path = CaissonConfig::default_path();

    match cli.command {
        Commands::Init {
            name,
            quota,
            no_quota,
            case_sensitive,
            nobrowse,
            container,
            mount,
            dry_run,
            no_agents,
        } => {
            let quota_spec = if no_quota {
                QuotaSpec::None
            } else {
                QuotaSpec::parse(&quota)?
            };
            let report = create_volume(
                &CreateVolumeOptions {
                    name,
                    tier: VolumeTier::Tier1,
                    container,
                    mount_point: mount,
                    quota: quota_spec,
                    case_sensitive,
                    nobrowse,
                    dry_run,
                },
                &registry_path,
            )?;
            if json {
                print_json(&report)?;
            } else {
                let prefix = if report.dry_run { "[dry-run] " } else { "" };
                println!(
                    "{prefix}Tier-1 volume '{}' -> {} (container {})",
                    report.name, report.mount_point, report.container
                );
                println!("  command: {}", report.diskutil_command);
                match report.quota_bytes {
                    Some(q) => println!("  hard APFS quota: {}", format_bytes(q)),
                    None => println!("  hard APFS quota: none"),
                }
                if let Some(ref uuid) = report.uuid {
                    println!("  uuid: {uuid}");
                }
                println!("  shields ({} steps):", report.shield_report.steps.len());
                for step in &report.shield_report.steps {
                    if let Some(ref cmd) = step.command {
                        println!("    - [{}] {}: {}", step.status, step.step, cmd);
                    } else {
                        println!("    - [{}] {}", step.status, step.step);
                    }
                }
                if !dry_run && !no_agents {
                    let home = resolve_user_home();
                    let mut cfg = CaissonConfig::load_from(&config_path)?;
                    if let Some(agent_report) =
                        prompt_interactive_agent_setup(&home, &config_path, &mut cfg)?
                    {
                        println!(
                            "Installed Caisson skill for {} agent(s):",
                            agent_report.targets.len()
                        );
                        for t in &agent_report.targets {
                            println!("  - {} -> {}", t.id, t.skill_path);
                        }
                    }
                }
            }
        }

        Commands::Vol { action } => match action {
            VolCommands::Create {
                name,
                quota,
                case_sensitive,
                nobrowse,
                container,
                mount,
                dry_run,
            } => {
                let quota_spec = match quota {
                    Some(ref q) => QuotaSpec::parse(q)?,
                    None => QuotaSpec::None,
                };
                let report = create_volume(
                    &CreateVolumeOptions {
                        name,
                        tier: VolumeTier::Tier2,
                        container,
                        mount_point: mount,
                        quota: quota_spec,
                        case_sensitive,
                        nobrowse,
                        dry_run,
                    },
                    &registry_path,
                )?;
                if json {
                    print_json(&report)?;
                } else {
                    let prefix = if report.dry_run { "[dry-run] " } else { "" };
                    println!(
                        "{prefix}Tier-2 volume '{}' -> {} (container {})",
                        report.name, report.mount_point, report.container
                    );
                    println!("  command: {}", report.diskutil_command);
                    match report.quota_bytes {
                        Some(q) => println!("  hard APFS quota: {}", format_bytes(q)),
                        None => println!("  hard APFS quota: none"),
                    }
                    if let Some(ref uuid) = report.uuid {
                        println!("  uuid: {uuid}");
                    }
                }
            }

            VolCommands::Ls => {
                let volumes = list_registered_volumes(&registry_path)?;
                if json {
                    print_json(&volumes)?;
                } else if volumes.is_empty() {
                    println!(
                        "No Caisson volumes registered. Run `sudo caisson init` to create one."
                    );
                } else {
                    println!(
                        "{:<18} {:<7} {:<10} {:<10} {:<10} MOUNT",
                        "NAME", "TIER", "USED", "QUOTA", "SHIELDS"
                    );
                    for v in &volumes {
                        let tier = match v.tier {
                            VolumeTier::Tier1 => "tier-1",
                            VolumeTier::Tier2 => "tier-2",
                        };
                        let quota_str = v
                            .quota_bytes
                            .map(format_bytes)
                            .unwrap_or_else(|| "none".to_string());
                        let shields = if !v.mounted {
                            "unmounted"
                        } else if v.shields.all_healthy {
                            "healthy"
                        } else {
                            "degraded"
                        };
                        println!(
                            "{:<18} {:<7} {:<10} {:<10} {:<10} {}",
                            v.name,
                            tier,
                            format_bytes(v.used_bytes),
                            quota_str,
                            shields,
                            v.mount_point
                        );
                    }
                }
            }

            VolCommands::Shield {
                target,
                nobrowse,
                dry_run,
            } => {
                let nb = nobrowse.then_some(true);
                let report = shield_volume(&target, &registry_path, nb, dry_run)?;
                if json {
                    print_json(&report)?;
                } else {
                    let prefix = if report.dry_run { "[dry-run] " } else { "" };
                    println!("{prefix}Shielded {}:", report.mount_point);
                    for step in &report.steps {
                        if let Some(ref cmd) = step.command {
                            println!("  - [{}] {}: {}", step.status, step.step, cmd);
                        } else {
                            println!("  - [{}] {}", step.status, step.step);
                        }
                    }
                }
            }

            VolCommands::Destroy {
                target,
                yes,
                dry_run,
            } => {
                let report = destroy_volume(&target, &registry_path, yes, dry_run)?;
                if json {
                    print_json(&report)?;
                } else {
                    let prefix = if report.dry_run { "[dry-run] " } else { "" };
                    println!(
                        "{prefix}Destroyed volume '{}' ({} / {}): triple-check safety guard passed",
                        report.name, report.uuid, report.device_identifier
                    );
                    if report.dry_run {
                        println!("  command: {}", report.command);
                    }
                }
            }
        },

        Commands::Status => {
            let config = CaissonConfig::load_from(&config_path)?;
            let status = build_status_report(&registry_path, &config)?;
            if json {
                print_json(&status)?;
            } else {
                if let Some(ref c) = status.internal_container {
                    println!(
                        "Container:   {} ({} free / {} total, {} volumes)",
                        c.container_reference,
                        format_bytes(c.capacity_free),
                        format_bytes(c.capacity_ceiling),
                        c.volume_count
                    );
                }
                println!(
                    "Tier-1 root: {} ({})",
                    status.tier1_root,
                    if status.tier1_root_exists {
                        "present"
                    } else {
                        "not initialized"
                    }
                );
                println!(
                    "Boot daemon: {} (installed={}, loaded={})",
                    status.daemon.label, status.daemon.installed, status.daemon.loaded
                );
                println!(
                    "Soft quotas: max_per_repo={}, max_total={}, soft_disk={}, min_host_free={}",
                    status.config.quotas.max_worktrees_per_repo,
                    status.config.quotas.max_worktrees_total,
                    status.config.quotas.soft_disk_limit,
                    status.config.quotas.min_host_free
                );
                println!("Volumes:     {} registered", status.volumes.len());
                for v in &status.volumes {
                    println!(
                        "  - {} ({}) used={} shields={}",
                        v.name,
                        v.mount_point,
                        format_bytes(v.used_bytes),
                        if v.shields.all_healthy {
                            "healthy"
                        } else {
                            "degraded"
                        }
                    );
                }
                maybe_notify_new_agents(&config, json);
            }
        }

        Commands::Daemon { action } => {
            let plist_path = Path::new(LAUNCH_DAEMON_PLIST_PATH);
            match action {
                DaemonCommands::Install { bin, dry_run } => {
                    let report = install_daemon(bin.as_deref(), plist_path, dry_run)?;
                    if json {
                        print_json(&report)?;
                    } else {
                        let prefix = if report.dry_run { "[dry-run] " } else { "" };
                        println!(
                            "{prefix}LaunchDaemon {} -> {}",
                            report.label, report.plist_path
                        );
                        for cmd in &report.commands {
                            println!("  - {cmd}");
                        }
                    }
                }
                DaemonCommands::Uninstall { dry_run } => {
                    let report = uninstall_daemon(plist_path, dry_run)?;
                    if json {
                        print_json(&report)?;
                    } else {
                        let prefix = if report.dry_run { "[dry-run] " } else { "" };
                        println!(
                            "{prefix}Removed LaunchDaemon {} ({})",
                            report.label, report.plist_path
                        );
                        for cmd in &report.commands {
                            println!("  - {cmd}");
                        }
                    }
                }
                DaemonCommands::Status => {
                    let status = daemon_status(plist_path);
                    if json {
                        print_json(&status)?;
                    } else {
                        println!("Label:     {}", status.label);
                        println!("Plist:     {}", status.plist_path);
                        println!("Installed: {}", status.installed);
                        println!("Loaded:    {}", status.loaded);
                        if !status.program_arguments.is_empty() {
                            println!("Command:   {}", status.program_arguments.join(" "));
                        }
                    }
                }
            }
        }

        Commands::MountAll { dry_run } => {
            let report = mount_all(&registry_path, dry_run)?;
            if json {
                print_json(&report)?;
            } else {
                let prefix = if report.dry_run { "[dry-run] " } else { "" };
                if report.volumes.is_empty() {
                    println!("{prefix}No registered Caisson volumes to mount.");
                } else {
                    for v in &report.volumes {
                        if let Some(ref err) = v.error {
                            println!("{prefix}{} ({}): ERROR {err}", v.name, v.mount_point);
                        } else {
                            println!(
                                "{prefix}{} ({}): {}",
                                v.name, v.mount_point, v.mounted_action
                            );
                        }
                    }
                }
            }
            if report.has_errors() {
                return Err(CaissonError::Message(
                    "one or more volumes failed during `mount-all`".to_string(),
                ));
            }
        }

        Commands::Config { action } => match action {
            ConfigCommands::Show => {
                let cfg = CaissonConfig::load_from(&config_path)?;
                if json {
                    print_json(&cfg)?;
                } else {
                    println!("# {}", config_path.display());
                    print!("{}", toml::to_string_pretty(&cfg)?);
                }
            }
            ConfigCommands::Get { key } => {
                let cfg = CaissonConfig::load_from(&config_path)?;
                let val = cfg.get_key(&key)?;
                if json {
                    #[derive(Serialize)]
                    struct Kv<'a> {
                        key: &'a str,
                        value: String,
                    }
                    print_json(&Kv {
                        key: &key,
                        value: val,
                    })?;
                } else {
                    println!("{val}");
                }
            }
            ConfigCommands::Set { key, value } => {
                let mut cfg = CaissonConfig::load_from(&config_path)?;
                cfg.set_key(&key, &value)?;
                cfg.save_to(&config_path)?;
                let updated = cfg.get_key(&key)?;
                if json {
                    #[derive(Serialize)]
                    struct Kv<'a> {
                        key: &'a str,
                        value: String,
                        path: String,
                    }
                    print_json(&Kv {
                        key: &key,
                        value: updated,
                        path: config_path.display().to_string(),
                    })?;
                } else {
                    println!("{key} = {updated}");
                }
            }
        },

        Commands::Doctor => {
            let cfg = CaissonConfig::load_from(&config_path)?;
            let report = run_doctor(&registry_path, &cfg)?;
            if json {
                print_json(&report)?;
            } else {
                print!("{}", format_doctor_human(&report));
                maybe_notify_new_agents(&cfg, json);
            }
        }

        Commands::Watch {
            label,
            out,
            interval,
            watch_pid,
            post_settle,
            summarize,
            append,
        } => {
            let (summary, summary_path) = run_watch(&WatchOptions {
                label,
                out,
                interval_secs: interval,
                watch_pid,
                post_settle_secs: post_settle,
                summarize,
                append,
            })?;
            if json || summary_path.is_none() {
                print_json(&summary)?;
            } else if let Some(sp) = summary_path {
                println!(
                    "Saved summary -> {} (wall={:.2}s, samples={}, daemon_cpu={:.3}s)",
                    sp.display(),
                    summary.wall_s,
                    summary.samples,
                    summary.daemon_total_cpu_s
                );
            }
        }

        Commands::Clone { src, dst } => {
            clone_path(&src, &dst)?;
            if json {
                #[derive(Serialize)]
                struct CloneResult {
                    src: String,
                    dst: String,
                    cloned: bool,
                }
                print_json(&CloneResult {
                    src: src.display().to_string(),
                    dst: dst.display().to_string(),
                    cloned: true,
                })?;
            } else {
                println!("Cloned {} -> {} (APFS CoW)", src.display(), dst.display());
            }
        }

        Commands::Wt { action } => {
            let cfg = CaissonConfig::load_from(&config_path)?;
            match action {
                WtCommands::Add {
                    branch,
                    from,
                    name,
                    repo,
                    no_cow,
                    force,
                } => {
                    let report = wt_add(
                        &cfg,
                        &WtAddOptions {
                            branch,
                            from_ref: from,
                            name,
                            repo_path: repo,
                            no_cow,
                            force,
                        },
                    )?;
                    if json {
                        print_json(&report)?;
                    } else {
                        for w in &report.warnings {
                            eprintln!("warning: {w}");
                        }
                        for c in &report.cow_clones {
                            if c.status == "cloned" {
                                eprintln!(
                                    "cow-cloned '{}' from {}",
                                    c.entry,
                                    c.source.as_deref().unwrap_or("")
                                );
                            } else if let Some(ref detail) = c.detail {
                                eprintln!("cow-copy '{}' ({}): {detail}", c.entry, c.status);
                            }
                        }
                        // Print canonical worktree path on stdout for easy shell capture (`cd "$(caisson wt add ...)"`).
                        println!("{}", report.path);
                        maybe_notify_new_agents(&cfg, json);
                    }
                }

                WtCommands::Ls { repo, all } => {
                    let entries = wt_ls(&cfg, repo.as_deref(), all)?;
                    if json {
                        print_json(&entries)?;
                    } else if entries.is_empty() {
                        println!("No managed Caisson worktrees found.");
                    } else {
                        println!(
                            "{:<22} {:<20} {:<8} {:<10} {:<14} PATH",
                            "SLUG", "BRANCH", "IDLE", "SIZE", "STATE"
                        );
                        for e in &entries {
                            let mut flags = Vec::new();
                            if e.dirty {
                                flags.push("dirty");
                            }
                            if e.unpushed {
                                flags.push("unpushed");
                            }
                            if e.inspection_failed {
                                flags.push("unknown");
                            }
                            if e.orphaned {
                                flags.push("orphaned");
                            }
                            let state = if flags.is_empty() {
                                "clean".to_string()
                            } else {
                                flags.join(",")
                            };
                            println!(
                                "{:<22} {:<20} {:<8} {:<10} {:<14} {}",
                                e.slug,
                                e.branch.as_deref().unwrap_or("-"),
                                e.inactive_human,
                                format_bytes(e.disk_bytes),
                                state,
                                e.path
                            );
                        }
                    }
                }

                WtCommands::Path { target, repo } => {
                    let entry = wt_path(&cfg, &target, repo.as_deref())?;
                    if json {
                        print_json(&entry)?;
                    } else {
                        println!("{}", entry.path);
                    }
                }

                WtCommands::Rm {
                    target,
                    repo,
                    force,
                    sync,
                } => {
                    let report = wt_rm(&cfg, &target, repo.as_deref(), force, sync)?;
                    if json {
                        print_json(&report)?;
                    } else {
                        let mode = if report.cleaned_sync {
                            "unlinked synchronously"
                        } else {
                            "unlinking in background"
                        };
                        println!(
                            "Removed worktree '{}' in {:.2} ms ({mode})",
                            report.slug, report.rename_ms
                        );
                    }
                }

                WtCommands::Prune {
                    inactive,
                    include_dirty,
                    include_unpushed,
                    all,
                    repo,
                    apply,
                    sync,
                } => {
                    let report = wt_prune(
                        &cfg,
                        &WtPruneOptions {
                            inactive,
                            include_dirty,
                            include_unpushed,
                            all,
                            repo_path: repo,
                            apply,
                            sync,
                        },
                    )?;
                    if json {
                        print_json(&report)?;
                    } else {
                        let prefix = if report.dry_run { "[dry-run] " } else { "" };
                        println!(
                            "{prefix}Prune (inactive >= {}): {} candidate(s) ({}), {} protected, {} active",
                            report.inactive_threshold,
                            report.pruned.len(),
                            format_bytes(report.reclaimed_bytes),
                            report.skipped_protected.len(),
                            report.active_count
                        );
                        for w in &report.pruned {
                            println!(
                                "  {} {} (idle {}, {})",
                                if report.dry_run {
                                    "would remove"
                                } else {
                                    "removed"
                                },
                                w.slug,
                                w.inactive_human,
                                format_bytes(w.disk_bytes)
                            );
                        }
                        for s in &report.skipped_protected {
                            println!(
                                "  skipped {} (idle {}, protected: {})",
                                s.slug, s.inactive_human, s.reason
                            );
                        }
                        if report.dry_run && !report.pruned.is_empty() {
                            println!(
                                "Re-run with `--apply` to remove these {} worktree(s).",
                                report.pruned.len()
                            );
                        }
                    }
                }

                WtCommands::Hydrate {
                    path,
                    dry_run,
                    no_post_create,
                } => {
                    let report = wt_hydrate(
                        &cfg,
                        &WtHydrateOptions {
                            path,
                            dry_run,
                            no_post_create,
                        },
                    )?;
                    if json {
                        print_json(&report)?;
                    } else {
                        let prefix = if report.dry_run { "[dry-run] " } else { "" };
                        if report.cow_clones.is_empty() && report.post_create_command.is_none() {
                            println!(
                                "{prefix}Hydrate {}: no .caisson.toml cow_copy entries configured.",
                                report.worktree_path
                            );
                        } else {
                            for c in &report.cow_clones {
                                match c.status.as_str() {
                                    "cloned" => println!(
                                        "{prefix}cow-cloned '{}' from {}",
                                        c.entry,
                                        c.source.as_deref().unwrap_or("")
                                    ),
                                    "planned" => println!(
                                        "{prefix}would cow-clone '{}' from {}",
                                        c.entry,
                                        c.source.as_deref().unwrap_or("")
                                    ),
                                    _ => println!(
                                        "{prefix}cow-copy '{}' ({}): {}",
                                        c.entry,
                                        c.status,
                                        c.detail.as_deref().unwrap_or("")
                                    ),
                                }
                            }
                            if let Some(ref hook) = report.post_create_command {
                                if report.dry_run {
                                    println!("{prefix}would run post_create: {hook}");
                                } else if report.post_create_ran {
                                    println!("ran post_create: {hook}");
                                }
                            }
                        }
                    }
                }
            }
        }

        Commands::Gc { repo } => {
            let cfg = CaissonConfig::load_from(&config_path)?;
            let report = run_gc(&cfg, repo.as_deref())?;
            if json {
                print_json(&report)?;
            } else {
                println!(
                    "GC complete: drained {} trash item(s) ({}), pruned {} repo(s), removed {} empty repo dir(s)",
                    report.trash.entries_removed,
                    format_bytes(report.trash.bytes_reclaimed),
                    report.pruned_repos.len(),
                    report.removed_empty_repo_dirs
                );
            }
        }

        Commands::Completions { shell } => {
            let mut cmd = Cli::command();
            let clap_shell = match shell {
                ShellKind::Zsh => Shell::Zsh,
                ShellKind::Bash => Shell::Bash,
                ShellKind::Fish => Shell::Fish,
            };
            generate(clap_shell, &mut cmd, "caisson", &mut io::stdout());
        }

        Commands::ShellInit { shell, alias } => {
            print!("{}", render_shell_init(shell, alias.as_deref()));
        }

        Commands::Agent { action } => {
            let home = resolve_user_home();
            let mut cfg = CaissonConfig::load_from(&config_path)?;
            match action {
                AgentCommands::Status => {
                    let statuses = inspect_agents(&home, &cfg);
                    if json {
                        print_json(&statuses)?;
                    } else {
                        println!(
                            "{:<14} {:<10} {:<10} {:<10} SKILL PATH",
                            "AGENT", "DETECTED", "INSTALLED", "DISMISSED"
                        );
                        for s in &statuses {
                            println!(
                                "{:<14} {:<10} {:<10} {:<10} {}",
                                s.id, s.detected, s.installed, s.dismissed, s.skill_path
                            );
                        }
                    }
                }

                AgentCommands::Install {
                    targets,
                    all,
                    dry_run,
                } => {
                    let report = if targets.is_empty() && !all && !dry_run && !json {
                        let interactive = unsafe {
                            libc::isatty(libc::STDIN_FILENO) != 0
                                && libc::isatty(libc::STDOUT_FILENO) != 0
                        };
                        if interactive {
                            match prompt_interactive_agent_setup(&home, &config_path, &mut cfg)? {
                                Some(r) => r,
                                None => {
                                    println!("No new agent skills installed.");
                                    return Ok(());
                                }
                            }
                        } else {
                            let r = install_agent_skills(&home, &mut cfg, &[], false, false)?;
                            cfg.save_to(&config_path)?;
                            r
                        }
                    } else {
                        let r = install_agent_skills(&home, &mut cfg, &targets, all, dry_run)?;
                        if !dry_run {
                            cfg.save_to(&config_path)?;
                        }
                        r
                    };

                    if json {
                        print_json(&report)?;
                    } else if report.targets.is_empty() {
                        println!("No matching coding agents detected (pass explicit IDs like `caisson agent install claude codex`).");
                    } else {
                        let prefix = if report.dry_run { "[dry-run] " } else { "" };
                        println!(
                            "{prefix}Installed Caisson skill for {} agent(s):",
                            report.targets.len()
                        );
                        for t in &report.targets {
                            println!("  - {} ({}) -> {}", t.id, t.display_name, t.skill_path);
                        }
                    }
                }

                AgentCommands::Uninstall {
                    targets,
                    all,
                    dry_run,
                } => {
                    let report = uninstall_agent_skills(&home, &mut cfg, &targets, all, dry_run)?;
                    if !dry_run {
                        cfg.save_to(&config_path)?;
                    }
                    if json {
                        print_json(&report)?;
                    } else {
                        let prefix = if report.dry_run { "[dry-run] " } else { "" };
                        println!(
                            "{prefix}Removed Caisson skill for {} agent(s):",
                            report.targets.len()
                        );
                        for t in &report.targets {
                            println!("  - {} ({})", t.id, t.skill_path);
                        }
                    }
                }

                AgentCommands::Dismiss { targets } => {
                    let report = dismiss_agents(&home, &mut cfg, &targets)?;
                    cfg.save_to(&config_path)?;
                    if json {
                        print_json(&report)?;
                    } else {
                        println!(
                            "Silenced passive discovery tips for: {}",
                            report
                                .targets
                                .iter()
                                .map(|t| t.id.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        );
                    }
                }

                AgentCommands::Snippet => {
                    if json {
                        #[derive(Serialize)]
                        struct SnippetOut {
                            snippet: &'static str,
                        }
                        print_json(&SnippetOut {
                            snippet: AGENTS_MD_SNIPPET,
                        })?;
                    } else {
                        print!("{AGENTS_MD_SNIPPET}");
                    }
                }
            }
        }
    }

    Ok(())
}

fn render_shell_init(shell: ShellKind, alias: Option<&str>) -> String {
    let mut script = match shell {
        ShellKind::Zsh | ShellKind::Bash => String::from(
            r#"# Caisson shell integration: enables `caisson wt cd <target>`
caisson() {
  if [ "$1" = "wt" ] && [ "$2" = "cd" ] && [ -n "$3" ]; then
    local target_dir
    target_dir="$(command caisson wt path "$3")" || return $?
    builtin cd "$target_dir" || return $?
    return 0
  fi
  command caisson "$@"
}
"#,
        ),
        ShellKind::Fish => String::from(
            r#"# Caisson shell integration: enables `caisson wt cd <target>`
function caisson --wraps caisson
    if test (count $argv) -ge 3; and test "$argv[1]" = "wt"; and test "$argv[2]" = "cd"
        set -l target_dir (command caisson wt path "$argv[3]")
        or return $status
        builtin cd "$target_dir"
        return $status
    end
    command caisson $argv
end
"#,
        ),
    };

    if let Some(short) = alias {
        let trimmed = short.trim();
        if !trimmed.is_empty()
            && trimmed
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            match shell {
                ShellKind::Zsh | ShellKind::Bash => {
                    script.push_str(&format!("alias {trimmed}=caisson\n"));
                }
                ShellKind::Fish => {
                    script.push_str(&format!("alias {trimmed} caisson\n"));
                }
            }
        }
    }
    script
}
