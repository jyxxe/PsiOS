# PsiOS

PsiOS is an independent **Agent-Native Linux operating system** project with three planned editions: **Helium** (minimal), **Neon** (server), and **Argon** (KDE Plasma desktop). PsiOS defines its own system contracts and release criteria. The native image baseline is tracked in [ADR-01](docs/v1/adr/ADR-001.md).

The repository contains the **PsiOS v1.0.0 documentation baseline (2026-11-22)** and an initial Rust implementation of the single-node Core. `psiosd` serves the local System API. `psiosctl` and the Rust SDK use that API for identity and policy, durable tasks, OCI applications, local volumes, backups and agent tools.

Build with `cargo build --locked --workspace`, or use the [portable amd64 Core build](docs/core-local.md#portable-amd64-core) for standalone Linux binaries. The [Core package guide](docs/host-package.md) covers the optional .deb integration tested on Ubuntu 24.04. The [QEMU guide](docs/host-vm.md) runs a development node on an Ubuntu Minimal guest. The [local Core guide](docs/core-local.md) covers initialization, running a task, enrolling an agent, approvals, recovery and verification. The [host and Agent Runtime guide](docs/core-runtime.md) covers systemd control, concurrent scheduling, release switching, binary files, versioned Skills and restartable multi-step sessions. The sealed v1 documentation retains its original release and acceptance status.

## Documentation

- [Implementation status, measured scope and remaining gaps](docs/implementation-status.md)
- [Offline inspection of a stopped node's ext4 disk image](docs/offline-rescue.md)
- [Documentation index](docs/README.md)
- [v1.0.0 full documentation set](docs/v1/README.md)
- [Complete specification (Markdown)](docs/v1/PsiOS-v1-complete.md)
- [Architecture and API contracts](docs/v1/docs/00-reading-and-authority.md)
- [ADR register](docs/v1/adr/README.md)
- [Acceptance plans](docs/v1/acceptance/README.md)
- [Document validation report](docs/v1/validation-report.json)

Historical material under `docs/v1/sources/archive` is retained for provenance only; it is not an approved implementation baseline. The latest Forgejo/XeLab code requires independent auditing before reuse.
