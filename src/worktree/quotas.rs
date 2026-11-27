use crate::apfs::statfs::statfs_path;
use crate::config::CaissonConfig;
use crate::error::{CaissonError, QuotaViolation, Result};
use crate::units::format_bytes;
use crate::worktree::trash::allocated_dir_bytes;
use std::fs;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct QuotaSnapshot {
    pub repo_worktrees: usize,
    pub total_worktrees: usize,
    pub reclaimable_worktrees: usize,
    pub caisson_used_bytes: u64,
    pub host_available_bytes: u64,
}

#[derive(Debug, Clone, Default)]
pub struct QuotaCheckOutcome {
    pub warnings: Vec<String>,
}

/// Measure live usage for `<caisson_root>`.
///
/// When `<caisson_root>` is its own dedicated APFS volume mountpoint (e.g. `/Volumes/caisson`),
/// `statfs(2)` gives O(1) instant real APFS usage. When `CAISSON_ROOT` points to a subdirectory
/// inside a larger volume (e.g. during unprivileged testing), sums allocated bytes under
/// `<caisson_root>/{worktrees,cache,.trash}`.
pub fn measure_root_usage(caisson_root: &Path) -> Result<(u64, u64)> {
    let st = statfs_path(caisson_root)?;
    let canonical_root =
        fs::canonicalize(caisson_root).unwrap_or_else(|_| caisson_root.to_path_buf());
    let canonical_mount = fs::canonicalize(&st.mount_point)
        .unwrap_or_else(|_| Path::new(&st.mount_point).to_path_buf());

    let caisson_used = if canonical_root == canonical_mount {
        st.used_bytes
    } else {
        allocated_dir_bytes(&caisson_root.join("worktrees"))
            .saturating_add(allocated_dir_bytes(&caisson_root.join("cache")))
            .saturating_add(allocated_dir_bytes(&caisson_root.join(".trash")))
    };

    Ok((caisson_used, st.available_bytes))
}

