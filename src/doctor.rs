use crate::apfs::shield::{inspect_shields, parse_mdutil_disabled, ShieldStatus};
use crate::apfs::volume::list_registered_volumes;
use crate::config::CaissonConfig;
use crate::daemon::launchd::{daemon_status, DaemonStatus, LAUNCH_DAEMON_PLIST_PATH};
use crate::error::Result;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

pub const TRACKED_DAEMONS: &[&str] = &[
    "mds_stores",
    "mds",
    "mdworker_shared",
    "syspolicyd",
    "fseventsd",
    "lsd",
    "backupd",
    "XProtect",
];

pub const FSEVENTS_BLOAT_WARN_MB: f64 = 500.0;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct DaemonStats {
    pub cpu_seconds: f64,
    pub cpu_minutes: f64,
    pub rss_mb: f64,
    pub procs: usize,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SwapUsage {
    pub total_mb: f64,
    pub used_mb: f64,
    pub free_mb: f64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DoctorWarning {
    pub code: String,
    pub severity: String,
    pub message: String,
    pub remediation: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct VolumeShieldAudit {
    pub name: String,
    pub mount_point: String,
    pub shields: ShieldStatus,
}

#[derive(Debug, Clone, Serialize)]
pub struct DoctorReport {
    pub macos_version: String,
    pub chip: String,
    pub ram_gb: f64,
    pub uptime: String,
    pub kern_maxvnodes: u64,
    pub loadavg: Vec<f64>,
    pub swap: SwapUsage,
    pub spotlight_root_status: String,
    pub spotlight_root_enabled: bool,
    pub daemons: BTreeMap<String, DaemonStats>,
    pub local_apfs_snapshots: Vec<String>,
    pub leaked_temp_mounts: Vec<String>,
    pub caisson_volumes: Vec<VolumeShieldAudit>,
    pub launch_daemon: DaemonStatus,
    pub developer_tools_hint: String,
    pub warnings: Vec<DoctorWarning>,
}

fn run_capture(program: &str, args: &[&str]) -> String {
    Command::new(program)
        .args(args)
        .output()
        .ok()
        .map(|o| {
            let stdout = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if stdout.is_empty() {
                String::from_utf8_lossy(&o.stderr).trim().to_string()
            } else {
                stdout
            }
        })
        .unwrap_or_default()
}

/// Parse BSD `ps` cumulative CPU time (`[dd-]hh:mm:ss` or `mm:ss.cc`) into seconds.
pub fn parse_ps_seconds(raw: &str) -> f64 {
    let trimmed = raw.trim();
    let (days, rest) = if let Some((d, r)) = trimmed.split_once('-') {
        (d.parse::<f64>().unwrap_or(0.0), r)
    } else {
        (0.0, trimmed)
    };
    let parts: Vec<&str> = rest.split(':').collect();
    let secs = match parts.as_slice() {
        [m, s] => m.parse::<f64>().unwrap_or(0.0) * 60.0 + s.parse::<f64>().unwrap_or(0.0),
        [h, m, s] => {
            h.parse::<f64>().unwrap_or(0.0) * 3600.0
                + m.parse::<f64>().unwrap_or(0.0) * 60.0
                + s.parse::<f64>().unwrap_or(0.0)
        }
        _ => 0.0,
    };
    days * 86_400.0 + secs
}

/// Parse `ps -eo pid,pcpu,time,rss,comm` output into aggregated daemon stats.
pub fn parse_ps_daemons(ps_output: &str) -> BTreeMap<String, DaemonStats> {
    let mut stats: BTreeMap<String, DaemonStats> = BTreeMap::new();

    for line in ps_output.lines().skip(1) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let mut iter = trimmed.split_whitespace();
        let _pid = match iter.next() {
            Some(v) => v,
            None => continue,
        };
        let _pcpu = match iter.next() {
            Some(v) => v,
            None => continue,
        };
        let cputime = match iter.next() {
            Some(v) => v,
            None => continue,
        };
        let rss_kb = match iter.next().and_then(|v| v.parse::<f64>().ok()) {
            Some(v) => v,
            None => continue,
        };
        let comm: Vec<&str> = iter.collect();
        if comm.is_empty() {
            continue;
        }
        let comm_joined = comm.join(" ");
        let basename = Path::new(&comm_joined)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(&comm_joined);

        let matched = TRACKED_DAEMONS
            .iter()
            .find(|&&d| basename == d || (d == "XProtect" && basename.starts_with("XProtect")));
        let Some(&daemon_name) = matched else {
            continue;
        };

        let entry = stats
            .entry(daemon_name.to_string())
            .or_insert_with(|| DaemonStats {
                cpu_seconds: 0.0,
                cpu_minutes: 0.0,
                rss_mb: 0.0,
                procs: 0,
            });
        let secs = parse_ps_seconds(cputime);
        entry.cpu_seconds += secs;
        entry.rss_mb += rss_kb / 1024.0;
        entry.procs += 1;
    }

    for entry in stats.values_mut() {
        entry.cpu_seconds = round2(entry.cpu_seconds);
        entry.cpu_minutes = round2(entry.cpu_seconds / 60.0);
        entry.rss_mb = round2(entry.rss_mb);
    }

    stats
}

/// Parse `sysctl -n vm.swapusage` output (e.g. `total = 2048.00M  used = 472.62M  free = 1575.38M`).
pub fn parse_swap_usage(output: &str) -> SwapUsage {
    let parse_field = |key: &str| -> f64 {
        let needle = format!("{key} =");
        let Some(idx) = output.find(&needle) else {
            return 0.0;
        };
        let after = output[idx + needle.len()..].trim_start();
        let token = after.split_whitespace().next().unwrap_or("");
        parse_mb_token(token)
    };
    SwapUsage {
        total_mb: round1(parse_field("total")),
        used_mb: round1(parse_field("used")),
        free_mb: round1(parse_field("free")),
    }
}

fn parse_mb_token(token: &str) -> f64 {
    let trimmed = token.trim();
    if trimmed.is_empty() {
        return 0.0;
    }
    let last = trimmed.chars().last().unwrap_or(' ');
    let (num_str, mult) = match last {
        'M' | 'm' => (&trimmed[..trimmed.len() - 1], 1.0),
        'G' | 'g' => (&trimmed[..trimmed.len() - 1], 1024.0),
        'T' | 't' => (&trimmed[..trimmed.len() - 1], 1024.0 * 1024.0),
        'K' | 'k' => (&trimmed[..trimmed.len() - 1], 1.0 / 1024.0),
        _ => (trimmed, 1.0),
    };
    num_str.parse::<f64>().unwrap_or(0.0) * mult
}

/// Parse `sysctl -n vm.loadavg` output (e.g. `{ 2.30 2.06 2.02 }`).
pub fn parse_loadavg(output: &str) -> Vec<f64> {
    output
        .split(|c: char| c.is_whitespace() || c == '{' || c == '}')
        .filter_map(|tok| tok.parse::<f64>().ok())
        .take(3)
        .collect()
}

/// Parse `tmutil listlocalsnapshots /` output into snapshot names.
pub fn parse_local_snapshots(output: &str) -> Vec<String> {
    output
        .lines()
        .map(str::trim)
        .filter(|line| {
            !line.is_empty() && !line.starts_with("Snapshots for disk") && !line.ends_with(':')
        })
        .map(ToString::to_string)
        .collect()
}

/// Detect leaked ephemeral DMG / test mounts under `/var/folders/`.
pub fn parse_leaked_temp_mounts(mount_output: &str) -> Vec<String> {
    mount_output
        .lines()
        .filter(|line| {
            if !line.contains("/var/folders/") {
                return false;
            }
            let lower = line.to_ascii_lowercase();
            lower.contains("dmg") || lower.contains("test")
        })
        .map(|s| s.trim().to_string())
        .collect()
}

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/// Collect a complete live read-only macOS diagnostic audit.
pub fn run_doctor(registry_path: &Path, config: &CaissonConfig) -> Result<DoctorReport> {
    let macos_version = run_capture("sw_vers", &["-productVersion"]);
    let chip = run_capture("sysctl", &["-n", "machdep.cpu.brand_string"]);
    let memsize_bytes: u64 = run_capture("sysctl", &["-n", "hw.memsize"])
        .parse()
        .unwrap_or(0);
    let ram_gb = round1(memsize_bytes as f64 / (1024.0_f64.powi(3)));
    let uptime = run_capture("uptime", &[]);
    let kern_maxvnodes: u64 = run_capture("sysctl", &["-n", "kern.maxvnodes"])
        .parse()
        .unwrap_or(0);
    let loadavg = parse_loadavg(&run_capture("sysctl", &["-n", "vm.loadavg"]));
    let swap = parse_swap_usage(&run_capture("sysctl", &["-n", "vm.swapusage"]));

    let spotlight_raw = run_capture("mdutil", &["-s", "/"]);
    let spotlight_root_enabled = !parse_mdutil_disabled(&spotlight_raw);
    let spotlight_root_status = spotlight_raw
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ");

    let ps_out = run_capture("ps", &["-eo", "pid,pcpu,time,rss,comm"]);
    let daemons = parse_ps_daemons(&ps_out);

    let snapshots_raw = run_capture("tmutil", &["listlocalsnapshots", "/"]);
    let local_apfs_snapshots = parse_local_snapshots(&snapshots_raw);

    let mount_raw = run_capture("mount", &[]);
    let leaked_temp_mounts = parse_leaked_temp_mounts(&mount_raw);

    let mut caisson_volumes = Vec::new();
    if let Ok(vols) = list_registered_volumes(registry_path) {
        for v in vols {
            caisson_volumes.push(VolumeShieldAudit {
                name: v.name,
                mount_point: v.mount_point,
                shields: v.shields,
            });
        }
    }
    let tier1_root = config.resolve_root();
    if tier1_root.exists()
        && !caisson_volumes
            .iter()
            .any(|v| Path::new(&v.mount_point) == tier1_root.as_path())
    {
        caisson_volumes.push(VolumeShieldAudit {
            name: "tier1-root".to_string(),
            mount_point: tier1_root.display().to_string(),
            shields: inspect_shields(&tier1_root),
        });
    }

    let launch_daemon = daemon_status(Path::new(LAUNCH_DAEMON_PLIST_PATH));
    let developer_tools_hint = "Ensure your terminal/IDE is authorized in System Settings -> Privacy & Security -> Developer Tools (or run `spctl developer-mode enable-terminal`) to reduce syspolicyd/XProtect scanning overhead on freshly compiled local binaries.".to_string();

    let mut warnings = Vec::new();

    if let Some(fs_stats) = daemons.get("fseventsd") {
        if fs_stats.rss_mb > FSEVENTS_BLOAT_WARN_MB {
            warnings.push(DoctorWarning {
                code: "FSEVENTS_BLOAT".to_string(),
                severity: "warn".to_string(),
                message: format!(
                    "fseventsd resident memory is {:.1} MB (exceeds {:.0} MB threshold)",
                    fs_stats.rss_mb, FSEVENTS_BLOAT_WARN_MB
                ),
                remediation: "Move heavy builds/worktrees onto a Caisson sibling APFS volume with `.fseventsd/no_log` (`sudo caisson init`).".to_string(),
            });
        }
    }

    if swap.used_mb >= 1024.0 {
        warnings.push(DoctorWarning {
            code: "HIGH_SWAP".to_string(),
            severity: "warn".to_string(),
            message: format!(
                "System swap usage is {:.1} MB / {:.1} MB",
                swap.used_mb, swap.total_mb
            ),
            remediation:
                "Check daemon RSS bloat (`fseventsd`, `mds_stores`) or reduce parallel linker jobs."
                    .to_string(),
        });
    }

    if !leaked_temp_mounts.is_empty() {
        warnings.push(DoctorWarning {
            code: "LEAKED_TEMP_MOUNTS".to_string(),
            severity: "warn".to_string(),
            message: format!(
                "Found {} orphaned temporary DMG/test mount(s) under /var/folders/",
                leaked_temp_mounts.len()
            ),
            remediation: "Detach stale disk images with `hdiutil detach <mountpoint>`.".to_string(),
        });
    }

    if !local_apfs_snapshots.is_empty() {
        warnings.push(DoctorWarning {
            code: "LOCAL_APFS_SNAPSHOTS".to_string(),
            severity: "info".to_string(),
            message: format!(
                "Found {} local APFS snapshot(s) on / which can pin deleted build artifacts",
                local_apfs_snapshots.len()
            ),
            remediation: "Use a Time-Machine-excluded Caisson volume (`sudo caisson init`) for builds and worktrees.".to_string(),
        });
    }

    if caisson_volumes.is_empty() && spotlight_root_enabled {
        warnings.push(DoctorWarning {
            code: "NO_SHIELDED_VOLUME".to_string(),
            severity: "info".to_string(),
            message: "No Caisson-shielded APFS volume detected and Spotlight indexing is enabled on /".to_string(),
            remediation: "Run `sudo caisson init` once to create a shielded `/Volumes/caisson` workspace volume.".to_string(),
        });
    }

    for vol in &caisson_volumes {
        if !vol.shields.all_healthy {
            warnings.push(DoctorWarning {
                code: "DEGRADED_SHIELDS".to_string(),
                severity: "warn".to_string(),
                message: format!(
                    "Volume '{}' ({}) has incomplete daemon shields (noatime={}, spotlight_off={}, fseventsd_no_log={}, tm_excluded={})",
                    vol.name,
                    vol.mount_point,
                    vol.shields.noatime,
                    vol.shields.spotlight_disabled,
                    vol.shields.fseventsd_no_log,
                    vol.shields.tm_excluded
                ),
                remediation: format!("Re-apply shields with `sudo caisson vol shield {}`.", vol.name),
            });
        }
    }

    Ok(DoctorReport {
        macos_version,
        chip,
        ram_gb,
        uptime,
        kern_maxvnodes,
        loadavg,
        swap,
        spotlight_root_status,
        spotlight_root_enabled,
        daemons,
        local_apfs_snapshots,
        leaked_temp_mounts,
        caisson_volumes,
        launch_daemon,
        developer_tools_hint,
        warnings,
    })
}

/// Render a crisp human-readable terminal summary of `DoctorReport`.
pub fn format_doctor_human(report: &DoctorReport) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "System:     macOS {} | {} | {:.1} GB RAM\n",
        report.macos_version, report.chip, report.ram_gb
    ));
    out.push_str(&format!("Uptime:     {}\n", report.uptime));
    out.push_str(&format!(
        "Load avg:   {}\n",
        report
            .loadavg
            .iter()
            .map(|v| format!("{v:.2}"))
            .collect::<Vec<_>>()
            .join(" ")
    ));
    out.push_str(&format!(
        "Vnodes:     kern.maxvnodes = {}\n",
        report.kern_maxvnodes
    ));
    out.push_str(&format!(
        "Swap:       {:.1} MB used / {:.1} MB total ({:.1} MB free)\n",
        report.swap.used_mb, report.swap.total_mb, report.swap.free_mb
    ));
    out.push_str(&format!(
        "Spotlight:  {}\n",
        if report.spotlight_root_status.is_empty() {
            "unknown"
        } else {
            &report.spotlight_root_status
        }
    ));
    out.push_str(&format!(
        "Snapshots:  {} local APFS snapshot(s) on /\n",
        report.local_apfs_snapshots.len()
    ));
    out.push_str(&format!(
        "Temp DMGs:  {} leaked mount(s) under /var/folders/\n",
        report.leaked_temp_mounts.len()
    ));
    out.push_str(&format!(
        "Daemon:     {} (installed={}, loaded={})\n",
        report.launch_daemon.label, report.launch_daemon.installed, report.launch_daemon.loaded
    ));

    out.push_str("\nDaemon Cumulative CPU & RSS:\n");
    let mut sorted: Vec<(&String, &DaemonStats)> = report.daemons.iter().collect();
    sorted.sort_by(|a, b| {
        b.1.cpu_seconds
            .partial_cmp(&a.1.cpu_seconds)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    for (name, s) in sorted {
        let bloat_flag = if name == "fseventsd" && s.rss_mb > FSEVENTS_BLOAT_WARN_MB {
            " [BLOAT]"
        } else {
            ""
        };
        out.push_str(&format!(
            "  {name:<18} {:8.2} min ({:5.2} h) | {:7.1} MB RSS | {} proc(s){bloat_flag}\n",
            s.cpu_minutes,
            s.cpu_minutes / 60.0,
            s.rss_mb,
            s.procs
        ));
    }

    if !report.caisson_volumes.is_empty() {
        out.push_str("\nCaisson Volume Shields:\n");
        for v in &report.caisson_volumes {
            let state = if v.shields.all_healthy {
                "HEALTHY"
            } else {
                "DEGRADED"
            };
            out.push_str(&format!(
                "  {:<18} {} [{}] (noatime={}, spotlight_off={}, fseventsd_no_log={}, tm_excluded={})\n",
                v.name,
                v.mount_point,
                state,
                v.shields.noatime,
                v.shields.spotlight_disabled,
                v.shields.fseventsd_no_log,
                v.shields.tm_excluded
            ));
        }
    }

    if !report.warnings.is_empty() {
        out.push_str("\nFindings:\n");
        for w in &report.warnings {
            out.push_str(&format!(
                "  [{}] {}: {}\n      -> {}\n",
                w.severity.to_ascii_uppercase(),
                w.code,
                w.message,
                w.remediation
            ));
        }
    }

    out.push_str(&format!(
        "\nDeveloper Tools: {}\n",
        report.developer_tools_hint
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ps_time_formats() {
        assert!((parse_ps_seconds("0:12.34") - 12.34).abs() < 1e-6);
        assert!((parse_ps_seconds("15:30.50") - 930.5).abs() < 1e-6);
        assert!((parse_ps_seconds("2:05:10") - 7510.0).abs() < 1e-6);
        assert!((parse_ps_seconds("1-01:00:00") - 90_000.0).abs() < 1e-6);
    }

    #[test]
    fn parses_ps_daemons_and_xprotect_prefix() {
        let sample = "\
  PID  %CPU      TIME    RSS COMM
  101   0.5   1:30.00 102400 /System/Library/Frameworks/CoreServices.framework/mds_stores
  102   0.0  10:00.00 614400 /System/Library/Frameworks/CoreServices.framework/fseventsd
  103   0.0   0:05.00  20480 /Library/Apple/XProtectRemediatorSnowdrift
  104   0.0   0:10.00  10240 /Library/Apple/XProtectPluginService
  105   0.0   0:01.00   4096 /bin/zsh
";
        let stats = parse_ps_daemons(sample);
        assert_eq!(stats["mds_stores"].cpu_seconds, 90.0);
        assert_eq!(stats["mds_stores"].rss_mb, 100.0);
        assert_eq!(stats["fseventsd"].rss_mb, 600.0);
        assert!(stats["fseventsd"].rss_mb > FSEVENTS_BLOAT_WARN_MB);
        assert_eq!(stats["XProtect"].procs, 2);
        assert_eq!(stats["XProtect"].cpu_seconds, 15.0);
        assert!(!stats.contains_key("zsh"));
    }

    #[test]
    fn parses_swap_loadavg_snapshots_and_leaked_mounts() {
        let swap = parse_swap_usage("total = 4096.00M  used = 1.50G  free = 2560.00M  (encrypted)");
        assert_eq!(swap.total_mb, 4096.0);
        assert_eq!(swap.used_mb, 1536.0);
        assert_eq!(swap.free_mb, 2560.0);

        let load = parse_loadavg("{ 2.30 2.06 2.02 }");
        assert_eq!(load, vec![2.30, 2.06, 2.02]);

        let snaps = parse_local_snapshots(
            "Snapshots for disk /:\ncom.apple.TimeMachine.2026-11-26-120000.local\ncom.apple.os.update-abc\n",
        );
        assert_eq!(snaps.len(), 2);

        let leaked = parse_leaked_temp_mounts(
            "/dev/disk3s5 on /System/Volumes/Data (apfs)\n\
             /dev/disk9s1 on /var/folders/xx/yy/T/test-fixture.dmg.mount (apfs)\n\
             /dev/disk10s1 on /Volumes/caisson (apfs, noatime)\n",
        );
        assert_eq!(leaked.len(), 1);
        assert!(leaked[0].contains("test-fixture.dmg.mount"));
    }
}
