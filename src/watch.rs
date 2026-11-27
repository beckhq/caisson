use crate::doctor::{parse_loadavg, parse_ps_seconds, parse_swap_usage, SwapUsage};
use crate::error::{CaissonError, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

pub const WATCH_DAEMONS: &[&str] = &[
    "mds_stores",
    "mds",
    "mdworker_shared",
    "fseventsd",
    "syspolicyd",
    "lsd",
    "backupd",
    "XProtect",
];

const PERSISTENT_DAEMONS: &[&str] = &[
    "mds_stores",
    "mds",
    "fseventsd",
    "syspolicyd",
    "lsd",
    "backupd",
];

const BUILD_PROCS: &[&str] = &[
    "ninja",
    "clang",
    "clang++",
    "lld",
    "ld64.lld",
    "ld",
    "rustc",
    "swiftc",
    "cargo",
    "xcodebuild",
];

static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_stop_signal(_sig: libc::c_int) {
    STOP_REQUESTED.store(true, Ordering::SeqCst);
}

fn install_signal_handlers() {
    STOP_REQUESTED.store(false, Ordering::SeqCst);
    unsafe {
        libc::signal(
            libc::SIGINT,
            handle_stop_signal as *const () as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGTERM,
            handle_stop_signal as *const () as libc::sighandler_t,
        );
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WatchDaemonSample {
    pub cpu_s: f64,
    pub rss_mb: f64,
    pub procs: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WatchBuildSample {
    pub procs: usize,
    pub rss_mb: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WatchSwapSample {
    pub total_mb: f64,
    pub used_mb: f64,
    pub free_mb: f64,
}

impl From<SwapUsage> for WatchSwapSample {
    fn from(s: SwapUsage) -> Self {
        Self {
            total_mb: s.total_mb,
            used_mb: s.used_mb,
            free_mb: s.free_mb,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatchSample {
    pub ts_utc: String,
    pub mono_s: f64,
    pub label: String,
    pub screen_locked: bool,
    pub loadavg: Vec<f64>,
    pub swap: WatchSwapSample,
    pub daemons: BTreeMap<String, WatchDaemonSample>,
    pub build: WatchBuildSample,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WatchDaemonSummary {
    pub cpu_delta_s: f64,
    pub rss_start_mb: f64,
    pub rss_peak_mb: f64,
    pub rss_end_mb: f64,
    pub rss_delta_mb: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WatchSwapSummary {
    pub start: f64,
    pub peak: f64,
    pub end: f64,
    pub delta: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatchSummary {
    pub label: String,
    pub started_utc: String,
    pub ended_utc: String,
    pub wall_s: f64,
    pub samples: usize,
    pub screen_locked_samples: usize,
    pub peak_build_procs: usize,
    pub swap_used_mb: WatchSwapSummary,
    pub daemon_total_cpu_s: f64,
    pub daemons: BTreeMap<String, WatchDaemonSummary>,
}

#[derive(Debug, Clone)]
pub struct WatchOptions {
    pub label: String,
    pub out: Option<PathBuf>,
    pub interval_secs: f64,
    pub watch_pid: Option<i32>,
    pub post_settle_secs: f64,
    pub summarize: Option<PathBuf>,
    pub append: bool,
}

/// Check whether `pid` is alive and NOT a zombie (`Z`) process.
pub fn pid_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    let rc = unsafe { libc::kill(pid, 0) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ESRCH) {
            return false;
        }
        // EPERM means process exists (owned by another user); still check ps stat below.
    }

    let output = Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output();
    match output {
        Ok(out) if out.status.success() => {
            let stat = String::from_utf8_lossy(&out.stdout).trim().to_string();
            !stat.is_empty() && !stat.starts_with('Z')
        }
        _ => false,
    }
}

pub fn parse_watch_ps(ps_output: &str) -> (BTreeMap<String, WatchDaemonSample>, WatchBuildSample) {
    let mut daemons: BTreeMap<String, WatchDaemonSample> = WATCH_DAEMONS
        .iter()
        .map(|&d| {
            (
                d.to_string(),
                WatchDaemonSample {
                    cpu_s: 0.0,
                    rss_mb: 0.0,
                    procs: 0,
                },
            )
        })
        .collect();
    let mut build = WatchBuildSample {
        procs: 0,
        rss_mb: 0.0,
    };

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
        let name = Path::new(&comm_joined)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(&comm_joined);

        if BUILD_PROCS.contains(&name) {
            build.procs += 1;
            build.rss_mb += rss_kb / 1024.0;
        }

        for &d in WATCH_DAEMONS {
            if name == d || (d == "XProtect" && name.starts_with("XProtect")) {
                if let Some(entry) = daemons.get_mut(d) {
                    entry.cpu_s += parse_ps_seconds(cputime);
                    entry.rss_mb += rss_kb / 1024.0;
                    entry.procs += 1;
                }
                break;
            }
        }
    }

    for entry in daemons.values_mut() {
        entry.cpu_s = round3(entry.cpu_s);
        entry.rss_mb = round2(entry.rss_mb);
    }
    build.rss_mb = round1(build.rss_mb);
    (daemons, build)
}

fn sample_screen_locked() -> bool {
    Command::new("ioreg")
        .args(["-n", "Root", "-d1"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains("\"CGSSessionScreenIsLocked\"=Yes"))
        .unwrap_or(false)
}

pub fn take_sample(label: &str, start: Instant) -> WatchSample {
    let ps_out = Command::new("ps")
        .args(["-eo", "pid,time,rss,comm"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    let (daemons, build) = parse_watch_ps(&ps_out);

    let swap_raw = Command::new("sysctl")
        .args(["-n", "vm.swapusage"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    let swap = WatchSwapSample::from(parse_swap_usage(&swap_raw));

    let load_raw = Command::new("sysctl")
        .args(["-n", "vm.loadavg"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    let loadavg = parse_loadavg(&load_raw);

    WatchSample {
        ts_utc: Utc::now().to_rfc3339(),
        mono_s: round3(start.elapsed().as_secs_f64()),
        label: label.to_string(),
        screen_locked: sample_screen_locked(),
        loadavg,
        swap,
        daemons,
        build,
    }
}

/// Summarize an existing JSONL telemetry file (handles daemon restarts cleanly).
pub fn summarize_jsonl(jsonl_path: &Path) -> Result<WatchSummary> {
    let raw = fs::read_to_string(jsonl_path)?;
    let mut rows: Vec<WatchSample> = Vec::new();
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        rows.push(serde_json::from_str(trimmed)?);
    }
    summarize_samples(&rows)
}

pub fn summarize_samples(rows: &[WatchSample]) -> Result<WatchSummary> {
    if rows.is_empty() {
        return Err(CaissonError::Message(
            "cannot summarize empty JSONL telemetry file".to_string(),
        ));
    }

    let first = &rows[0];
    let last = &rows[rows.len() - 1];
    let wall_s = round2((last.mono_s - first.mono_s).max(0.0));

    let mut daemon_summary = BTreeMap::new();
    let mut total_cpu_delta = 0.0;

    for &d in WATCH_DAEMONS {
        let first_d = first.daemons.get(d).cloned().unwrap_or(WatchDaemonSample {
            cpu_s: 0.0,
            rss_mb: 0.0,
            procs: 0,
        });
        let last_d = last.daemons.get(d).cloned().unwrap_or(WatchDaemonSample {
            cpu_s: 0.0,
            rss_mb: 0.0,
            procs: 0,
        });

        let mut cpu_delta = 0.0;
        let mut prev_cpu = first_d.cpu_s;
        let mut peak_rss = first_d.rss_mb;

        for r in &rows[1..] {
            let cur = r.daemons.get(d).cloned().unwrap_or(WatchDaemonSample {
                cpu_s: 0.0,
                rss_mb: 0.0,
                procs: 0,
            });
            if cur.cpu_s >= prev_cpu {
                cpu_delta += cur.cpu_s - prev_cpu;
            } else if PERSISTENT_DAEMONS.contains(&d) {
                // Persistent daemon restarted: count its fresh cumulative CPU.
                cpu_delta += cur.cpu_s;
            }
            prev_cpu = cur.cpu_s;
            if cur.rss_mb > peak_rss {
                peak_rss = cur.rss_mb;
            }
        }

        cpu_delta = round3(cpu_delta);
        total_cpu_delta += cpu_delta;

        daemon_summary.insert(
            d.to_string(),
            WatchDaemonSummary {
                cpu_delta_s: cpu_delta,
                rss_start_mb: first_d.rss_mb,
                rss_peak_mb: round2(peak_rss),
                rss_end_mb: last_d.rss_mb,
                rss_delta_mb: round2(last_d.rss_mb - first_d.rss_mb),
            },
        );
    }

    let swap_start = first.swap.used_mb;
    let swap_end = last.swap.used_mb;
    let swap_peak = rows
        .iter()
        .map(|r| r.swap.used_mb)
        .fold(swap_start, f64::max);

    let screen_locked_samples = rows.iter().filter(|r| r.screen_locked).count();
    let peak_build_procs = rows.iter().map(|r| r.build.procs).max().unwrap_or(0);

    Ok(WatchSummary {
        label: first.label.clone(),
        started_utc: first.ts_utc.clone(),
        ended_utc: last.ts_utc.clone(),
        wall_s,
        samples: rows.len(),
        screen_locked_samples,
        peak_build_procs,
        swap_used_mb: WatchSwapSummary {
            start: swap_start,
            peak: swap_peak,
            end: swap_end,
            delta: round1(swap_end - swap_start),
        },
        daemon_total_cpu_s: round3(total_cpu_delta),
        daemons: daemon_summary,
    })
}

/// Compute the paired `.summary.json` path for a given JSONL path, guaranteed distinct from `out_path`.
pub fn summary_path_for(out_path: &Path) -> PathBuf {
    let candidate = out_path.with_extension("summary.json");
    if candidate == out_path {
        let mut s = out_path.as_os_str().to_os_string();
        s.push(".summary.json");
        PathBuf::from(s)
    } else {
        candidate
    }
}

/// Validate finite, bounded sampling durations before touching the filesystem.
fn validate_watch_durations(
    interval_secs: f64,
    post_settle_secs: f64,
) -> Result<(Duration, Duration)> {
    if !interval_secs.is_finite() || !(0.05..=86_400.0).contains(&interval_secs) {
        return Err(CaissonError::Message(format!(
            "invalid `--interval` value '{interval_secs}': must be finite and between 0.05 and 86400 seconds"
        )));
    }
    if !post_settle_secs.is_finite() || !(0.0..=3_600.0).contains(&post_settle_secs) {
        return Err(CaissonError::Message(format!(
            "invalid `--post-settle` value '{post_settle_secs}': must be finite and between 0 and 3600 seconds"
        )));
    }
    Ok((
        Duration::from_secs_f64(interval_secs),
        Duration::from_secs_f64(post_settle_secs),
    ))
}

/// Execute `caisson watch`, either summarizing an existing JSONL or running the live sampler.
pub fn run_watch(opts: &WatchOptions) -> Result<(WatchSummary, Option<PathBuf>)> {
    if let Some(ref existing) = opts.summarize {
        let summary = summarize_jsonl(existing)?;
        return Ok((summary, None));
    }

    let (interval, post_settle) =
        validate_watch_durations(opts.interval_secs, opts.post_settle_secs)?;

    let out_path = opts.out.as_ref().ok_or_else(|| {
        CaissonError::Message(
            "`--out <jsonl>` is required unless `--summarize` is given".to_string(),
        )
    })?;

    if !opts.append {
        if let Ok(meta) = fs::metadata(out_path) {
            if meta.len() > 0 {
                return Err(CaissonError::Message(format!(
                    "output file '{}' already exists and is non-empty; choose a fresh `--out` path or pass `--append` explicitly",
                    out_path.display()
                )));
            }
        }
    }

    if let Some(parent) = out_path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }

    let summary_path = summary_path_for(out_path);
    install_signal_handlers();

    let start = Instant::now();

    {
        let mut fh = OpenOptions::new()
            .create(true)
            .append(true)
            .open(out_path)?;

        // Initial baseline sample
        let s0 = take_sample(&opts.label, start);
        writeln!(fh, "{}", serde_json::to_string(&s0)?)?;
        fh.flush()?;

        while !STOP_REQUESTED.load(Ordering::SeqCst) {
            if let Some(pid) = opts.watch_pid {
                if !pid_alive(pid) {
                    break;
                }
            }

            let deadline = Instant::now()
                .checked_add(interval)
                .unwrap_or_else(Instant::now);
            while !STOP_REQUESTED.load(Ordering::SeqCst) && Instant::now() < deadline {
                if let Some(pid) = opts.watch_pid {
                    if !pid_alive(pid) {
                        break;
                    }
                }
                let remaining = deadline.saturating_duration_since(Instant::now());
                std::thread::sleep(remaining.min(Duration::from_millis(250)));
            }

            if STOP_REQUESTED.load(Ordering::SeqCst) {
                break;
            }
            if let Some(pid) = opts.watch_pid {
                if !pid_alive(pid) {
                    break;
                }
            }

            let sample = take_sample(&opts.label, start);
            writeln!(fh, "{}", serde_json::to_string(&sample)?)?;
            fh.flush()?;
        }

        // Post-settle final sample to capture trailing Spotlight / fseventsd bursts
        if !post_settle.is_zero() {
            std::thread::sleep(post_settle);
        }
        let s_end = take_sample(&opts.label, start);
        writeln!(fh, "{}", serde_json::to_string(&s_end)?)?;
        fh.flush()?;
    }

    let summary = summarize_jsonl(out_path)?;
    fs::write(
        &summary_path,
        format!("{}\n", serde_json::to_string_pretty(&summary)?),
    )?;
    Ok((summary, Some(summary_path)))
}

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

fn round3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    struct SampleSpec {
        mono_s: f64,
        mds_cpu: f64,
        mds_rss: f64,
        fsevents_cpu: f64,
        fsevents_rss: f64,
        swap_used: f64,
        build_procs: usize,
        screen_locked: bool,
    }

    fn make_sample(spec: SampleSpec) -> WatchSample {
        let mut daemons: BTreeMap<String, WatchDaemonSample> = WATCH_DAEMONS
            .iter()
            .map(|&d| {
                (
                    d.to_string(),
                    WatchDaemonSample {
                        cpu_s: 0.0,
                        rss_mb: 0.0,
                        procs: 0,
                    },
                )
            })
            .collect();
        daemons.insert(
            "mds_stores".to_string(),
            WatchDaemonSample {
                cpu_s: spec.mds_cpu,
                rss_mb: spec.mds_rss,
                procs: 1,
            },
        );
        daemons.insert(
            "fseventsd".to_string(),
            WatchDaemonSample {
                cpu_s: spec.fsevents_cpu,
                rss_mb: spec.fsevents_rss,
                procs: 1,
            },
        );

        WatchSample {
            ts_utc: format!("2026-11-26T00:00:{:02}Z", spec.mono_s as u64),
            mono_s: spec.mono_s,
            label: "test-run".to_string(),
            screen_locked: spec.screen_locked,
            loadavg: vec![1.0, 1.0, 1.0],
            swap: WatchSwapSample {
                total_mb: 2048.0,
                used_mb: spec.swap_used,
                free_mb: 2048.0 - spec.swap_used,
            },
            daemons,
            build: WatchBuildSample {
                procs: spec.build_procs,
                rss_mb: spec.build_procs as f64 * 128.0,
            },
        }
    }

    #[test]
    fn summarizes_samples_including_daemon_restart() {
        let rows = vec![
            make_sample(SampleSpec {
                mono_s: 0.0,
                mds_cpu: 10.0,
                mds_rss: 100.0,
                fsevents_cpu: 50.0,
                fsevents_rss: 200.0,
                swap_used: 100.0,
                build_procs: 4,
                screen_locked: false,
            }),
            make_sample(SampleSpec {
                mono_s: 10.0,
                mds_cpu: 12.5,
                mds_rss: 180.0,
                fsevents_cpu: 52.0,
                fsevents_rss: 240.0,
                swap_used: 150.0,
                build_procs: 12,
                screen_locked: true,
            }),
            // Simulate fseventsd restart (cpu drops from 52.0 to 1.5 -> persistent daemon counts +1.5)
            make_sample(SampleSpec {
                mono_s: 20.0,
                mds_cpu: 15.0,
                mds_rss: 140.0,
                fsevents_cpu: 1.5,
                fsevents_rss: 80.0,
                swap_used: 120.0,
                build_procs: 2,
                screen_locked: false,
            }),
        ];

        let summary = summarize_samples(&rows).unwrap();
        assert_eq!(summary.label, "test-run");
        assert_eq!(summary.wall_s, 20.0);
        assert_eq!(summary.samples, 3);
        assert_eq!(summary.screen_locked_samples, 1);
        assert_eq!(summary.peak_build_procs, 12);
        assert_eq!(summary.daemons["mds_stores"].cpu_delta_s, 5.0);
        assert_eq!(summary.daemons["mds_stores"].rss_peak_mb, 180.0);
        assert_eq!(summary.daemons["fseventsd"].cpu_delta_s, 3.5);
        assert_eq!(summary.daemon_total_cpu_s, 8.5);
        assert_eq!(summary.swap_used_mb.peak, 150.0);
        assert_eq!(summary.swap_used_mb.delta, 20.0);
    }

    #[test]
    fn watch_pid_tracks_short_lived_child_cleanly() {
        let dir = tempdir().unwrap();
        let out_path = dir.path().join("telemetry.jsonl");

        // Spawn a short-lived child process (`sleep 0.2`) and watch it.
        let mut child = Command::new("sleep").arg("0.2").spawn().unwrap();
        let pid = child.id() as i32;
        assert!(pid_alive(pid));

        let opts = WatchOptions {
            label: "unit-test".to_string(),
            out: Some(out_path.clone()),
            interval_secs: 0.1,
            watch_pid: Some(pid),
            post_settle_secs: 0.0,
            summarize: None,
            append: false,
        };
        // Reap child in background so it doesn't stay a zombie forever, OR note `pid_alive` handles zombies!
        let reaper = std::thread::spawn(move || {
            let _ = child.wait();
        });

        let (summary, summary_path) = run_watch(&opts).unwrap();
        reaper.join().unwrap();

        assert!(summary.samples >= 2);
        assert_eq!(summary.label, "unit-test");
        let sp = summary_path.unwrap();
        assert!(sp.exists());
        assert!(out_path.exists());

        // Reusing a non-empty output file without `--append` MUST fail cleanly.
        assert!(run_watch(&opts).is_err());
    }

    #[test]
    fn watch_rejects_non_finite_durations_and_keeps_summary_path_distinct() {
        assert!(validate_watch_durations(f64::INFINITY, 1.0).is_err());
        assert!(validate_watch_durations(f64::NAN, 1.0).is_err());
        assert!(validate_watch_durations(1.0, f64::INFINITY).is_err());
        assert!(validate_watch_durations(1.0, -1.0).is_err());

        let same_ext = Path::new("/tmp/run.summary.json");
        assert_ne!(summary_path_for(same_ext), same_ext);
    }

    #[test]
    fn pid_alive_returns_false_for_nonexistent_pid() {
        assert!(!pid_alive(999_999));
        assert!(!pid_alive(-1));
        assert!(pid_alive(std::process::id() as i32));
    }
}
