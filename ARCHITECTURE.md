# Caisson Architecture

**Caisson** is a fast, native macOS CLI written in Rust that gives developers and multi-agent coding workflows isolated, daemon-shielded APFS workspaces with first-class Copy-on-Write git worktree management.

---

## 1. The Problem on macOS

Heavy parallel development on macOS (multiple coding agents running git worktrees, large C++/Rust/Chromium/Xcode builds) hits a compounding set of OS-level bottlenecks on the default Data volume:

1. **Spotlight indexing churn (`mds` / `mds_stores` / `mdworker`):** Directory-level exclusions (`.metadata_never_index`, `.noindex`, and even System Settings → Spotlight → Search Privacy) do not reliably stop `mds_stores` or `mds` from waking up and burning CPU during high-frequency write storms on `/`.
2. **`fseventsd` log & memory bloat:** XNU emits VFS events on every create/modify/unlink. Under sustained build and worktree churn, `fseventsd` writes large compressed event logs into `/.fseventsd` and can bloat to gigabytes of resident memory and swap.
3. **Time Machine local APFS snapshots:** Hourly local snapshots on the main Data volume pin deleted build outputs (`.o` files, `node_modules`, dead worktrees) for up to 24 hours, fragmenting APFS extent trees and bloating purgeable space.
4. **Access-time metadata churn (`atime`), `rm -rf` storms, and worktree sprawl:** Deleting tens of thousands of build files via `rm -rf` takes minutes and triggers a second `fseventsd` storm; unpruned agent worktrees silently accumulate into the hundreds.
5. **External-drive TCC trap:** Moving builds to external Thunderbolt/USB APFS drives avoids internal volume contention, but macOS TCC treats external drives as *Removable Volumes* and pops permission prompts for freshly built ad-hoc test binaries on every new cdhash.

### Why NOT `.sparseimage`, FUSE, or FSKit?

* **`.sparseimage` / `.sparsebundle`** stacks APFS-on-APFS, suffers 3×–7× slower small-file and worktree I/O due to sparse allocation overhead, requires manual compaction (`hdiutil compact`), and still triggers host volume events as the backing image grows.
* **User-space filesystems (FUSE / FSKit)** still sit *below* XNU's VFS layer (`bsd/vfs/vfs_fsevents.c`), so they cannot hide events from `fseventsd`, while adding kernel↔userspace context-switch overhead to every `stat()`/`open()`/`unlink()` call.

### Why Internal Sibling APFS Volumes Win

Creating a **native sibling APFS volume** inside the Mac's internal APFS container (`diskutil apfs addVolume`):
* Runs direct kernel APFS → NVMe with **zero virtualization overhead**.
* **Dynamically shares free space** across the APFS container (starts at ~1 MB, grows and shrinks automatically).
* Exposes clean **per-volume** daemon controls: `mdutil -i off`, `.metadata_never_index`, `.fseventsd/no_log`, `tmutil addexclusion`, `mount -u -o noatime[,nobrowse]`.
* Is treated by macOS as **fixed internal storage**, avoiding external-drive TCC prompts when running freshly compiled ad-hoc binaries.
* Supports native **`clonefile(2)` Copy-on-Write** within the volume and **O(1) teardown** via `diskutil apfs deleteVolume`.

---

## 2. Two-Tier Storage Model

Caisson separates everyday worktrees from heavyweight build sandboxes so that **coding agents never need `sudo`** while still getting full APFS isolation.

```text
Internal APFS Container (e.g. disk3)
├── Macintosh HD / Data          (macOS system & personal files, untouched)
│
├── [Tier 1] /Volumes/caisson    (Created ONCE via `sudo caisson init`)
│   ├── .caisson-volume.json     (UUID ownership marker)
│   ├── .metadata_never_index    (Spotlight shield)
│   ├── .fseventsd/no_log        (Disable fseventsd disk log persistence)
│   ├── .Trashes                 (0-byte file blocking Finder trash creation)
│   ├── worktrees/<repo>/<slug>/ (All `caisson wt` worktrees — NO sudo!)
│   ├── cache/<repo>/            (Optional warm CoW templates for node_modules/deps)
│   └── .trash/<uuid>/           (Atomic <1ms rename target for async unlink)
│
└── [Tier 2] /Volumes/<name>     (Dedicated volumes via `sudo caisson vol create`)
    └── Heavyweight builds (Chromium, Xcode DerivedData, ephemeral swarms)
        • Custom hard APFS quota (`--quota 300g`)
        • Optional Case-sensitive APFS (`--case-sensitive`)
        • Instant ~1s O(1) teardown via `sudo caisson vol destroy <name>`
```

