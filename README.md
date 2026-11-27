# Caisson

**Daemon-shielded sibling APFS developer workspaces and instant Copy-on-Write git worktrees for macOS, written in Rust.**

[![CI](https://github.com/beckhq/caisson/actions/workflows/ci.yml/badge.svg)](https://github.com/beckhq/caisson/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](./LICENSE)
[![Platform: macOS APFS](https://img.shields.io/badge/platform-macOS%20APFS-black.svg)](#why-caisson)

> **The native macOS APFS workspace engine, purpose-built for parallel AI coding agents and heavyweight builds.**
>
> Born while building the **Refactor** browser at [BeckHQ](https://github.com/beckhq), where running dozens of parallel coding agents alongside multi-gigabyte Chromium builds pushed macOS Spotlight (`mds_stores`), `fseventsd`, Time Machine local snapshots, and `rm -rf` teardown storms past their breaking point.

**Why "Caisson"?** In civil engineering, a *caisson* is a watertight retaining chamber sunk below water or soft ground so crews can construct foundations in dry, isolated conditions without fighting the surrounding pressure. Caisson does the same thing on macOS: it carves out a native, daemon-shielded APFS chamber inside your internal SSD so builds and coding agents run at full NVMe speed, isolated from Spotlight, `fseventsd`, and Time Machine pressure.

---

## Why Caisson?

When you run parallel coding agents (`git worktree`) or heavy C++/Rust/Xcode/Chromium builds on the default macOS Data volume, macOS fights your filesystem on five fronts:

1. **Spotlight indexing churn (`mds` / `mds_stores`):** Directory exclusions like `.noindex` and `.metadata_never_index` on `/` provide **~0% CPU relief** during high-frequency write storms.
2. **`fseventsd` log & memory bloat:** Sustained build and worktree churn writes massive logs to `/.fseventsd` and can bloat `fseventsd` to gigabytes of RSS and swap.
3. **Time Machine local snapshots:** Hourly local APFS snapshots on `/` pin deleted build artifacts and dead worktrees for up to 24 hours.
4. **Slow teardown & worktree sprawl:** Deleting `node_modules` or build trees via `rm -rf` takes seconds to minutes and triggers a second `fseventsd` storm; forgotten agent worktrees accumulate silently.
5. **External-drive TCC traps:** Moving builds to external Thunderbolt/USB drives triggers macOS *Removable Volumes* TCC prompts for freshly compiled ad-hoc test binaries.

**Caisson solves all five at the APFS volume boundary** by creating a native **internal sibling APFS volume** (`/Volumes/caisson`) inside your Mac's internal APFS container. It dynamically shares free space with zero virtualization overhead, shielded from Spotlight, `fseventsd` disk logs, Time Machine, and `atime` churn.

---

## Benchmarks

Measured across a 4-way matrix on Apple Silicon APFS (12 parallel git worktrees + dependency cloning + 600-file compile + 15k small-file write storm + teardown):

| Storage Strategy | `mds_stores` CPU | Total Daemon CPU | 12× Worktrees | 15k Small Files | Teardown | End-to-End Wall |
| :--- | ---: | ---: | ---: | ---: | ---: | ---: |
| Main APFS (default) | 7.80 s | 36.93 s | 2.47 s | 0.58 s | 7.64 s (`rm -rf`) | 28.36 s |
| Main APFS + `.noindex` | 7.49 s | 37.82 s | 2.41 s | 0.57 s | 7.83 s (`rm -rf`) | 27.15 s |
| Shielded `.sparseimage` | 2.33 s | 18.51 s | 6.99 s *(3.1× slower)* | 3.65 s *(6.7× slower)* | 0.45 s | 33.53 s |
| **Caisson (Sibling APFS)** | **0.06 s (-99.2%)** | **14.47 s (-60.8%)** | **2.26 s** | **0.54 s** | **1.44 s (O(1))** | **20.56 s (-27.5%)** |

Key takeaways:
- **`.noindex` on `/` gave 0% relief** (`mds_stores` 7.49 s vs 7.80 s, total daemon CPU 37.82 s vs 36.93 s).
- **`.sparseimage` penalized small-file I/O by 6.7×** and parallel worktrees by 3.1× due to APFS-on-APFS sparse allocation overhead.
- **Sibling APFS volume** eliminated **99.2%** of `mds_stores` CPU and **60.8%** of total OS daemon CPU with native kernel NVMe speed.

---

## Quickstart

### 1. Install

```bash
# Homebrew (recommended, universal Apple Silicon + Intel binary)
brew install beckhq/tap/caisson

# Or from source (macOS, Rust stable)
cargo install --git https://github.com/beckhq/caisson
```

### 2. One-Time Setup (`sudo` once; never needed again for worktrees)

```bash
# Audit your Mac's current daemon overhead first (read-only, no sudo)
caisson doctor

# Preview every privileged command before running anything
caisson init --dry-run

# Create & shield /Volumes/caisson with an auto-computed hard APFS safety ceiling
sudo caisson init

# Optional: persist `noatime` & shields across reboots via one-shot LaunchDaemon
sudo caisson daemon install
```

### 3. Daily Zero-`sudo` Agent & Developer Worktrees

Run directly inside **any existing git repository**, with no need to move `~/workspace`:

```bash
# Create an isolated worktree on /Volumes/caisson (with instant APFS CoW dependency cloning)
caisson wt add feat/agent-auth

# Create from a specific ref or custom slug, with structured JSON for coding agents
caisson wt add fix/parser --from origin/main --name parser-v2 --json

# List worktrees with last-activity idle time, dirty/unpushed status, and size
caisson wt ls
caisson wt ls --all

# Jump directly into a worktree (after adding `eval "$(caisson shell-init zsh)"`)
caisson wt cd feat-agent-auth

# Remove in <1 ms (atomic rename to .trash/ + git worktree prune + detached background unlink)
caisson wt rm feat-agent-auth

# Dry-run prune of inactive worktrees (protects dirty & unpushed work by default)
caisson wt prune --inactive 7d
caisson wt prune --inactive 7d --apply

# Drain .trash/ and reconcile orphaned git worktree metadata
caisson gc
```

---

## Two-Tier Storage Architecture

```text
Internal APFS Container (e.g. disk3)
├── Macintosh HD / Data          (macOS system & personal files, untouched)
│
├── [Tier 1] /Volumes/caisson    (Created ONCE via `sudo caisson init`)
│   ├── .caisson-volume.json     (UUID ownership marker)
│   ├── .metadata_never_index    (Spotlight shield)
│   ├── .fseventsd/no_log        (Disable persistent fseventsd disk logs)
│   ├── .Trashes                 (0-byte file blocking Finder trash creation)
│   ├── worktrees/<repo>/<slug>/ (All `caisson wt` worktrees, ZERO sudo!)
│   ├── cache/<repo>/            (Optional warm CoW templates for node_modules/.venv)
│   └── .trash/<uuid>/           (<1 ms atomic rename target for async unlink)
│
└── [Tier 2] /Volumes/<name>     (Dedicated volumes via `sudo caisson vol create`)
    └── Heavyweight builds (Chromium, Xcode DerivedData, ephemeral build swarms)
        • Custom hard APFS quota (`--quota 300g`)
        • Optional Case-sensitive APFS (`--case-sensitive`)
        • Instant O(1) teardown via `sudo caisson vol destroy <name>`
```

See [`ARCHITECTURE.md`](./ARCHITECTURE.md) for the complete technical specification.

---

## Repo CoW Config (`.caisson.toml`)

Drop an optional `.caisson.toml` in any repository root to enable instant APFS `clonefile(2)` Copy-on-Write cloning of untracked dependency trees and post-create hooks:

```toml
[worktree]
# Cloned in O(1) with zero extra disk blocks from warm cache, sibling worktrees,
# or same-volume main checkout via native Darwin clonefile(2) (CLONE_NOFOLLOW).
cow_copy = ["node_modules", ".venv"]

# Optional shell command run inside the new worktree (`CAISSON_WORKTREE`, `CAISSON_REPO`, `CAISSON_BRANCH` set)
post_create = ""
```

---

## Two-Layer Quotas (Hard Blast Shield + Live Soft Limits)

APFS locks hard `-quota` into the volume superblock at creation time. Caisson pairs that with **live-editable soft quotas** in `~/.config/caisson/config.toml` (zero `sudo`):

```toml
[quotas]
max_worktrees_per_repo = 25   # warn at 80%, refuse at 100% unless --force
max_worktrees_total = 50      # global cap across all repos
soft_disk_limit = "250G"      # editable anytime without recreating the volume
min_host_free = "50G"         # refuse wt add if container free space drops below 50G

[prune]
default_inactive = "7d"       # measured by last activity, never crawling node_modules
protect_dirty = true          # never prune uncommitted changes unless --include-dirty
protect_unpushed = true       # never prune unpushed commits unless --include-unpushed
```

Update live anytime:

```bash
caisson config show
caisson config set quotas.max_worktrees_per_repo 40
caisson config set quotas.soft_disk_limit 400G
```

---

## Command Reference

### Zero-`sudo` Daily Commands

| Command | Purpose |
| :--- | :--- |
| `caisson wt add <branch> [--from <ref>] [--name <slug>] [--no-cow] [--force]` | Create a shielded CoW git worktree on `/Volumes/caisson` |
| `caisson wt ls [--all]` | List worktrees with idle age, dirty/unpushed flags, and size |
| `caisson wt path <target>` | Print canonical path of a worktree (powers `wt cd`) |
| `caisson wt rm <target> [--force] [--sync]` | Instant `<1 ms` atomic rename to `.trash/` + async unlink |
| `caisson wt prune [--inactive 7d] [--include-dirty] [--apply]` | Reclaim inactive worktrees safely (dry-run by default) |
| `caisson gc` | Drain `.trash/` and reconcile orphaned git worktree metadata |
| `caisson clone <src> <dst>` | Native Darwin `clonefile(2)` CoW clone (`CLONE_NOFOLLOW`) |
| `caisson doctor` | Audit daemon CPU/RSS (`fseventsd` bloat), snapshots, swap, shields |
| `caisson watch --label <name> --out <jsonl> [--watch-pid <pid>]` | Passive build telemetry sampler (`JSONL` + `.summary.json`) |
| `caisson status` / `caisson vol ls` | Inspect container capacity, volumes, shields, and daemon state |
| `caisson config show \| get \| set` | View or live-edit soft quotas without `sudo` |
| `caisson agent status \| install \| uninstall \| dismiss \| snippet` | Wire up coding agents (Claude Code, Codex, Antigravity, Cursor, OpenCode, Windsurf) |
| `caisson completions <zsh\|bash\|fish>` | Generate shell completions |
| `caisson shell-init <zsh\|bash\|fish> [--alias cai]` | Enable `caisson wt cd <slug>` shell navigation |

Every command supports `--json` for coding agents, scripts, and menu-bar companion apps.

### Privileged Administrative Commands (`sudo`)

| Command | Purpose |
| :--- | :--- |
| `sudo caisson init [--quota auto\|<size>\|none] [--case-sensitive] [--dry-run]` | Create & shield Tier-1 `/Volumes/caisson` once |
| `sudo caisson vol create <name> [--quota 300g] [--case-sensitive] [--nobrowse]` | Create & shield a dedicated Tier-2 build volume |
| `sudo caisson vol shield <name>` | Idempotently re-apply all 7 daemon shields |
| `sudo caisson vol destroy <name> [--yes] [--dry-run]` | O(1) delete a Caisson volume (**triple-checked safety guard**) |
| `sudo caisson daemon install \| uninstall` | Manage one-shot boot LaunchDaemon (`dev.caisson.mount`) |
| `sudo caisson mount-all` | Mount & re-apply `noatime` / shields on registered volumes |

---

## Coding Agent Integration

Caisson ships an official Agent Skill ([`skills/caisson/SKILL.md`](./skills/caisson/SKILL.md)) embedded right inside the binary so Claude Code, OpenAI Codex, Antigravity (AGY), Cursor, OpenCode, and Windsurf automatically prefer `caisson wt` over raw `git worktree` on `/`.

```bash
# Detect installed agents and install SKILL.md interactively (supports opt-out like `-cursor`)
caisson agent install

# Check installed vs detected coding agents
caisson agent status

# Print a concise snippet for any repository's AGENTS.md / CLAUDE.md
caisson agent snippet
```

During interactive `sudo caisson init`, Caisson detects your installed coding agents and offers to install the skill right away (pass `--no-agents` to skip). If you install a new coding agent later, interactive `caisson status` / `caisson doctor` / `caisson wt add` runs print a one-line tip on stderr offering `caisson agent install <agent>` (or `caisson agent dismiss <agent>` to silence).

---

## Shell Integration

Add to `~/.zshrc` (or `~/.bashrc` / `~/.config/fish/config.fish`):

```zsh
eval "$(caisson shell-init zsh --alias cai)"
eval "$(caisson completions zsh)"
```

Now you can jump directly into worktrees with `caisson wt cd <slug>` or `cai wt cd <slug>`.

---

## Safety Guarantees

Caisson treats destructive disk operations with extreme conservatism:

1. **Triple-checked `destroy`:** Refuses to delete any volume unless **all three** match independently:
   - Volume UUID is registered in `/Library/Application Support/caisson/registry.json`,
   - Live APFS volume has **no protected system role** (`System`, `Data`, `VM`, `Preboot`, `Recovery`, etc.) and is not mounted under `/` or `/System/*`, **AND**
   - Mounted volume root contains `.caisson-volume.json` with `managed_by == "caisson"` and an exact UUID match.
2. **Explicit confirmation:** Interactive volume-name confirmation or `--yes` required.
3. **Dry-run everywhere:** `--dry-run` prints every privileged command without mutating disk.
4. **Zero persistent root daemon:** The boot LaunchDaemon runs once (`RunAtLoad = true`, `KeepAlive = false`) and exits immediately.
5. **Structured APFS parsing:** Always parses `diskutil ... -plist` structurally via `plist`, never fragile text regexes.

---

## Contributing & License

- Contributions welcome! See [`CONTRIBUTING.md`](./CONTRIBUTING.md) and [`ARCHITECTURE.md`](./ARCHITECTURE.md).
- Security disclosures: [`SECURITY.md`](./SECURITY.md).
- Licensed under the [MIT License](./LICENSE).
