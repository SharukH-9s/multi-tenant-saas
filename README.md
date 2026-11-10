# Multi-Tenant AI SaaS Platform (`portfolio-engine`)

A production-oriented multi-tenant AI SaaS platform built in Rust.

## Prerequisites

- **Rust** 1.92.0+ (`rustup`, `rustfmt`, `clippy`)
- **Docker Desktop** (with Compose V2)
- **sqlx-cli**: `cargo install sqlx-cli --no-default-features --features postgres --locked`

## Workspace Layout

```text
portfolio-engine/
├── Cargo.toml              # Workspace manifest
├── rust-toolchain.toml     # Pinned toolchain channel
├── .gitignore
├── .env.example            # Environment template with separated DB roles
├── README.md
├── crates/
│   ├── common-config/      # Typed configuration loading
│   ├── common-errors/      # Error hierarchy and client-safe response mapping
│   ├── common-auth/        # Authentication, claims, and principal (Phase 2 stub)
│   ├── common-telemetry/   # Structured logging & tracing
│   ├── common-database/    # PostgreSQL connection pool and health check
│   ├── common-storage/     # Object storage abstraction (Phase 3 stub)
│   └── common-messaging/   # Redis Streams producer/consumer helpers (Phase 4 stub)
├── apps/
│   └── tenant-saas/        # Axum API and background worker application
├── infra/                  # Docker Compose, init scripts, and config
└── docs/                   # Architecture Decision Records (ADR)
```

## Quick Start (Phase 1)

1. **Copy environment variables**:
   ```powershell
   Copy-Item .env.example .env
   ```

2. **Verify workspace build & lint**:
   ```powershell
   cargo fmt --all --check
   cargo clippy --all-targets --all-features -- -D warnings
   cargo test --all
   ```