### Why One Shared Tier-1 Volume for Worktrees?
1. **Zero `sudo` for agents:** Only the one-time `sudo caisson init` requires root. Every `caisson wt add`, `rm`, `ls`, `prune`, `clone`, `doctor`, and `watch` runs completely unprivileged.
2. **Same-volume `clonefile(2)` CoW:** `clonefile(2)` fails across different volumes (`EXDEV`). Keeping all worktrees of a repo on `/Volumes/caisson` enables instant, zero-disk-cost cloning of `node_modules`, `.venv`, or warm caches between worktrees.
3. **No APFS volume-count ceiling:** Scales cleanly from 2 worktrees to hundreds without hitting APFS container volume limits.
4. **Works from any existing repo:** `git worktree add` works seamlessly across volumes—users do not have to migrate `~/workspace` on day 1 to start using `caisson wt`.

---

## 3. Two-Layer Quota & Hygiene Model

APFS locks `-quota` and `-reserve` into the volume superblock **at creation time** (macOS provides no live resize verb for individual APFS volume quotas). Caisson therefore uses a **two-layer quota architecture**:

### Layer A — Hard APFS Kernel Ceiling (Create-Time Blast Shield)
Protects the host OS even from runaway compilers or infinite log loops that bypass the `caisson` CLI.
* **Tier 1 (`caisson init`) default:** Auto-computed as `min(70% of container capacity, container capacity - 100 GiB)` (minimum `50 GiB`, or disabled if disk is too small unless specified). Override with `--quota <size>` or `--no-quota`.
* **Tier 2 (`caisson vol create`) default:** Explicit `--quota <size>` encouraged (e.g., `--quota 250g`), since Tier-2 volumes are scoped to a single heavy workload and easily recreated.

### Layer B — Live-Adjustable Soft Policy Quotas (Zero `sudo`, Anytime)
Stored in `~/.config/caisson/config.toml` and enforced instantaneously (via `statfs(2)` and git metadata checks) on `caisson wt add`:

```toml
[quotas]
max_worktrees_per_repo = 25   # warn at 80%, refuse at 100% unless --force
max_worktrees_total = 50      # global cap across all repos
soft_disk_limit = "250G"      # editable anytime without recreating the volume
min_host_free = "50G"         # refuse wt add if container free space drops below 50G

[prune]
default_inactive = "7d"       # prune based on last-activity, not creation age
protect_dirty = true          # never prune uncommitted changes unless --force
protect_unpushed = true       # never prune unpushed commits unless --force
```

When a soft quota is hit, `caisson wt add` exits with a structured error (and JSON payload in `--json` mode) showing how many inactive worktrees can be reclaimed and the exact `caisson wt prune` command to run.

---

## 4. Shield Stack & Boot Persistence

Every Caisson-managed volume receives the following idempotent shield stack (`caisson create` / `caisson shield`):
1. `mdutil -i off <mountpoint>` — disables Spotlight indexing at the volume level.
2. `<mountpoint>/.metadata_never_index` (mode `0444`) — belt-and-suspenders Spotlight marker.
3. `<mountpoint>/.fseventsd/no_log` — disables persistent `fseventsd` disk log files on the volume.
4. `<mountpoint>/.Trashes` (0-byte file, mode `0444`) — prevents Finder trash directory creation.
5. `tmutil addexclusion -v <mountpoint>` — excludes the volume by APFS UUID from Time Machine backups and local snapshots (requires Full Disk Access for the invoking terminal; gracefully warns if FDA is not granted).
6. `mount -u -o noatime[,nobrowse] <mountpoint>` — eliminates access-time writes on read-heavy compile/stat storms (and optional Finder/LaunchServices hiding).
7. `chown <invoking-user>:staff <mountpoint>` — hands full unprivileged ownership to the developer.

### Reboot Persistence (`LaunchDaemon`)
On modern macOS, `/etc/fstab` rejects `noatime` on secondary APFS volumes and breaks auto-mounting. `sudo caisson daemon install` writes a single auditable one-shot plist at `/Library/LaunchDaemons/dev.caisson.mount.plist`:
* `RunAtLoad = true`, `KeepAlive = false` — runs `caisson mount-all` once at boot to re-apply `noatime` / `nobrowse` and verify shields, then exits immediately (**zero persistent background process, zero root socket attack surface**).

---

## 5. Worktree Ergonomics & CoW Cloning

### Repo Configuration (`.caisson.toml`, optional)
```toml
[worktree]
# Untracked directories to APFS CoW-clone from a sibling worktree or warm cache
cow_copy = ["node_modules", ".venv"]
# Optional hook run inside the new worktree after creation
post_create = ""
```

