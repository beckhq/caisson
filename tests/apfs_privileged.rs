//! Privileged APFS volume lifecycle integration tests.
//!
//! Strictly gated behind `CAISSON_INTEGRATION_TESTS=1` per `AGENTS.md` so default `cargo test`
//! never invokes `diskutil apfs addVolume` or `deleteVolume`.

use caisson::apfs::volume::{create_volume, destroy_volume, CreateVolumeOptions, QuotaSpec};
use caisson::registry::VolumeTier;
use tempfile::tempdir;

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

    assert!(created.uuid.is_some());

    let destroyed = destroy_volume(&vol_name, &reg_path, true, false)
        .expect("triple-checked destroy should succeed for freshly created test volume");
    assert!(destroyed.safety_verified);
}
