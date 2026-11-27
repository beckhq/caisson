use serde::Serialize;
use std::path::PathBuf;
use thiserror::Error;

/// Structured soft-quota violation details suitable for both human and JSON output.
#[derive(Debug, Clone, Serialize)]
pub struct QuotaViolation {
    pub rule: String,
    pub message: String,
    pub current: String,
    pub limit: String,
    pub reclaimable_worktrees: usize,
    pub suggested_command: String,
}

#[derive(Debug, Error)]
pub enum CaissonError {
    #[error("{0}")]
    Message(String),

    #[error("invalid volume name '{name}': {reason}")]
    InvalidVolumeName { name: String, reason: String },

    #[error("invalid size '{0}': expected a number with optional suffix (B, K, M, G, T)")]
    InvalidSize(String),

    #[error("invalid duration '{0}': expected a number with suffix (s, m, h, d, w)")]
    InvalidDuration(String),

    #[error("invalid config key '{0}'")]
    InvalidConfigKey(String),

    #[error("invalid config value for '{key}': {reason}")]
    InvalidConfigValue { key: String, reason: String },

    #[error("root privileges required for `{operation}` — re-run with `sudo caisson ...`")]
    RequiresRoot { operation: String },

    #[error("safety guard blocked destroy of '{volume}': {reason}")]
    DestroySafetyViolation { volume: String, reason: String },

    #[error("confirmation required to destroy volume '{0}' (re-run with `--yes` or confirm interactively)")]
    ConfirmationRequired(String),

    #[error("volume '{0}' not found")]
    VolumeNotFound(String),

    #[error("cross-volume clonefile(2) failed (EXDEV): cannot APFS CoW-clone '{src}' -> '{dst}' across different volumes. Ensure both paths reside on the same APFS volume (e.g. /Volumes/caisson).")]
    CrossDeviceClone { src: PathBuf, dst: PathBuf },

    #[error("quota exceeded ({rule}): {message}")]
    QuotaExceeded {
        rule: String,
        message: String,
        violation: Box<QuotaViolation>,
    },

    #[error("worktree '{0}' is dirty or has unpushed commits; pass `--force` to override")]
    WorktreeProtected(String),

    #[error("command `{cmd}` failed (exit {code:?}): {stderr}")]
    CommandFailed {
        cmd: String,
        code: Option<i32>,
        stderr: String,
    },

    #[error("plist parse error: {0}")]
    Plist(#[from] plist::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("TOML deserialize error: {0}")]
    TomlDe(#[from] toml::de::Error),

    #[error("TOML serialize error: {0}")]
    TomlSer(#[from] toml::ser::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, CaissonError>;
