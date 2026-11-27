---
name: caisson
description: Use Caisson on macOS when you would otherwise run `git worktree add` yourself, to create, list, or remove worktrees on the daemon-shielded `/Volumes/caisson` APFS volume. Let the host manage worktrees when it owns them (T3 Code `workspaceStrategy`, Claude Code worktree isolation, or IDE worktree managers).
---

# Caisson (macOS APFS Worktrees)

## When to Use

- Use `caisson wt` when **you** would otherwise invoke `git worktree add` / `list` / `remove` directly on macOS.
- **Do not route around host-managed worktrees.** If your host environment creates or binds a worktree for the thread (such as T3 Code `workspaceStrategy`, Claude Code worktree isolation, or an IDE worktree manager), let the host manage it.

## Preflight & Failure Cases

Check availability before creating a worktree:

```bash
command -v caisson && caisson status --json
```

Handle each state explicitly:
- **`caisson` not installed:** Fall back to standard `git worktree`.
- **Not initialized (`tier1_root_exists: false` and no `caisson` volume registered):** Do not run `sudo caisson init` yourself. Fall back to standard `git worktree` and tell the user they can run `sudo caisson init` once.
- **Registered but unmounted (`mounted: false`, e.g. after a reboot without the LaunchDaemon):** Tell the user to mount it (`sudo caisson mount-all` or `diskutil mount caisson`), or fall back to standard `git worktree` if immediate progress is required.
- **Ready (`tier1_root_exists: true`):** Use `caisson wt` below.

## Outside-the-Project Working Directory Gotcha

Worktrees created by `caisson wt add` live under `/Volumes/caisson/worktrees/...`, which is outside the original repository directory. Agents with directory-scoped write approvals (such as Claude Code) will prompt on every file edit unless `/Volumes/caisson` (or the worktree path) is permitted via `--add-dir /Volumes/caisson` or `additionalDirectories` in settings.

## Standard Commands (All Zero-`sudo`)

Always pass `--json` when parsing output programmatically.

```bash
# Create a worktree on /Volumes/caisson
caisson wt add <branch> [--from <ref>] [--name <slug>] --json

# Hydrate an existing worktree (e.g. host-managed) from `.caisson.toml` via same-volume APFS CoW
caisson wt hydrate [--dry-run] [--no-post-create] --json

# List managed worktrees (current repo, or --all across repos)
caisson wt ls --json
caisson wt ls --all --json

# Print canonical filesystem path of a worktree
caisson wt path <slug-or-branch>

# Remove a clean worktree (atomic rename to .trash/ + git worktree prune + background unlink)
caisson wt rm <slug-or-branch> --json
# Only pass --force if the user explicitly asked to discard uncommitted or unpushed work:
caisson wt rm <slug-or-branch> --force --json

# Preview inactive worktrees eligible for cleanup (dry-run by default)
caisson wt prune --inactive 7d --json

# Read-only macOS daemon & storage health audit
caisson doctor --json
```

## Optional `.caisson.toml` & Large Repos

`caisson wt add` checks out **tracked git files only** plus any relative paths listed under `cow_copy` in `<repo>/.caisson.toml`:

```toml
[worktree]
cow_copy = ["node_modules", ".venv", ".turbo"]
```

- Safe on large (e.g. Chromium-scale) repos: untracked build output directories (`out/`, `target/`, `DerivedData/`) are not copied unless listed in `cow_copy`.
- **Never put build output directories in `cow_copy`**; restrict `cow_copy` to dependency/package caches.

## Same-Volume Limit for `caisson clone`

```bash
caisson clone <src> <dst> --json
```

`caisson clone` invokes Darwin `clonefile(2)` directly. **It fails with `EXDEV` if `<src>` and `<dst>` are on different volumes** (for example, `/` to `/Volumes/caisson`) and does **not** fall back to a byte copy. Both paths must reside on the same APFS volume.

## Soft Quotas & Rules

- If `caisson wt add --json` exits non-zero with a `quota_violation` object, inspect `quota_violation.rule`, `quota_violation.reclaimable_worktrees`, and `quota_violation.suggested_command`. Preview with `caisson wt prune --json` and ask the user before running `--apply` or `--force`.
- **Never run `sudo` commands** (`caisson init`, `caisson vol ...`, `caisson daemon ...`) from an agent turn.
- **Never pass `--force` to `caisson wt rm`** unless the user explicitly requested discarding uncommitted or unpushed changes.
