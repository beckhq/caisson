use crate::apfs::clonefile::clone_path;
use crate::apfs::shield::{inspect_shields, ShieldStatus};
use crate::apfs::statfs::statfs_path;
use crate::config::CaissonConfig;
use crate::doctor::{probe_syspolicyd_exec_latency, SyspolicydProbe};
use crate::error::{CaissonError, Result};
use crate::tui::{bar_meter, format_banner, Palette};
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

#[derive(Debug, Clone)]
pub struct BenchOptions {
    pub files: usize,
    pub target_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BenchReport {
    pub target_dir: String,
    pub volume_mount: Option<String>,
    pub filesystem: Option<String>,
    pub shielded: bool,
    pub shields: Option<ShieldStatus>,
    pub file_count: usize,
    pub synthetic_bytes: u64,
    pub cow_clone_ms: f64,
    pub regular_copy_ms: f64,
    pub cow_speedup: f64,
    pub trash_rename_ms: f64,
    pub sync_rm_ms: f64,
    pub teardown_speedup: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub syspolicyd_probe: Option<SyspolicydProbe>,
}

struct BenchSandboxGuard(PathBuf);

impl Drop for BenchSandboxGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

fn copy_dir_recursive_bytes(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let dst_path = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_recursive_bytes(&entry.path(), &dst_path)?;
        } else if ty.is_file() {
            let data = fs::read(entry.path())?;
            fs::write(&dst_path, data)?;
        }
    }
    Ok(())
}

