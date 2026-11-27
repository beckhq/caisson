use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ShellKind {
    Zsh,
    Bash,
    Fish,
}

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
    /// Interactive unprivileged-first onboarding wizard (drive picker, single-sudo seal & daemon, shell, agents, T3, exec probe).
    Setup {
        /// Preview planned actions without running `sudo` or editing files in `$HOME`.
        #[arg(long)]
        dry_run: bool,

        /// Skip LaunchDaemon installation prompt.
        #[arg(long)]
        no_daemon: bool,

        /// Skip shell profile (`~/.zshrc` / `~/.bashrc` / `config.fish`) integration prompt.
        #[arg(long)]
        no_shell: bool,

        /// Skip coding-agent skill installation prompt.
        #[arg(long)]
        no_agents: bool,

        /// Skip T3 Code worktree location prompt.
        #[arg(long)]
        no_t3: bool,

        /// Skip the live `syspolicyd` 2-binary exec latency probe.
        #[arg(long)]
        no_probe: bool,
    },

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

        /// Target the APFS container backing an existing mounted volume (e.g. `/Volumes/my-ssd`).
        #[arg(long)]
        on: Option<PathBuf>,

        /// Custom mount point path (defaults to `/Volumes/<name>`).
        #[arg(long)]
        mount: Option<PathBuf>,

        /// Print planned privileged operations without executing them.
        #[arg(long)]
        dry_run: bool,

        /// Skip the interactive coding-agent skill setup prompt after volume creation.
        #[arg(long)]
        no_agents: bool,
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

    /// Run a self-contained APFS CoW cloning, teardown, and `syspolicyd` first-exec micro-benchmark (zero sudo).
    Bench {
        /// Number of synthetic files to clone and remove (10..=20000).
        #[arg(long, default_value_t = 1000)]
        files: usize,

        /// Directory to benchmark inside (defaults to active Caisson volume root or temp dir).
        #[arg(long)]
        target: Option<PathBuf>,
    },

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

        /// Allow appending to an existing non-empty `--out` JSONL file.
        #[arg(long)]
        append: bool,
    },

    /// Native APFS Copy-on-Write clone via Darwin `clonefile(2)` (`CLONE_NOFOLLOW`).
    Clone {
        /// Source file, symlink, or directory.
        src: PathBuf,
        /// Destination path (must reside on the same APFS volume and not exist).
        dst: PathBuf,
    },

    /// Manage Copy-on-Write git worktrees on the shared Tier-1 Caisson volume (zero sudo).
    Wt {
        #[command(subcommand)]
        action: WtCommands,
    },

    /// Drain `.trash/` and reconcile orphaned git worktree metadata.
    Gc {
        /// Optional path to a git repository to reconcile in addition to managed worktrees.
        #[arg(long)]
        repo: Option<PathBuf>,
    },

    /// Generate shell completion scripts (`zsh`, `bash`, `fish`).
    Completions {
        /// Target shell.
        #[arg(value_enum)]
        shell: ShellKind,
    },

    /// Print shell integration snippet enabling `caisson wt cd <target>` (and optional alias).
    ShellInit {
        /// Target shell (`zsh`, `bash`, or `fish`).
        #[arg(value_enum)]
        shell: ShellKind,

        /// Optional short alias name (e.g. `--alias cai`).
        #[arg(long)]
        alias: Option<String>,
    },

    /// Manage coding-agent skill integration (Claude Code, Codex, Antigravity, Cursor, OpenCode, Windsurf).
    Agent {
        #[command(subcommand)]
        action: AgentCommands,
    },

    /// Guided T3 Code integration (configure Worktree location on a shielded Caisson volume).
    T3 {
        #[command(subcommand)]
        action: T3Commands,
    },
}

