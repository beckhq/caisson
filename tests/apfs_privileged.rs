//! Privileged APFS volume lifecycle integration tests.
//!
//! Strictly gated behind `CAISSON_INTEGRATION_TESTS=1` per `AGENTS.md` so default `cargo test`
//! never invokes `diskutil apfs addVolume` or `deleteVolume`.

use caisson::apfs::shield::inspect_shields;
use caisson::apfs::volume::{create_volume, destroy_volume, CreateVolumeOptions, QuotaSpec};
use caisson::registry::VolumeTier;
use std::path::{Path, PathBuf};
use tempfile::tempdir;

/// RAII cleanup guard ensuring any temporary APFS test volume is destroyed even if an assertion
/// panics mid-test.
struct TempApfsVolumeGuard {
    name: String,
    registry_path: PathBuf,
    active: bool,
}

impl TempApfsVolumeGuard {
    fn new(name: String, registry_path: PathBuf) -> Self {
        Self {
            name,
            registry_path,
            active: true,
        }
    }

    fn disarm(&mut self) {
        self.active = false;
    }
}

impl Drop for TempApfsVolumeGuard {
    fn drop(&mut self) {
        if self.active {
            let _ = destroy_volume(&self.name, &self.registry_path, true, false);
        }
    }
}

#[test]
fn privileged_apfs_create_shield_and_destroy_roundtrip() {
    if std::env::var("CAISSON_INTEGRATION_TESTS").as_deref() != Ok("1") {
        // Skipped during unprivileged `cargo test`.
        return;
    }

    let tmp = tempdir().unwrap();
    let reg_path = tmp.path().join("registry.json");
    let suffix = &uuid::Uuid::new_v4().to_string()[..8];
    let vol_name = format!("caisson-test-{suffix}");

    let created = create_volume(
        &CreateVolumeOptions {
            name: vol_name.clone(),
            tier: VolumeTier::Tier2,
            container: None,
            mount_point: None,
            quota: QuotaSpec::Bytes(5 * caisson::units::GIB),
            case_sensitive: false,
            nobrowse: true,
            dry_run: false,
        },
        &reg_path,
    )
    .expect("create_volume should succeed when running with sudo");

    let mut guard = TempApfsVolumeGuard::new(vol_name.clone(), reg_path.clone());

    assert!(created.uuid.is_some());
    let mp = Path::new(&created.mount_point);
    assert!(
        mp.exists(),
        "created volume should be mounted at {}",
        mp.display()
    );

    // Verify live shields on the mounted sibling APFS volume.
    let shields = inspect_shields(mp);
    assert!(shields.mounted, "volume should report mounted");
    assert!(
        shields.metadata_never_index,
        ".metadata_never_index missing"
    );
    assert!(shields.fseventsd_no_log, ".fseventsd/no_log missing");
    assert!(shields.trashes_blocked, ".Trashes blocker missing");
    assert!(shields.noatime, "noatime mount flag missing");
    assert!(shields.nobrowse, "nobrowse mount flag missing");
    assert!(
        shields.spotlight_disabled,
        "Spotlight indexing still enabled"
    );

    // `tmutil addexclusion -v` requires Full Disk Access on macOS; verify either it succeeded
    // or emitted an explicit FDA warning rather than crashing volume creation.
    let tm_step = created
        .shield_report
        .steps
        .iter()
        .find(|s| s.step == "timemachine_exclusion")
        .expect("timemachine_exclusion step should be reported");
    if tm_step.status == "applied" {
        assert!(shields.tm_excluded, "Time Machine exclusion missing");
        assert!(
            shields.all_healthy,
            "expected all shields healthy: {shields:?}"
        );
    } else {
        assert_eq!(tm_step.status, "warning");
        eprintln!(
            "Note: tmutil volume exclusion skipped (Full Disk Access not granted to shell): {:?}",
            tm_step.detail
        );
    }

    let destroyed = destroy_volume(&vol_name, &reg_path, true, false)
        .expect("triple-checked destroy should succeed for freshly created test volume");
    assert!(destroyed.safety_verified);
    guard.disarm();
    assert!(!mp.exists(), "mount point should be gone after destroy");
}