/// Run a self-contained, unprivileged APFS CoW + teardown + `syspolicyd` micro-benchmark.
pub fn run_bench(config: &CaissonConfig, opts: &BenchOptions) -> Result<BenchReport> {
    if !(10..=20_000).contains(&opts.files) {
        return Err(CaissonError::Message(
            "`--files` must be between 10 and 20000".to_string(),
        ));
    }

    let base_dir = match opts.target_dir {
        Some(ref p) => p.clone(),
        None => {
            let root = config.resolve_root();
            if root.is_dir() {
                root
            } else {
                std::env::temp_dir()
            }
        }
    };

    let meta = fs::symlink_metadata(&base_dir).map_err(|_| {
        CaissonError::Message(format!(
            "benchmark target directory '{}' does not exist",
            base_dir.display()
        ))
    })?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err(CaissonError::Message(format!(
            "benchmark target '{}' must be an existing non-symlink directory",
            base_dir.display()
        )));
    }

    let canonical_base = fs::canonicalize(&base_dir)?;
    let stat = statfs_path(&canonical_base).ok();
    let volume_mount = stat.as_ref().map(|s| s.mount_point.clone());
    let filesystem = stat.as_ref().map(|s| s.fs_type.clone());
    let shields = volume_mount
        .as_deref()
        .filter(|mp| *mp != "/")
        .map(|mp| inspect_shields(Path::new(mp)));
    let shielded = shields.as_ref().map(|s| s.all_healthy).unwrap_or(false);

    // Create a unique temporary sandbox inside the target directory with unconditional RAII cleanup.
    let sandbox_dir = canonical_base.join(format!(".caisson-bench-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&sandbox_dir)?;
    let _guard = BenchSandboxGuard(sandbox_dir.clone());

    // Build a realistic nested package tree (`pkg-000/file-00.js`, 4 KiB each).
    let donor_dir = sandbox_dir.join("donor-tree");
    fs::create_dir(&donor_dir)?;
    let payload = vec![b'x'; 4096];
    let pkgs = ((opts.files as f64) / 10.0).ceil().max(1.0) as usize;
    let mut created = 0usize;
    for p in 0..pkgs {
        let pkg_dir = donor_dir.join(format!("pkg-{p:04}"));
        fs::create_dir(&pkg_dir)?;
        for f in 0..10 {
            if created >= opts.files {
                break;
            }
            fs::write(pkg_dir.join(format!("mod-{f:02}.js")), &payload)?;
            created += 1;
        }
    }
    let synthetic_bytes = (created as u64) * (payload.len() as u64);

    // 1. APFS CoW clone (`clonefile(2)`) vs byte-for-byte recursive copy.
    let cow_dst = sandbox_dir.join("cow-clone");
    let t_cow = Instant::now();
    clone_path(&donor_dir, &cow_dst)?;
    let cow_clone_ms = t_cow.elapsed().as_secs_f64() * 1000.0;

    let copy_dst = sandbox_dir.join("regular-copy");
    let t_copy = Instant::now();
    copy_dir_recursive_bytes(&donor_dir, &copy_dst)?;
    let regular_copy_ms = t_copy.elapsed().as_secs_f64() * 1000.0;

    let cow_speedup = if cow_clone_ms > 0.001 {
        regular_copy_ms / cow_clone_ms
    } else {
        regular_copy_ms / 0.001
    };

    // 2. Interactive atomic rename-to-trash teardown vs synchronous `rm -rf`.
    let trash_dir = sandbox_dir.join(".trash");
    fs::create_dir(&trash_dir)?;
    let trash_dst = trash_dir.join("cow-clone");
    let t_rename = Instant::now();
    fs::rename(&cow_dst, &trash_dst)?;
    let trash_rename_ms = t_rename.elapsed().as_secs_f64() * 1000.0;

    let t_rm = Instant::now();
    fs::remove_dir_all(&copy_dst)?;
    let sync_rm_ms = t_rm.elapsed().as_secs_f64() * 1000.0;

    let teardown_speedup = if trash_rename_ms > 0.001 {
        sync_rm_ms / trash_rename_ms
    } else {
        sync_rm_ms / 0.001
    };

    // 3. Live 2-binary `syspolicyd` first-exec latency probe.
    let syspolicyd_probe = probe_syspolicyd_exec_latency();

    Ok(BenchReport {
        target_dir: canonical_base.display().to_string(),
        volume_mount,
        filesystem,
        shielded,
        shields,
        file_count: created,
        synthetic_bytes,
        cow_clone_ms: round2(cow_clone_ms),
        regular_copy_ms: round2(regular_copy_ms),
        cow_speedup: round2(cow_speedup),
        trash_rename_ms: round2(trash_rename_ms),
        sync_rm_ms: round2(sync_rm_ms),
        teardown_speedup: round2(teardown_speedup),
        syspolicyd_probe,
    })
}

pub fn format_bench_human(r: &BenchReport) -> String {
    let pal = Palette::auto();
    let mut out = String::new();
    out.push_str(&format_banner(
        "CAISSON",
        "APFS & Exec Micro-Benchmark",
        &format!("{} files", r.file_count),
        &pal,
    ));
    out.push('\n');

    let shield_badge = if r.shielded {
        pal.ok("[SHIELDED]")
    } else {
        pal.warn("[UNSHIELDED]")
    };
    out.push_str(&format!(
        "  Target     : {} {} ({})\n",
        r.target_dir,
        shield_badge,
        r.filesystem.as_deref().unwrap_or("unknown")
    ));
    out.push_str(&format!(
        "  Workload   : {} files ({:.1} MiB synthetic package tree)\n\n",
        r.file_count,
        r.synthetic_bytes as f64 / (1024.0 * 1024.0)
    ));

    let cow_ratio = (r.cow_clone_ms / r.regular_copy_ms.max(0.01)).clamp(0.02, 1.0);
    out.push_str(&format!(
        "  {}\n",
        pal.bold("1. Tree Cloning (clonefile CoW vs byte copy)")
    ));
    out.push_str(&format!(
        "     APFS CoW   [{}] {:>7.2} ms  {}\n",
        pal.ok(&bar_meter(cow_ratio, 20)),
        r.cow_clone_ms,
        pal.ok(&format!("({:.1}x faster)", r.cow_speedup))
    ));
    out.push_str(&format!(
        "     Byte copy  [{}] {:>7.2} ms\n\n",
        pal.dim(&bar_meter(1.0, 20)),
        r.regular_copy_ms
    ));

    let rm_ratio = (r.trash_rename_ms / r.sync_rm_ms.max(0.01)).clamp(0.02, 1.0);
    out.push_str(&format!(
        "  {}\n",
        pal.bold("2. Teardown Primitive (rename to trash vs rm -rf)")
    ));
    out.push_str(&format!(
        "     rename     [{}] {:>7.2} ms  {}\n",
        pal.ok(&bar_meter(rm_ratio, 20)),
        r.trash_rename_ms,
        pal.ok(&format!("({:.1}x faster)", r.teardown_speedup))
    ));
    out.push_str(&format!(
        "     rm -rf     [{}] {:>7.2} ms\n",
        pal.dim(&bar_meter(1.0, 20)),
        r.sync_rm_ms
    ));

    if let Some(ref p) = r.syspolicyd_probe {
        out.push('\n');
        out.push_str(&format!(
            "  {}\n",
            pal.bold("3. Fresh Local Binary Exec Latency (syspolicyd / Developer Tools)")
        ));
        let max_ms = p.first_exec_ms.max(p.cached_exec_ms).max(1.0);
        let first_bar = bar_meter(p.first_exec_ms / max_ms, 20);
        let cached_bar = bar_meter(p.cached_exec_ms / max_ms, 20);
        let status = if p.stalled {
            pal.warn(&format!("[STALLED +{:.1} ms]", p.overhead_ms))
        } else {
            pal.ok("[FAST / EXEMPT]")
        };
        out.push_str(&format!(
            "     1st exec   [{}] {:>7.1} ms  {}\n",
            if p.stalled {
                pal.warn(&first_bar)
            } else {
                pal.ok(&first_bar)
            },
            p.first_exec_ms,
            status
        ));
        out.push_str(&format!(
            "     Cached     [{}] {:>7.1} ms\n",
            pal.ok(&cached_bar),
            p.cached_exec_ms
        ));
        if let Some(ref proc_name) = p.responsible_process {
            out.push_str(&format!(
                "     Ancestor   {} (enable Developer Tools in System Settings)\n",
                pal.dim(proc_name)
            ));
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn bench_runs_and_cleans_up_sandbox() {
        let dir = tempdir().unwrap();
        let cfg = CaissonConfig::default();
        let report = run_bench(
            &cfg,
            &BenchOptions {
                files: 20,
                target_dir: Some(dir.path().to_path_buf()),
            },
        )
        .unwrap();
        assert_eq!(report.file_count, 20);
        assert!(report.cow_clone_ms >= 0.0);
        assert!(report.regular_copy_ms >= 0.0);
        assert!(report.trash_rename_ms >= 0.0);
        assert!(report.sync_rm_ms >= 0.0);
        // Sandbox directory must be completely cleaned up by Drop guard.
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn bench_rejects_out_of_range_file_counts() {
        let dir = tempdir().unwrap();
        let cfg = CaissonConfig::default();
        assert!(run_bench(
            &cfg,
            &BenchOptions {
                files: 5,
                target_dir: Some(dir.path().to_path_buf()),
            }
        )
        .is_err());
        assert!(run_bench(
            &cfg,
            &BenchOptions {
                files: 50_000,
                target_dir: Some(dir.path().to_path_buf()),
            }
        )
        .is_err());
    }
}
