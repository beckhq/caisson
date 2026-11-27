use caisson::apfs::clonefile::clone_path;
use caisson::apfs::volume::{
    build_status_report, create_volume, destroy_volume, list_registered_volumes, shield_volume,
    CreateVolumeOptions, QuotaSpec,
};
use caisson::cli::{Cli, Commands, ConfigCommands, DaemonCommands, VolCommands};
use caisson::config::CaissonConfig;
use caisson::daemon::launchd::{
    daemon_status, install_daemon, mount_all, uninstall_daemon, LAUNCH_DAEMON_PLIST_PATH,
};
use caisson::doctor::{format_doctor_human, run_doctor};
use caisson::error::{CaissonError, Result};
use caisson::registry::{VolumeRegistry, VolumeTier};
use caisson::units::format_bytes;
use caisson::watch::{run_watch, WatchOptions};
use clap::Parser;
use serde::Serialize;
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
                        "{prefix}Destroyed volume '{}' ({} / {}) — triple-check safety guard passed",
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
            }
        }

        Commands::Daemon { action } => {
            let plist_path = Path::new(LAUNCH_DAEMON_PLIST_PATH);
            match action {
                DaemonCommands::Install { bin, dry_run } => {
                    let bin_path = match bin {
                        Some(p) => p,
                        None => std::env::current_exe()?,
                    };
                    let report = install_daemon(&bin_path, plist_path, dry_run)?;
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
            }
        }

        Commands::Watch {
            label,
            out,
            interval,
            watch_pid,
            post_settle,
            summarize,
        } => {
            let (summary, summary_path) = run_watch(&WatchOptions {
                label,
                out,
                interval_secs: interval,
                watch_pid,
                post_settle_secs: post_settle,
                summarize,
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
    }

    Ok(())
}