#[derive(Debug, Subcommand)]
pub enum T3Commands {
    /// Preview or update T3 Code's Worktree location (`~/.t3/userdata/settings.json`) to a shielded Caisson volume.
    Setup {
        /// Registered Caisson volume name to use (defaults to the healthy volume with the most free space).
        #[arg(long)]
        volume: Option<String>,

        /// Explicit target directory path (overrides `--volume`).
        #[arg(long)]
        path: Option<PathBuf>,

        /// Preview planned changes without editing `~/.t3/userdata/settings.json`.
        #[arg(long)]
        dry_run: bool,

        /// Skip interactive confirmation prompt.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum AgentCommands {
    /// Show detected coding agents and whether the Caisson skill is installed.
    Status,

    /// Install the official Caisson `SKILL.md` for detected or specified coding agents (zero sudo).
    Install {
        /// Specific agent IDs (e.g. `claude`, `codex`, `antigravity`, `cursor`, `opencode`, `windsurf`, `agents`).
        targets: Vec<String>,

        /// Install for all detected agents without prompting.
        #[arg(long)]
        all: bool,

        /// Preview planned skill installations without writing files.
        #[arg(long)]
        dry_run: bool,
    },

    /// Remove installed Caisson `SKILL.md` files for specified agents (or `--all`).
    Uninstall {
        /// Specific agent IDs to uninstall.
        targets: Vec<String>,

        /// Uninstall from every agent where the Caisson skill is installed.
        #[arg(long)]
        all: bool,

        /// Preview planned removals without deleting files.
        #[arg(long)]
        dry_run: bool,
    },

    /// Refresh already-installed agent skills to match this binary's embedded `SKILL.md` (preserves custom edits unless `--force`).
    Update {
        /// Overwrite customized skill files too.
        #[arg(long)]
        force: bool,

        /// Preview planned updates without writing files.
        #[arg(long)]
        dry_run: bool,
    },

    /// Silence passive discovery tips for specified coding agents.
    Dismiss {
        /// Agent IDs to silence (e.g. `codex`, `cursor`).
        targets: Vec<String>,
    },

    /// Print a concise drop-in snippet for repo-level `AGENTS.md` / `CLAUDE.md` files.
    Snippet,
}

#[derive(Debug, Subcommand)]
pub enum WtCommands {
    /// Create a new git worktree on `/Volumes/caisson` with optional APFS CoW cloning.
    Add {
        /// Branch name to create or check out.
        branch: String,

        /// Base ref (commit, tag, or branch) when creating a new branch.
        #[arg(long)]
        from: Option<String>,

        /// Explicit worktree directory slug (defaults to sanitized branch name).
        #[arg(long)]
        name: Option<String>,

        /// Target git repository path (defaults to current working directory).
        #[arg(long)]
        repo: Option<PathBuf>,

        /// Skip `.caisson.toml` `cow_copy` cloning.
        #[arg(long)]
        no_cow: bool,

        /// Bypass soft quota limits (emits a warning instead of refusing).
        #[arg(long)]
        force: bool,
    },

    /// List managed Caisson worktrees with last-activity age, dirty/unpushed flags, and size.
    Ls {
        /// Target git repository path (defaults to current working directory).
        #[arg(long)]
        repo: Option<PathBuf>,

        /// Show worktrees across all repositories.
        #[arg(long)]
        all: bool,
    },

    /// Print the canonical filesystem path of a managed worktree (for `cd` / scripts).
    Path {
        /// Worktree slug, branch name, or full path.
        target: String,

        /// Target git repository path (defaults to current working directory).
        #[arg(long)]
        repo: Option<PathBuf>,
    },

    /// Remove a worktree via instant `<1ms` atomic rename to `.trash/` and async unlink.
    Rm {
        /// Worktree slug, branch name, or full path.
        target: String,

        /// Target git repository path (defaults to current working directory).
        #[arg(long)]
        repo: Option<PathBuf>,

        /// Remove even if the worktree has uncommitted changes or unpushed commits.
        #[arg(long)]
        force: bool,

        /// Unlink files synchronously instead of spawning a detached background worker.
        #[arg(long)]
        sync: bool,
    },

    /// Prune inactive worktrees based on last-activity timestamp (dry-run by default).
    Prune {
        /// Inactivity threshold (e.g. `7d`, `48h`, `30m`). Defaults to `prune.default_inactive`.
        #[arg(long)]
        inactive: Option<String>,

        /// Include worktrees with uncommitted changes.
        #[arg(long)]
        include_dirty: bool,

        /// Include worktrees with unpushed commits.
        #[arg(long)]
        include_unpushed: bool,

        /// Prune across all repositories instead of only the current repository.
        #[arg(long)]
        all: bool,

        /// Target git repository path (defaults to current working directory).
        #[arg(long)]
        repo: Option<PathBuf>,

        /// Actually remove matching worktrees (without `--apply`, runs in dry-run preview mode).
        #[arg(long)]
        apply: bool,

        /// Unlink removed worktrees synchronously when `--apply` is given.
        #[arg(long)]
        sync: bool,
    },

    /// Hydrate an existing git worktree (e.g. from T3 Code or `git worktree add`) with APFS CoW clones.
    Hydrate {
        /// Path inside the target git worktree (defaults to current working directory).
        #[arg(long)]
        path: Option<PathBuf>,

        /// Preview planned CoW clones and donor paths without modifying files.
        #[arg(long)]
        dry_run: bool,

        /// Explicitly execute the main repository's `.caisson.toml` `worktree.post_create` hook
        /// (disabled by default so untrusted branches cannot trigger shell commands during hydration).
        #[arg(long)]
        run_post_create: bool,

        /// Opt-in package-manager install fallback when no `node_modules` donor exists or the donor's
        /// lockfile differs from the target worktree (runs offline/frozen install directly without `/bin/sh`).
        #[arg(long)]
        fallback_install: bool,
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

        /// Target the APFS container backing an existing mounted volume (e.g. `/Volumes/my-ssd`).
        #[arg(long)]
        on: Option<PathBuf>,

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
