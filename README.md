# PsiOS

PsiOS is a Linux-based **Agent-Native Operating System** project with three planned editions: **Helium** (minimal), **Neon** (server), and **Argon** (KDE Plasma desktop).

The repository contains the **PsiOS v1.0.0 documentation baseline (2026-11-22)** and an initial Rust implementation of the single-node Core. `psiosd` serves the local System API. `psiosctl` and the Rust SDK use that API for identity and policy, durable tasks, OCI applications, local volumes, backups and agent tools.

Build with `cargo build --locked --workspace`. For installation on an existing systemd host, see the [Core package guide](docs/host-package.md). The [QEMU installation guide](docs/host-vm.md) builds a bootable amd64 guest with a per-instance administrator key and local authority. The [local Core guide](docs/core-local.md) covers initialization, running a task, enrolling an agent, approvals, recovery and verification. The [host and Agent Runtime guide](docs/core-runtime.md) covers systemd control, concurrent scheduling, release switching, binary files, versioned Skills and restartable multi-step sessions. The sealed v1 documentation retains its original release and acceptance status.

## Documentation

- [Implementation status, measured scope and remaining gaps](docs/implementation-status.md)
- [Documentation index](docs/README.md)
- [v1.0.0 full documentation set](docs/v1/README.md)
- [Complete specification (Markdown)](docs/v1/PsiOS-v1-complete.md)
- [Architecture and API contracts](docs/v1/docs/00-reading-and-authority.md)
- [ADR register](docs/v1/adr/README.md)
- [Acceptance plans](docs/v1/acceptance/README.md)
- [Document validation report](docs/v1/validation-report.json)\n