# ADR-001: Modular monolith rather than microservices

**Status:** Accepted

## Context
The platform is built and operated by one developer. It needs clear internal
boundaries (auth, storage, messaging, retrieval) without the cost of running
and coordinating many deployable services.

## Decision
Build a single Cargo workspace. Cross-cutting concerns live in `common-*` crates;
the application lives in `apps/tenant-saas`. The API and background worker are
separate processes built from the same codebase (see the system design, §20).

## Alternatives considered
- Microservices per domain (auth, ingestion, retrieval).
- A single crate with no internal crate boundaries.

## Positive consequences
- One build, one CI pipeline, one deployable artifact.
- No network contracts or distributed transactions between modules.
- Crate boundaries still enforce dependency direction.

## Negative consequences
- Modules scale together, not independently.
- Discipline is needed to stop crates from depending on each other cyclically.

## Security / operational implications
- A single trust boundary and a single set of secrets to manage.

## Revisit when
A module has a clearly different scaling, availability, or team-ownership
profile that justifies extraction.
