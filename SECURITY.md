# Security Policy

## Scope & Privilege Model

Caisson invokes privileged macOS storage tools (`diskutil`, `mdutil`, `tmutil`, `mount`, `launchctl`) only during explicit administrative commands (`sudo caisson init`, `vol create/shield/destroy`, `daemon install/uninstall`, and boot-time `mount-all`).

- **Zero persistent root daemon:** The boot LaunchDaemon (`/Library/LaunchDaemons/dev.caisson.mount.plist`) runs `caisson mount-all` once at startup (`RunAtLoad = true`, `KeepAlive = false`) and exits immediately. There is no root IPC socket.
- **Zero `sudo` for daily agent & developer workflows:** `caisson wt`, `gc`, `clone`, `config`, `status`, `doctor`, and `watch` run strictly unprivileged.
- **Triple-checked volume destruction:** `caisson vol destroy` refuses to delete any volume unless all three independent checks match (registry UUID, live non-system APFS role, and on-volume `.caisson-volume.json` UUID marker) plus explicit confirmation.

## Reporting a Vulnerability

If you discover a security issue or safety-guard bypass in Caisson:

1. **Do not** open a public GitHub issue with exploit details.
2. Report it privately using GitHub Security Advisories on [`beckhq/caisson`](https://github.com/beckhq/caisson/security/advisories/new).
3. Include reproduction steps, macOS version, and `caisson --version`.

We aim to acknowledge reports within 48 hours and publish coordinated fixes promptly.
