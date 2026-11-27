use crate::error::{CaissonError, Result};
use crate::units::{parse_duration, parse_size};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const DEFAULT_TIER1_VOLUME_NAME: &str = "caisson";
pub const DEFAULT_TIER1_MOUNT: &str = "/Volumes/caisson";
pub const REPO_CONFIG_FILENAME: &str = ".caisson.toml";

/// User-level configuration stored in `~/.config/caisson/config.toml`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct CaissonConfig {
    pub volume_root: Option<String>,
    pub quotas: QuotaConfig,
    pub prune: PruneConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct QuotaConfig {
    pub max_worktrees_per_repo: usize,
    pub max_worktrees_total: usize,
    pub soft_disk_limit: String,
    pub min_host_free: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct PruneConfig {
    pub default_inactive: String,
    pub protect_dirty: bool,
    pub protect_unpushed: bool,
}

impl Default for QuotaConfig {
    fn default() -> Self {
        Self {
            max_worktrees_per_repo: 25,
            max_worktrees_total: 50,
            soft_disk_limit: "250G".to_string(),
            min_host_free: "50G".to_string(),
        }
    }
}

impl Default for PruneConfig {
    fn default() -> Self {
        Self {
            default_inactive: "7d".to_string(),
            protect_dirty: true,
            protect_unpushed: true,
        }
    }
}

/// Optional repo-level configuration stored in `<repo>/.caisson.toml`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct RepoConfig {
    pub worktree: RepoWorktreeConfig,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct RepoWorktreeConfig {
    pub cow_copy: Vec<String>,
    pub post_create: Option<String>,
}

impl CaissonConfig {
    /// Resolve the default config path (`$CAISSON_CONFIG` or `~/.config/caisson/config.toml`).
    pub fn default_path() -> PathBuf {
        if let Ok(override_path) = std::env::var("CAISSON_CONFIG") {
            if !override_path.trim().is_empty() {
                return PathBuf::from(override_path);
            }
        }
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
        PathBuf::from(home)
            .join(".config")
            .join("caisson")
            .join("config.toml")
    }

    /// Resolve the active Tier-1 caisson root directory.
    /// Priority: `CAISSON_ROOT` env var > `config.volume_root` > `/Volumes/caisson`.
    pub fn resolve_root(&self) -> PathBuf {
        if let Ok(env_root) = std::env::var("CAISSON_ROOT") {
            if !env_root.trim().is_empty() {
                return PathBuf::from(env_root);
            }
        }
        if let Some(ref root) = self.volume_root {
            if !root.trim().is_empty() {
                return PathBuf::from(root);
            }
        }
        PathBuf::from(DEFAULT_TIER1_MOUNT)
    }

    pub fn load_default() -> Result<Self> {
        Self::load_from(&Self::default_path())
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = fs::read_to_string(path)?;
        let cfg: Self = toml::from_str(&content)?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        self.validate()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let rendered = toml::to_string_pretty(self)?;
        fs::write(path, rendered)?;
        Ok(())
    }

    pub fn validate(&self) -> Result<()> {
        let _ = self.soft_disk_limit_bytes()?;
        let _ = self.min_host_free_bytes()?;
        let _ = self.default_inactive_duration()?;
        Ok(())
    }

    pub fn soft_disk_limit_bytes(&self) -> Result<u64> {
        parse_size(&self.quotas.soft_disk_limit)
    }

    pub fn min_host_free_bytes(&self) -> Result<u64> {
        parse_size(&self.quotas.min_host_free)
    }

    pub fn default_inactive_duration(&self) -> Result<Duration> {
        parse_duration(&self.prune.default_inactive)
    }

    /// Read a dotted config key (e.g. `quotas.soft_disk_limit`).
    pub fn get_key(&self, key: &str) -> Result<String> {
        match key {
            "volume_root" => Ok(self
                .volume_root
                .clone()
                .unwrap_or_else(|| DEFAULT_TIER1_MOUNT.to_string())),
            "quotas.max_worktrees_per_repo" => Ok(self.quotas.max_worktrees_per_repo.to_string()),
            "quotas.max_worktrees_total" => Ok(self.quotas.max_worktrees_total.to_string()),
            "quotas.soft_disk_limit" => Ok(self.quotas.soft_disk_limit.clone()),
            "quotas.min_host_free" => Ok(self.quotas.min_host_free.clone()),
            "prune.default_inactive" => Ok(self.prune.default_inactive.clone()),
            "prune.protect_dirty" => Ok(self.prune.protect_dirty.to_string()),
            "prune.protect_unpushed" => Ok(self.prune.protect_unpushed.to_string()),
            _ => Err(CaissonError::InvalidConfigKey(key.to_string())),
        }
    }

    /// Update a dotted config key with validation.
    pub fn set_key(&mut self, key: &str, value: &str) -> Result<()> {
        match key {
            "volume_root" => {
                if value.trim().is_empty() {
                    self.volume_root = None;
                } else {
                    self.volume_root = Some(value.trim().to_string());
                }
            }
            "quotas.max_worktrees_per_repo" => {
                let n: usize = value
                    .parse()
                    .map_err(|_| CaissonError::InvalidConfigValue {
                        key: key.to_string(),
                        reason: "expected non-negative integer".to_string(),
                    })?;
                self.quotas.max_worktrees_per_repo = n;
            }
            "quotas.max_worktrees_total" => {
                let n: usize = value
                    .parse()
                    .map_err(|_| CaissonError::InvalidConfigValue {
                        key: key.to_string(),
                        reason: "expected non-negative integer".to_string(),
                    })?;
                self.quotas.max_worktrees_total = n;
            }
            "quotas.soft_disk_limit" => {
                let _ = parse_size(value).map_err(|_| CaissonError::InvalidConfigValue {
                    key: key.to_string(),
                    reason: "expected size like 250G, 500G, 1T".to_string(),
                })?;
                self.quotas.soft_disk_limit = value.trim().to_string();
            }
            "quotas.min_host_free" => {
                let _ = parse_size(value).map_err(|_| CaissonError::InvalidConfigValue {
                    key: key.to_string(),
                    reason: "expected size like 50G, 100G".to_string(),
                })?;
                self.quotas.min_host_free = value.trim().to_string();
            }
            "prune.default_inactive" => {
                let _ = parse_duration(value).map_err(|_| CaissonError::InvalidConfigValue {
                    key: key.to_string(),
                    reason: "expected duration like 7d, 48h".to_string(),
                })?;
                self.prune.default_inactive = value.trim().to_string();
            }
            "prune.protect_dirty" => {
                let b: bool = value
                    .parse()
                    .map_err(|_| CaissonError::InvalidConfigValue {
                        key: key.to_string(),
                        reason: "expected true or false".to_string(),
                    })?;
                self.prune.protect_dirty = b;
            }
            "prune.protect_unpushed" => {
                let b: bool = value
                    .parse()
                    .map_err(|_| CaissonError::InvalidConfigValue {
                        key: key.to_string(),
                        reason: "expected true or false".to_string(),
                    })?;
                self.prune.protect_unpushed = b;
            }
            _ => return Err(CaissonError::InvalidConfigKey(key.to_string())),
        }
        Ok(())
    }
}

impl RepoConfig {
    pub fn load_from_repo(repo_root: &Path) -> Result<Self> {
        let path = repo_root.join(REPO_CONFIG_FILENAME);
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = fs::read_to_string(&path)?;
        let cfg: Self = toml::from_str(&content)?;
        Ok(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn config_roundtrip_and_set_get() {
        let dir = tempdir().unwrap();
        let cfg_path = dir.path().join("config.toml");

        let mut cfg = CaissonConfig::load_from(&cfg_path).unwrap();
        assert_eq!(cfg.quotas.max_worktrees_per_repo, 25);
        assert_eq!(cfg.quotas.max_worktrees_total, 50);
        assert_eq!(cfg.quotas.soft_disk_limit, "250G");
        assert_eq!(cfg.quotas.min_host_free, "50G");
        assert_eq!(cfg.prune.default_inactive, "7d");
        assert!(cfg.prune.protect_dirty);
        assert!(cfg.prune.protect_unpushed);

        cfg.set_key("quotas.soft_disk_limit", "400G").unwrap();
        cfg.set_key("quotas.max_worktrees_per_repo", "40").unwrap();
        cfg.set_key("prune.default_inactive", "14d").unwrap();
        cfg.set_key("prune.protect_dirty", "false").unwrap();
        cfg.save_to(&cfg_path).unwrap();

        let reloaded = CaissonConfig::load_from(&cfg_path).unwrap();
        assert_eq!(reloaded.get_key("quotas.soft_disk_limit").unwrap(), "400G");
        assert_eq!(
            reloaded.get_key("quotas.max_worktrees_per_repo").unwrap(),
            "40"
        );
        assert_eq!(reloaded.get_key("prune.default_inactive").unwrap(), "14d");
        assert_eq!(reloaded.get_key("prune.protect_dirty").unwrap(), "false");
    }

    #[test]
    fn config_rejects_invalid_keys_and_values() {
        let mut cfg = CaissonConfig::default();
        assert!(cfg.get_key("nonexistent.key").is_err());
        assert!(cfg.set_key("nonexistent.key", "1").is_err());
        assert!(cfg.set_key("quotas.soft_disk_limit", "not-a-size").is_err());
        assert!(cfg.set_key("prune.default_inactive", "forever").is_err());
        assert!(cfg.set_key("prune.protect_dirty", "maybe").is_err());
    }

    #[test]
    fn loads_repo_caisson_toml() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join(REPO_CONFIG_FILENAME),
            r#"
[worktree]
cow_copy = ["node_modules", ".venv"]
post_create = "echo ready"
"#,
        )
        .unwrap();

        let repo_cfg = RepoConfig::load_from_repo(dir.path()).unwrap();
        assert_eq!(repo_cfg.worktree.cow_copy, vec!["node_modules", ".venv"]);
        assert_eq!(repo_cfg.worktree.post_create.as_deref(), Some("echo ready"));
    }
}