### Fast Worktree Lifecycle
* **`caisson wt add <branch> [--from <ref>] [--name <slug>] [--json]`**
  1. Checks soft quotas (`min_host_free`, `soft_disk_limit`, `max_worktrees_*`).
  2. Runs `git worktree add` targeting `/Volumes/caisson/worktrees/<repo-id>/<slug>`.
  3. For each entry in `cow_copy`, looks for a matching directory in the main repo (if on the same APFS volume) or in another worktree/warm cache on `/Volumes/caisson`, and clones it via native `libc::clonefile(2)` (`CLONE_NOFOLLOW`).
  4. Prints the canonical path (or structured JSON with `--json`).
* **`caisson wt rm <target> [--force]`**
  1. Verifies clean state unless `--force`.
  2. Atomically renames the worktree directory into `/Volumes/caisson/.trash/<uuid>` (`rename(2)` — **<1 ms**).
  3. Runs `git worktree prune` to deregister the worktree immediately.
  4. Spawns a detached background worker (or cleans inline with `--sync`) to unlink `/Volumes/caisson/.trash/<uuid>`.
* **`caisson wt prune [--inactive 7d] [--apply]`**
  * Dry-run by default.
  * Computes **last-activity timestamp** in `<1 ms` per worktree from `.git/worktrees/<name>/{index,HEAD,logs/HEAD}` and top-level directory `mtime` (never crawls `node_modules`).
  * Skips dirty or unpushed worktrees unless explicitly overridden.

---

## 6. Safety Invariants

Because Caisson invokes destructive disk operations (`diskutil apfs deleteVolume`), the following invariants are enforced unconditionally:
1. **Triple-checked `destroy`:** Refuses to delete any volume unless:
   * Its APFS Volume UUID is present in Caisson's registry (`/Library/Application Support/caisson/registry.json`),
   * The mounted volume root contains `.caisson-volume.json` matching that exact UUID, AND
   * Its APFS role is not `System`, `Data`, `VM`, `Preboot`, or `Recovery`.
2. **Explicit confirmation:** `destroy` requires interactive confirmation or `--yes`.
3. **Dry-run everywhere:** `--dry-run` prints every privileged command without executing it.
4. **No spaces in volume names:** Rejected by default (`[a-z0-9][a-z0-9._-]*`) to prevent silent failures in Chromium/GN/Make/Autotools toolchains.
5. **Strict privilege boundary:** Only `init`, `vol create/shield/destroy`, and `daemon install/uninstall` require `sudo`. No persistent root IPC socket in v0.1.

---

## 7. Crate & Module Structure (Rust)

Single static binary (`caisson`, Rust 2021/2024 edition) with minimal dependencies (`clap`, `serde`, `serde_json`, `toml`, `plist`, `libc`, `thiserror`, `chrono`):

```text
src/
├── main.rs              # CLI entrypoint & exit codes
├── cli.rs               # clap command definitions & --json dispatch
├── config.rs            # ~/.config/caisson/config.toml & .caisson.toml
├── registry.rs          # /Library/Application Support/caisson/registry.json
├── apfs/
│   ├── diskutil.rs      # APFS container/volume discovery, addVolume, deleteVolume
│   ├── shield.rs        # mdutil, .metadata_never_index, .fseventsd/no_log, tmutil, mount -u
│   ├── clonefile.rs     # Safe wrapper around Darwin clonefile(2) + EXDEV handling
│   └── statfs.rs        # Free space, quota usage, mount-flag inspection (MNT_NOATIME/DONTBROWSE)
├── daemon/
│   └── launchd.rs       # One-shot LaunchDaemon plist generation & launchctl bootstrap
├── worktree/
│   ├── git.rs           # git worktree add/list/prune, dirty/unpushed detection
│   ├── activity.rs      # Fast last-activity mtime calculation
│   ├── trash.rs         # <1ms atomic rename + async background unlink
│   └── quotas.rs        # Soft quota enforcement & actionable diagnostics
├── doctor.rs            # Daemon CPU/RSS audit, fseventsd bloat, maxvnodes, snapshots, leaked DMGs
└── watch.rs             # Passive read-only build telemetry sampler (JSONL + summary)
```

### Companion App Compatibility (v0.2+)
Every read/status command (`doctor`, `status`, `vol ls`, `wt ls`, `watch`) exposes a stable `--json` schema so a thin optional **SwiftUI `MenuBarExtra` app** can poll the ~2 ms Rust CLI directly with zero FFI and zero duplicated logic.