/// Enforce Layer-B soft quotas (`ARCHITECTURE.md` §3) prior to `caisson wt add`.
///
/// - Emits warnings at >=80% utilization.
/// - Refuses with structured `CaissonError::QuotaExceeded` at >=100% unless `force` is true.
pub fn evaluate_add_quotas(
    config: &CaissonConfig,
    snapshot: &QuotaSnapshot,
    force: bool,
) -> Result<QuotaCheckOutcome> {
    let mut warnings = Vec::new();
    let suggested_command = format!(
        "caisson wt prune --inactive {} --apply",
        config.prune.default_inactive
    );

    let min_host_free = config.min_host_free_bytes()?;
    let soft_disk_limit = config.soft_disk_limit_bytes()?;

    // 1. min_host_free (host container free space blast shield)
    if min_host_free > 0 && snapshot.host_available_bytes < min_host_free {
        let msg = format!(
            "host container free space ({}) is below `quotas.min_host_free` ({})",
            format_bytes(snapshot.host_available_bytes),
            format_bytes(min_host_free)
        );
        if force {
            warnings.push(format!("WARNING (--force): {msg}"));
        } else {
            return Err(CaissonError::QuotaExceeded {
                rule: "min_host_free".to_string(),
                message: msg.clone(),
                violation: Box::new(QuotaViolation {
                    rule: "min_host_free".to_string(),
                    message: msg,
                    current: format_bytes(snapshot.host_available_bytes),
                    limit: format_bytes(min_host_free),
                    reclaimable_worktrees: snapshot.reclaimable_worktrees,
                    suggested_command,
                }),
            });
        }
    }

    // 2. soft_disk_limit
    if soft_disk_limit > 0 {
        if snapshot.caisson_used_bytes >= soft_disk_limit {
            let msg = format!(
                "caisson workspace disk usage ({}) reached `quotas.soft_disk_limit` ({})",
                format_bytes(snapshot.caisson_used_bytes),
                format_bytes(soft_disk_limit)
            );
            if force {
                warnings.push(format!("WARNING (--force): {msg}"));
            } else {
                return Err(CaissonError::QuotaExceeded {
                    rule: "soft_disk_limit".to_string(),
                    message: msg.clone(),
                    violation: Box::new(QuotaViolation {
                        rule: "soft_disk_limit".to_string(),
                        message: msg,
                        current: format_bytes(snapshot.caisson_used_bytes),
                        limit: format_bytes(soft_disk_limit),
                        reclaimable_worktrees: snapshot.reclaimable_worktrees,
                        suggested_command,
                    }),
                });
            }
        } else if (snapshot.caisson_used_bytes as u128) * 100 >= (soft_disk_limit as u128) * 80 {
            warnings.push(format!(
                "approaching `quotas.soft_disk_limit`: {} / {} used",
                format_bytes(snapshot.caisson_used_bytes),
                format_bytes(soft_disk_limit)
            ));
        }
    }

    // 3. max_worktrees_per_repo
    let max_per_repo = config.quotas.max_worktrees_per_repo;
    if max_per_repo > 0 {
        if snapshot.repo_worktrees >= max_per_repo {
            let msg = format!(
                "repository has {} worktrees (limit `quotas.max_worktrees_per_repo` = {})",
                snapshot.repo_worktrees, max_per_repo
            );
            if force {
                warnings.push(format!("WARNING (--force): {msg}"));
            } else {
                return Err(CaissonError::QuotaExceeded {
                    rule: "max_worktrees_per_repo".to_string(),
                    message: msg.clone(),
                    violation: Box::new(QuotaViolation {
                        rule: "max_worktrees_per_repo".to_string(),
                        message: msg,
                        current: snapshot.repo_worktrees.to_string(),
                        limit: max_per_repo.to_string(),
                        reclaimable_worktrees: snapshot.reclaimable_worktrees,
                        suggested_command,
                    }),
                });
            }
        } else if snapshot.repo_worktrees * 100 >= max_per_repo * 80 {
            warnings.push(format!(
                "approaching `quotas.max_worktrees_per_repo`: {}/{} worktrees",
                snapshot.repo_worktrees, max_per_repo
            ));
        }
    }

    // 4. max_worktrees_total
    let max_total = config.quotas.max_worktrees_total;
    if max_total > 0 {
        if snapshot.total_worktrees >= max_total {
            let msg = format!(
                "global worktree count is {} (limit `quotas.max_worktrees_total` = {})",
                snapshot.total_worktrees, max_total
            );
            if force {
                warnings.push(format!("WARNING (--force): {msg}"));
            } else {
                return Err(CaissonError::QuotaExceeded {
                    rule: "max_worktrees_total".to_string(),
                    message: msg.clone(),
                    violation: Box::new(QuotaViolation {
                        rule: "max_worktrees_total".to_string(),
                        message: msg,
                        current: snapshot.total_worktrees.to_string(),
                        limit: max_total.to_string(),
                        reclaimable_worktrees: snapshot.reclaimable_worktrees,
                        suggested_command,
                    }),
                });
            }
        } else if snapshot.total_worktrees * 100 >= max_total * 80 {
            warnings.push(format!(
                "approaching `quotas.max_worktrees_total`: {}/{} worktrees",
                snapshot.total_worktrees, max_total
            ));
        }
    }

    Ok(QuotaCheckOutcome { warnings })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::units::GIB;

    #[test]
    fn warns_at_80_percent_and_blocks_at_100_percent() {
        let cfg = CaissonConfig::default(); // 25 per repo, 50 total, 250G soft, 50G free

        // 80% per repo -> warning, allowed
        let warn_snap = QuotaSnapshot {
            repo_worktrees: 20,
            total_worktrees: 20,
            reclaimable_worktrees: 3,
            caisson_used_bytes: 10 * GIB,
            host_available_bytes: 200 * GIB,
        };
        let outcome = evaluate_add_quotas(&cfg, &warn_snap, false).unwrap();
        assert_eq!(outcome.warnings.len(), 1);
        assert!(outcome.warnings[0].contains("max_worktrees_per_repo"));

        // 100% per repo -> QuotaExceeded with reclaimable count + hint
        let block_snap = QuotaSnapshot {
            repo_worktrees: 25,
            total_worktrees: 25,
            reclaimable_worktrees: 7,
            caisson_used_bytes: 10 * GIB,
            host_available_bytes: 200 * GIB,
        };
        let err = evaluate_add_quotas(&cfg, &block_snap, false).unwrap_err();
        match err {
            CaissonError::QuotaExceeded {
                rule, violation, ..
            } => {
                assert_eq!(rule, "max_worktrees_per_repo");
                assert_eq!(violation.reclaimable_worktrees, 7);
                assert!(violation.suggested_command.contains("caisson wt prune"));
            }
            other => panic!("unexpected error: {other:?}"),
        }

        // With --force, bypasses and emits warning
        let forced = evaluate_add_quotas(&cfg, &block_snap, true).unwrap();
        assert!(!forced.warnings.is_empty());
    }

    #[test]
    fn enforces_min_host_free_and_soft_disk_limit() {
        let cfg = CaissonConfig::default();

        let low_free = QuotaSnapshot {
            repo_worktrees: 1,
            total_worktrees: 1,
            reclaimable_worktrees: 0,
            caisson_used_bytes: 10 * GIB,
            host_available_bytes: 20 * GIB, // < 50G
        };
        let err = evaluate_add_quotas(&cfg, &low_free, false).unwrap_err();
        assert!(matches!(
            err,
            CaissonError::QuotaExceeded { ref rule, .. } if rule == "min_host_free"
        ));

        let over_soft = QuotaSnapshot {
            repo_worktrees: 1,
            total_worktrees: 1,
            reclaimable_worktrees: 2,
            caisson_used_bytes: 260 * GIB, // > 250G
            host_available_bytes: 200 * GIB,
        };
        let err = evaluate_add_quotas(&cfg, &over_soft, false).unwrap_err();
        assert!(matches!(
            err,
            CaissonError::QuotaExceeded { ref rule, .. } if rule == "soft_disk_limit"
        ));
    }
}
