---
name: caisson
description: Use Caisson on macOS to create, inspect, and clean up daemon-shielded sibling APFS git worktrees with instant Copy-on-Write dependency cloning instead of raw `git worktree` on `/`.
---

# Caisson (macOS APFS Worktree Engine)

Use `caisson` whenever you create, list, or remove git worktrees or perform Copy-on-Write clones on macOS. Caisson places worktrees on the shielded `/Volumes/caisson` sibling APFS volume (isolated from Spotlight, `fseventsd` disk logs, Time Machine snapshots, and `atime` churn) with zero `sudo` required.

## Quick Check

Before creating a worktree, verify that `/Volumes/caisson` is initialized:

```bash
caisson status --json
```

If `tier1_root_exists` is `true`, use `caisson wt` for all worktree operations. If it is `false`, do **not** run `sudo caisson init` yourself; tell the user to run `sudo caisson init` once, or fall back to standard `git worktree`.

## Standard Agent Commands (All Zero-`sudo`)

Always pass `--json` when parsing command output programmatically.

```bash
# Create a worktree on /Volumes/caisson (auto-clones .caisson.toml cow_copy deps via APFS clonefile)
caisson wt add <branch> [--from <ref>] [--name <slug>] --json

# List managed worktrees (current repo, or --all across repos)
caisson wt ls --json
caisson wt ls --all --json

# Print canonical filesystem path of a worktree
caisson wt path <slug-or-branch>

# Remove a clean worktree in <1 ms (atomic rename to .trash/ + git worktree prune + async unlink)
caisson wt rm <slug-or-branch> --json
# Only pass --force if the user explicitly asked to discard uncommitted or unpushed work:
caisson wt rm <slug-or-branch> --force --json

# Preview inactive worktrees eligible for cleanup (dry-run by default)
caisson wt prune --inactive 7d --json

# Native O(1) APFS Copy-on-Write clone on the same volume
caisson clone <src> <dst> --json

# Read-only macOS daemon & storage health audit
caisson doctor --json
```

## Handling Soft Quotas

If `caisson wt add --json` exits non-zero with a `quota_violation` JSON object:
1. Inspect `quota_violation.rule`, `quota_violation.reclaimable_worktrees`, and `quota_violation.suggested_command`.
2. Preview reclaimable worktrees with `caisson wt prune --json` (dry-run).
3. Ask the user before running `caisson wt prune --apply` or passing `--force`.

## Rules

- **Never run `sudo` commands** (`caisson init`, `caisson vol create/shield/destroy`, `caisson daemon install/uninstall`) from an agent turn; those are human-only administrative actions.
- **Never delete dirty or unpushed worktrees** with `--force` unless the user explicitly requested it.
- Prefer `caisson wt add <branch> --json` and use the returned `path` field as your working directory.
