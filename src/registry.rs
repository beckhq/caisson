use crate::error::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

pub const DEFAULT_REGISTRY_PATH: &str = "/Library/Application Support/caisson/registry.json";
pub const VOLUME_MARKER_FILENAME: &str = ".caisson-volume.json";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum VolumeTier {
    Tier1,
    Tier2,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RegisteredVolume {
    pub name: String,
    pub uuid: String,
    pub container: String,
    pub mount_point: String,
    pub tier: VolumeTier,
    pub case_sensitive: bool,
    pub nobrowse: bool,
    pub quota_bytes: Option<u64>,
    pub owner: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// On-volume ownership marker written at `<mountpoint>/.caisson-volume.json`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VolumeMarker {
    pub schema_version: u32,
    pub managed_by: String,
    pub name: String,
    pub uuid: String,
    pub container: String,
    pub tier: VolumeTier,
    pub created_at: DateTime<Utc>,
}

impl VolumeMarker {
    pub fn from_registered(vol: &RegisteredVolume) -> Self {
        Self {
            schema_version: 1,
            managed_by: "caisson".to_string(),
            name: vol.name.clone(),
            uuid: vol.uuid.clone(),
            container: vol.container.clone(),
            tier: vol.tier,
            created_at: vol.created_at,
        }
    }

    pub fn path_for(mount_point: &Path) -> PathBuf {
        mount_point.join(VOLUME_MARKER_FILENAME)
    }

    pub fn read_from(mount_point: &Path) -> Result<Self> {
        let path = Self::path_for(mount_point);
        let raw = fs::read_to_string(&path)?;
        let marker: Self = serde_json::from_str(&raw)?;
        Ok(marker)
    }

    pub fn write_to(&self, mount_point: &Path) -> Result<PathBuf> {
        let path = Self::path_for(mount_point);
        let json = serde_json::to_string_pretty(self)?;
        fs::write(&path, format!("{json}\n"))?;
        Ok(path)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VolumeRegistry {
    pub schema_version: u32,
    pub volumes: Vec<RegisteredVolume>,
}

impl Default for VolumeRegistry {
    fn default() -> Self {
        Self {
            schema_version: 1,
            volumes: Vec::new(),
        }
    }
}

impl VolumeRegistry {
    pub fn default_path() -> PathBuf {
        if let Ok(override_path) = std::env::var("CAISSON_REGISTRY") {
            if !override_path.trim().is_empty() {
                return PathBuf::from(override_path);
            }
        }
        PathBuf::from(DEFAULT_REGISTRY_PATH)
    }

    pub fn load_default() -> Result<Self> {
        Self::load_from(&Self::default_path())
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = fs::read_to_string(path)?;
        let reg: Self = serde_json::from_str(&raw)?;
        Ok(reg)
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self)?;
        // Atomic write via sibling temp file + rename
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, format!("{json}\n"))?;
        fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn find_by_name(&self, name: &str) -> Option<&RegisteredVolume> {
        self.volumes.iter().find(|v| v.name == name)
    }

    pub fn find_by_uuid(&self, uuid: &str) -> Option<&RegisteredVolume> {
        self.volumes
            .iter()
            .find(|v| v.uuid.eq_ignore_ascii_case(uuid))
    }

    pub fn upsert(&mut self, vol: RegisteredVolume) {
        if let Some(existing) = self
            .volumes
            .iter_mut()
            .find(|v| v.uuid.eq_ignore_ascii_case(&vol.uuid) || v.name == vol.name)
        {
            *existing = vol;
        } else {
            self.volumes.push(vol);
        }
    }

    pub fn remove_by_uuid(&mut self, uuid: &str) -> Option<RegisteredVolume> {
        if let Some(pos) = self
            .volumes
            .iter()
            .position(|v| v.uuid.eq_ignore_ascii_case(uuid))
        {
            Some(self.volumes.remove(pos))
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn registry_and_marker_roundtrip() {
        let dir = tempdir().unwrap();
        let reg_path = dir.path().join("registry.json");
        let mount_dir = dir.path().join("vol");
        fs::create_dir_all(&mount_dir).unwrap();

        let vol = RegisteredVolume {
            name: "caisson".to_string(),
            uuid: "11111111-2222-3333-4444-555555555555".to_string(),
            container: "disk3".to_string(),
            mount_point: mount_dir.display().to_string(),
            tier: VolumeTier::Tier1,
            case_sensitive: false,
            nobrowse: false,
            quota_bytes: Some(250 * 1024 * 1024 * 1024),
            owner: Some("dev".to_string()),
            created_at: Utc::now(),
        };

        let mut reg = VolumeRegistry::default();
        reg.upsert(vol.clone());
        reg.save_to(&reg_path).unwrap();

        let loaded = VolumeRegistry::load_from(&reg_path).unwrap();
        assert_eq!(loaded.volumes.len(), 1);
        assert_eq!(loaded.find_by_name("caisson").unwrap().uuid, vol.uuid);
        assert!(loaded
            .find_by_uuid("11111111-2222-3333-4444-555555555555")
            .is_some());

        let marker = VolumeMarker::from_registered(&vol);
        marker.write_to(&mount_dir).unwrap();
        let read_marker = VolumeMarker::read_from(&mount_dir).unwrap();
        assert_eq!(read_marker.uuid, vol.uuid);
        assert_eq!(read_marker.managed_by, "caisson");
    }
}
