# ADR-003: Shared PostgreSQL schema with RLS and micro-scoped transactions

**Status:** Accepted

## Context
The platform must support multiple organizations without operating a separate
database for every tenant.

## Decision
Use shared tables with tenant IDs, PostgreSQL row-level security (RLS),
tenant-scoped transactions, composite foreign keys, and application-level
membership authorization.

Two database roles keep this enforceable:
- `db_owner` owns the schema and runs migrations.
- `app_runtime` is used by the API. It is not a superuser, owns nothing, and has
  no `BYPASSRLS`, so RLS policies always apply to it.

## Alternatives considered
- Database per tenant.
- Schema per tenant.
- Shared tables without RLS.

## Positive consequences
- Simpler migrations and early operations.
- RLS provides a second line of defence if application checks have a bug.

## Negative consequences
- Requires strict transaction and role discipline.
- Requires cross-tenant security tests.
- Very large tenants may later need partitioning or a dedicated database.

## Security / operational implications
- The runtime role must never be granted DDL rights; CI verifies this.
- Tenant context is set per transaction (`begin_tenant_tx`, added in Phase 2).

## Revisit when
Tenant-specific regulatory, residency, performance, or isolation requirements
justify a different topology.
