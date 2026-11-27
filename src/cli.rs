use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "caisson",
    version,
    about = "Daemon-shielded sibling APFS developer workspaces and Copy-on-Write git worktrees for macOS"
)]
pub struct Cli {
    /// Emit structured JSON output for scripting or companion UI apps.
    #[arg(long, global = true)]
    pub json: bool,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Create and shield the shared Tier-1 workspace APFS volume (default: /Volumes/caisson).
    Init {
        /// Volume name (lowercase [a-z0-9][a-z0-9._-]*, no spaces).
        #[arg(long, default_value = "caisson")]
        name: String,

        /// Hard APFS volume quota: `auto` (min(70%, cap-100G)), `none`, or size (e.g. `300g`).
        #[arg(long, default_value = "auto")]
        quota: String,

        /// Disable the Layer-A hard APFS quota (equivalent to `--quota none`).
        #[arg(long)]
        no_quota: bool,

        /// Use Case-sensitive APFS formatting.
        #[arg(long)]
        case_sensitive: bool,

        /// Hide volume from Finder/LaunchServices via `nobrowse`.
        #[arg(long)]
        nobrowse: bool,

        /// Explicit APFS container reference (e.g. `disk3`); auto-detected if omitted.
        #[arg(long)]
        container: Option<String>,

        /// Custom mount point path (defaults to `/Volumes/<name>`).
        #[arg(long)]
        mount: Option<PathBuf>,

        /// Print planned privileged operations without executing them.
        #[arg(long)]
        dry_run: bool,
    },

    /// Manage dedicated Tier-2 APFS volumes (create, ls, shield, destroy).
    Vol {
        #[command(subcommand)]
        action: VolCommands,
    },

    /// Show overall Caisson status: container capacity, volumes, shields, daemon, and quotas.
    Status,

    /// Manage the one-shot boot LaunchDaemon (`/Library/LaunchDaemons/dev.caisson.mount.plist`).
    Daemon {
        #[command(subcommand)]
        action: DaemonCommands,
    },

    /// Re-mount registered Caisson volumes and re-apply `noatime` / daemon shields (called at boot).
    MountAll {
        /// Print planned operations without executing them.
        #[arg(long)]
        dry_run: bool,
    },

    /// View or live-edit soft policy quotas in `~/.config/caisson/config.toml` (no sudo).
    Config {
        #[command(subcommand)]
        action: ConfigCommands,
    },

    /// Audit macOS developer I/O health: daemon CPU/RSS, fseventsd bloat, snapshots, leaked DMGs, shields.
    Doctor,

    /// Passive read-only macOS daemon & system telemetry sampler for long builds.
    Watch {
        /// Run label recorded in every JSONL sample and summary.
        #[arg(long, default_value = "build")]
        label: String,

        /// Output JSONL file path (a paired `.summary.json` is written on exit).
        #[arg(long)]
        out: Option<PathBuf>,

        /// Sampling interval in seconds.
        #[arg(long, default_value_t = 10.0)]
        interval: f64,

        /// Stop automatically after this PID exits (including zombie detection).
        #[arg(long)]
        watch_pid: Option<i32>,

        /// Seconds to wait for a final trailing sample after stop/PID exit.
        #[arg(long, default_value_t = 5.0)]
        post_settle: f64,

        /// Recompute and print the JSON summary from an existing JSONL file and exit.
        #[arg(long)]
        summarize: Option<PathBuf>,
    },

    /// Native APFS Copy-on-Write clone via Darwin `clonefile(2)` (`CLONE_NOFOLLOW`).
    Clone {
        /// Source file, symlink, or directory.
        src: PathBuf,
        /// Destination path (must reside on the same APFS volume and not exist).
        dst: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
pub enum VolCommands {
    /// Create and shield a dedicated Tier-2 APFS volume.
    Create {
        /// Volume name (lowercase [a-z0-9][a-z0-9._-]*, no spaces).
        name: String,

        /// Optional hard APFS quota (e.g. `300g`, `500G`, or `none`).
        #[arg(long)]
        quota: Option<String>,

        /// Format as Case-sensitive APFS.
        #[arg(long)]
        case_sensitive: bool,

        /// Mount with `nobrowse` (hide from Finder).
        #[arg(long)]
        nobrowse: bool,

        /// Explicit APFS container reference (e.g. `disk3`); auto-detected if omitted.
        #[arg(long)]
        container: Option<String>,

        /// Custom mount point path (defaults to `/Volumes/<name>`).
        #[arg(long)]
        mount: Option<PathBuf>,

        /// Print planned commands without executing them.
        #[arg(long)]
        dry_run: bool,
    },

    /// List Caisson-managed APFS volumes with usage and shield health.
    Ls,

    /// Idempotently re-apply daemon shields (`mdutil`, `fseventsd`, `tmutil`, `noatime`) to a volume.
    Shield {
        /// Registered volume name, UUID, or absolute mount path.
        target: String,

        /// Enable `nobrowse` when remounting.
        #[arg(long)]
        nobrowse: bool,

        /// Print planned shield steps without executing them.
        #[arg(long)]
        dry_run: bool,
    },

    /// Permanently delete a Caisson-managed APFS volume (triple-checked safety guard).
    Destroy {
        /// Registered volume name or UUID.
        target: String,

        /// Skip interactive confirmation prompt.
        #[arg(long)]
        yes: bool,

        /// Verify safety checks and print `diskutil apfs deleteVolume` without executing.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum DaemonCommands {
    /// Install and bootstrap `/Library/LaunchDaemons/dev.caisson.mount.plist`.
    Install {
        /// Path to the `caisson` binary to invoke at boot (defaults to current executable).
        #[arg(long)]
        bin: Option<PathBuf>,

        /// Print planned plist and launchctl commands without executing them.
        #[arg(long)]
        dry_run: bool,
    },

    /// Unload and remove `/Library/LaunchDaemons/dev.caisson.mount.plist`.
    Uninstall {
        /// Print planned commands without executing them.
        #[arg(long)]
        dry_run: bool,
    },

    /// Check whether the Caisson boot LaunchDaemon is installed and loaded.
    Status,
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommands {
    /// Display the effective Caisson configuration.
    Show,

    /// Read a single dotted configuration key (e.g. `quotas.soft_disk_limit`).
    Get { key: String },

    /// Update a dotted configuration key and persist `~/.config/caisson/config.toml`.
    Set { key: String, value: String },
}
