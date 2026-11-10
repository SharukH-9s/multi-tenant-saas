# Multi-Tenant AI SaaS Platform

## End-to-End System Design and Engineering Specification

**Project:** Portfolio Project 01 --- Multi-Tenant AI SaaS Platform\
**Primary language:** Rust\
**Backend framework:** Axum\
**Architecture:** Modular monolith with independently deployable API and
worker processes\
**Document status:** Proposed engineering baseline\
**Version:** 1.0\
**Last updated:** 2026-11-09

------------------------------------------------------------------------

## Document purpose

This document defines the end-to-end architecture, requirements, data
model, APIs, security boundaries, asynchronous processing, AI workflows,
operational model, testing strategy, deployment approach, and
implementation plan for a production-oriented multi-tenant AI SaaS
platform.

It is intended to be the engineering blueprint for implementation. It is
not a claim that the system has already achieved the performance targets
listed here. Targets must be validated through repeatable tests and
recorded with their test environment.

## 1. Executive summary

The platform allows multiple independent organizations (tenants) to
upload documents, manage access to those documents, search them using
both lexical and semantic retrieval, and ask questions using
retrieval-augmented generation (RAG).

The platform is implemented primarily in Rust. It uses PostgreSQL as the
system of record, PostgreSQL Row-Level Security (RLS) as a
database-level tenant isolation control, pgvector for semantic
retrieval, PostgreSQL Full-Text Search (FTS) for lexical retrieval,
Redis Streams for asynchronous job delivery, and S3-compatible object
storage for original files.

The API and background worker are separate runtime processes built from
the same application codebase and shared internal crates. They can be
deployed and scaled independently.

### 1.1 Core capabilities

1.  User registration, authentication, and session management.
2.  Organization (tenant) creation and membership management.
3.  Role-based access control (RBAC).
4.  Secure document upload and lifecycle management.
5.  Asynchronous document parsing, normalization, chunking, and
    embedding.
6.  Hybrid search using vector retrieval and lexical retrieval.
7.  RAG question answering with source citations and Server-Sent Events
    (SSE).
8.  Tenant-level usage tracking, quotas, and audit records.
9.  Operational observability, reliability controls, and security
    testing.

### 1.2 Architectural principles

-   **Tenant isolation by multiple independent controls:** authorization
    in the application, tenant-scoped database transactions, PostgreSQL
    RLS, and tenant-aware relational constraints.
-   **PostgreSQL is the source of truth:** Redis transports work but
    does not own durable business state.
-   **At-least-once message delivery with idempotent consumers:** do not
    assume a queue delivers a message exactly once.
-   **Explicit transaction boundaries:** tenant context is
    transaction-local and every tenant-scoped database operation must
    use the scoped transaction path.
-   **Modular monolith first:** avoid premature microservices while
    keeping modules and runtime processes independently testable and
    deployable.
-   **External services are unreliable:** apply timeouts, bounded
    retries, backoff with jitter, circuit breakers, and clear failure
    states.
-   **Security is part of the design:** file parsing, prompt
    construction, secrets, logs, database roles, and provider data
    handling are all security concerns.
-   **Measure rather than promise:** performance and scalability claims
    must be backed by benchmark methodology and results.

------------------------------------------------------------------------

# 2. Scope and boundaries

## 2.1 In scope

-   Multi-tenant shared-database/shared-schema architecture.
-   User identity and organization memberships.
-   Owner, Admin, Member, and Viewer roles.
-   PDF, TXT, and Markdown document ingestion for the initial release.
-   Object storage for original files.
-   Document deduplication by tenant-scoped SHA-256 content hash.
-   Asynchronous ingestion using a transactional outbox and Redis
    Streams.
-   Text extraction, normalization, token-aware chunking, and embedding
    generation.
-   PostgreSQL FTS and pgvector hybrid retrieval with Reciprocal Rank
    Fusion (RRF).
-   RAG responses streamed over SSE.
-   Usage, quota, and audit-event foundations.
-   Docker-based local development and container deployment.
-   Automated tests, security verification, observability, and load
    testing.

## 2.2 Out of scope for the first release

The following should not be added until the core system is stable:

-   Database-per-tenant deployment.
-   Schema-per-tenant deployment.
-   Microservices for every domain module.
-   Kafka-based messaging.
-   Multi-region active-active deployment.
-   Custom model training or fine-tuning.
-   Complex enterprise SSO administration beyond a defined OIDC
    integration.
-   Arbitrary code execution or execution of uploaded document content.
-   Real-time collaborative editing of documents.
-   A sophisticated billing/payment platform.
-   Guaranteed sub-15ms or sub-25ms search latency before benchmarking.

## 2.3 External dependencies

The platform may depend on:

-   An OIDC identity provider, if external identity is enabled.
-   An embedding provider or self-hosted embedding service.
-   One or more LLM providers.
-   S3-compatible object storage.
-   PostgreSQL with the required extensions.
-   Redis.
-   An optional malware-scanning service.

External dependencies must be accessed through interfaces/adapters so
that provider-specific details do not leak throughout the domain layer.

------------------------------------------------------------------------

# 3. Users, tenants, and authorization model

## 3.1 Terminology

-   **User:** an individual identity that can authenticate.
-   **Tenant:** an organization or workspace that owns documents and
    usage.
-   **Membership:** the relationship between a user and a tenant,
    including role and status.
-   **Tenant context:** the authenticated user, selected tenant, and
    verified membership/permissions for one request.
-   **Resource owner:** the tenant that owns a document, chunk, job, API
    key, or usage record.

A user may belong to multiple tenants and may have a different role in
each tenant.

## 3.2 Roles

  ----------------------------------------------------------------------------------------
  Role       Organization     Manage      Upload      Search   Delete documents View usage
                 settings    members   documents   documents                    
  -------- -------------- ---------- ----------- ----------- ------------------ ----------
  Owner               Yes        Yes         Yes         Yes                Yes        Yes

  Admin           Limited        Yes         Yes         Yes                Yes        Yes

  Member               No         No         Yes         Yes   Own/authorized\*    Limited

  Viewer               No         No          No         Yes                 No         No
  ----------------------------------------------------------------------------------------

\*The initial implementation may choose either "delete own uploads" or
"no delete permission for Member." Make the chosen policy explicit in
authorization tests. Do not rely on UI restrictions.

Role names are not sufficient for fine-grained authorization. The
authorization layer should evaluate the action, tenant membership,
resource ownership, and any document-level access policy.

## 3.3 Authentication versus authorization

Authentication answers: "Who is making this request?"

Authorization answers: "May this identity perform this action in this
tenant on this resource?"

The authentication middleware validates the identity token and creates a
verified principal. The authorization layer verifies active membership
in the requested tenant and checks the permission required by the
operation.

A client-supplied `tenant_id` is a tenant selection hint, not proof of
membership.

## 3.4 Tenant selection

Tenant-scoped API routes may use a tenant identifier in the route or a
validated request context. The server must:

1.  Authenticate the user.
2.  Read the requested tenant identifier.
3.  Load or validate the user's active membership.
4.  Determine the user's role and permissions for that tenant.
5.  Construct a trusted `TenantContext`.
6.  Use that context to create tenant-scoped database transactions.

Never trust a tenant ID embedded in a request body without verifying
membership.

------------------------------------------------------------------------

# 4. Requirements

## 4.1 Functional requirements

### Identity and organization management

-   Register and authenticate users.
-   Support secure password hashing if local passwords are enabled.
-   Support OIDC login where configured.
-   Create organizations.
-   List organizations where the user has active membership.
-   Invite members.
-   Accept or revoke invitations.
-   Change member roles subject to permission rules.
-   Remove members.
-   Revoke sessions or refresh tokens.

### Document management

-   Upload PDF, TXT, and Markdown files.
-   Validate file size, extension, MIME type, and file signature where
    applicable.
-   Store original files in object storage.
-   Deduplicate documents within a tenant using SHA-256.
-   Return an asynchronous processing status.
-   List and retrieve document metadata.
-   Retry eligible failed ingestion jobs.
-   Delete documents and eventually remove associated chunks and stored
    objects.
-   Record lifecycle transitions and failure codes.

### Search

-   Search within the active tenant.
-   Combine lexical and semantic candidate sets.
-   Fuse rankings using RRF.
-   Support result limits and optional document filters.
-   Return snippets, document metadata, and stable citation identifiers.
-   Enforce authorization and tenant isolation for every result.

### RAG

-   Accept a question and optional retrieval filters.
-   Retrieve tenant-authorized chunks.
-   Build a bounded context.
-   Call a configured LLM provider.
-   Stream response events using SSE.
-   Include citations to source documents/chunks.
-   Track model, token usage, latency, and estimated cost where provider
    data is available.
-   Handle provider failures and client disconnections.

### Usage and audit

-   Record document and chunk counts.
-   Record embedding and LLM token usage where available.
-   Record request-level latency and provider information.
-   Record security-sensitive actions.
-   Enforce configurable quotas and rate limits.

## 4.2 Non-functional requirements

  -----------------------------------------------------------------------
  Category                            Requirement
  ----------------------------------- -----------------------------------
  Security                            No cross-tenant access through API
                                      or database paths

  Reliability                         Durable business state must not
                                      depend on Redis alone

  Correctness                         Duplicate jobs must not create
                                      duplicate logical chunks

  Availability                        External provider failures must not
                                      crash API/worker processes

  Performance                         Measure P50, P95, P99 and
                                      throughput for critical paths

  Scalability                         API and worker replicas can scale
                                      independently

  Maintainability                     Domain modules have explicit
                                      boundaries and interfaces

  Operability                         Logs, metrics, traces, health
                                      checks, and runbooks exist

  Portability                         Local development works through
                                      documented Docker Compose setup

  Privacy                             Data retention, deletion, and
                                      provider handling are documented
  -----------------------------------------------------------------------

## 4.3 Initial measurable acceptance targets

These are proposed test targets, not existing guarantees.

  -----------------------------------------------------------------------
  Area                                Initial acceptance target
  ----------------------------------- -----------------------------------
  Tenant isolation                    Zero cross-tenant records returned
                                      or modified in security tests

  Connection pool safety              Zero tenant context leakage under
                                      concurrent mixed-tenant tests

  Duplicate upload                    One logical document per
                                      tenant/content hash

  Worker recovery                     Abandoned jobs can be reclaimed and
                                      safely retried

  API load                            Test at 100, 250, and 500
                                      concurrent clients

  Search data scale                   Benchmark at 10k, 100k, and 1M
                                      chunks

  Search latency                      Publish P50/P95/P99 under a
                                      documented workload

  Search relevance                    Measure Recall@10 against a labeled
                                      query set

  RAG                                 Measure time-to-first-token and
                                      total response latency

  Deployment                          Reproducible build and deployment
                                      from documented instructions
  -----------------------------------------------------------------------

A target is accepted only after the benchmark environment, data
distribution, query mix, hardware, and test duration are recorded.

------------------------------------------------------------------------

# 5. System architecture

## 5.1 Architecture style

Use a **modular monolith** with two independently deployable process
types:

1.  **API process:** serves HTTP requests and SSE streams.
2.  **Worker process:** consumes ingestion jobs and performs parsing,
    chunking, embedding, and persistence.

Both processes share domain and infrastructure libraries. They do not
need to run as one process or scale together.

This design keeps deployment and operational complexity manageable while
preserving clear module boundaries.

## 5.2 Logical component diagram

``` mermaid
flowchart TB
    Client[Web App / API Client]
    Edge[Ingress / Reverse Proxy]
    API[Rust Axum API]
    Auth[Authentication and Authorization]
    Tenant[Tenant Context and Scoped DB Transaction]
    Docs[Document API]
    Search[Hybrid Search API]
    RAG[RAG API]
    Usage[Usage and Audit API]

    PG[(PostgreSQL + RLS + pgvector + FTS)]
    Redis[(Redis Streams / Cache / Rate Limits)]
    S3[(S3-compatible Object Storage)]
    Publisher[Outbox Publisher]
    Worker[Rust Ingestion Worker]
    Parser[Parser and Normalizer]
    Chunker[Token-aware Chunker]
    Embed[Embedding Provider]
    LLM[LLM Provider]
    OTel[OpenTelemetry Collector]
    Monitor[Metrics / Logs / Traces]

    Client --> Edge --> API
    API --> Auth --> Tenant
    Tenant --> Docs
    Tenant --> Search
    Tenant --> RAG
    Tenant --> Usage

    Docs --> PG
    Docs --> S3
    Docs --> Redis
    Publisher --> PG
    Publisher --> Redis
    Redis --> Worker
    Worker --> S3
    Worker --> Parser --> Chunker --> Embed
    Worker --> PG

    Search --> PG
    RAG --> PG
    RAG --> LLM

    API --> OTel
    Worker --> OTel
    OTel --> Monitor
```

## 5.3 Runtime components

### API process

Responsibilities:

-   HTTP routing and request validation.
-   Authentication and tenant authorization.
-   Tenant-scoped transaction creation.
-   Document upload orchestration.
-   Search and RAG orchestration.
-   SSE response management.
-   Usage and audit recording.
-   Health and readiness endpoints.

The API must not parse large documents or synchronously generate
embeddings during the upload request.

### Ingestion worker (`tenant-saas-worker`)

The worker process contains supervised background tasks:
- **Outbox Publisher:** A supervised Tokio task polling `outbox_events` and publishing to Redis Streams.
- **Document Processor & Embedding Worker:** Consumes from Redis Streams consumer groups, coordinates parsing, chunking, and embedding.

Ingestion worker responsibilities:

-   Consume ingestion events.
-   Claim and process jobs safely.
-   Download original files.
-   Parse and normalize supported formats.
-   Chunk extracted text.
-   Generate embeddings in bounded batches.
-   Persist chunks and embeddings.
-   Update document/job status.
-   Retry transient errors and classify permanent failures.
-   Acknowledge messages only after durable processing state is
    recorded.

The worker must enforce concurrency limits and provider rate limits.

### Outbox publisher (Supervised Worker Task)

The Outbox Publisher is **not** an independently deployed third service. It runs as a supervised Tokio background task inside `tenant-saas-worker` (or API process where configured).

Architecture:
```text
tenant-saas-api
      |
      v
PostgreSQL (outbox_events)
      |
      v
tenant-saas-worker
      |
      +--> Outbox Publisher (supervised Tokio task, SKIP LOCKED)
      |
      +--> Document Processor
      |
      +--> Embedding Worker
```

Execution mechanics:
-   **Concurrency control:** Queries unpublished events using `SELECT ... FOR UPDATE SKIP LOCKED` so multiple worker instances do not contend or double-publish.
-   **Polling schedule:** Configurable polling interval initialized at **200 ms**, to be tuned based on latency vs DB load benchmarks.
-   **Graceful shutdown:** Supervised via `tokio::select!` with cancellation tokens to ensure in-flight batches finish before shutdown.
-   **At-least-once guarantee:** Publish event to Redis Streams, then mark event published in PostgreSQL. If the worker restarts between publish and DB update, the consumer must handle duplicates idempotently.
-   **Dead-letter handling:** Configurable retry limits and dead-letter classification for unpublishable events.

### PostgreSQL

System of record for:

-   Users and tenants.
-   Memberships and roles.
-   Documents and document state.
-   Ingestion job state.
-   Chunks and embeddings.
-   Outbox events.
-   Usage records.
-   Audit records.
-   API key metadata and revocation state.

### Redis

Used for:

-   Redis Streams job transport.
-   Rate-limit counters.
-   Short-lived cache entries.
-   Optional distributed coordination where explicitly justified.

Redis is not the authoritative store for documents, memberships, job
state, or usage history.

### Object storage

Stores original document bytes. PostgreSQL stores object identifiers and
metadata, not large binary document bodies.

Use private buckets, server-side encryption where available, restricted
credentials, and tenant-aware object key prefixes. Object key prefixes
are organizational conventions, not a substitute for authorization.

------------------------------------------------------------------------

# 6. Rust workspace and application structure

## 6.1 Workspace layout

``` text
portfolio-engine/
├── Cargo.toml
├── Cargo.lock
├── rust-toolchain.toml
├── .env.example
├── .gitignore
├── README.md
│
├── crates/
│   ├── common-config/
│   ├── common-errors/
│   ├── common-auth/
│   ├── common-telemetry/
│   ├── common-database/
│   ├── common-storage/
│   └── common-messaging/
│
├── apps/
│   └── tenant-saas/
│       ├── Cargo.toml
│       ├── Dockerfile
│       ├── migrations/
│       └── src/
│           ├── main.rs
│           ├── lib.rs
│           ├── app_state.rs
│           ├── config/
│           ├── routes/
│           ├── middleware/
│           ├── domain/
│           │   ├── auth/
│           │   ├── tenant/
│           │   ├── membership/
│           │   ├── document/
│           │   ├── ingestion/
│           │   ├── search/
│           │   ├── rag/
│           │   ├── usage/
│           │   └── audit/
│           ├── services/
│           ├── repositories/
│           ├── workers/
│           └── ai/
│
├── infra/
│   ├── docker-compose.yml
│   ├── postgres/
│   ├── redis/
│   ├── object-storage/
│   └── monitoring/
│
├── docs/
│   ├── architecture/
│   ├── adr/
│   ├── api/
│   ├── security/
│   └── runbooks/
│
└── tests/
    ├── integration/
    ├── security/
    └── performance/
```

## 6.2 Crate responsibilities

  -----------------------------------------------------------------------
  Crate                   Responsibility          Must not contain
  ----------------------- ----------------------- -----------------------
  `common-config`         Typed configuration     Business rules
                          loading and validation  

  `common-errors`         Shared error types and  Raw secrets or internal
                          safe HTTP mapping       error leakage

  `common-auth`           Token verification,     Tenant data access
                          claims, principal types 

  `common-telemetry`      Tracing, metrics,       Business-specific
                          OpenTelemetry           dashboards
                          initialization          

  `common-database`       Pool construction and   Domain repository logic
                          safe database helpers   for every app

  `common-storage`        Object storage          Tenant authorization
                          abstraction and         decisions
                          adapters                

  `common-messaging`      Redis Streams           Authoritative job state
                          producer/consumer       
                          helpers                 
  -----------------------------------------------------------------------

Avoid turning shared crates into a dumping ground. A crate should exist
because at least two modules/processes need a stable shared capability
or because it defines an important boundary.

## 6.3 Application module responsibilities

-   **Routes:** HTTP request/response translation; keep business logic
    out.
-   **Middleware:** authentication, request IDs, rate limiting, and
    request-wide concerns.
-   **Domain:** entities, value objects, state transitions, and domain
    rules.
-   **Services:** orchestrate use cases and transactions.
-   **Repositories:** persistence operations and SQL.
-   **Workers:** message handling and background orchestration.
-   **AI adapters:** provider-specific embedding and LLM clients behind
    interfaces.

## 6.4 Dependency direction

Preferred dependency direction:

``` text
Routes
  |
  v
Application Services
  |
  +------> Domain
  |
  +------> Repository Interfaces
                 |
                 v
           SQLx Adapters

Application Services
  |
  +------> Storage Interfaces ---> S3 Adapter
  |
  +------> AI Interfaces --------> Provider Adapters
  |
  +------> Messaging Interfaces -> Redis Streams Adapter
```

Domain types should not depend on Axum request types, SQLx rows, Redis
message types, or provider-specific SDK response structures.

------------------------------------------------------------------------

# 7. Tenant isolation and database security

Tenant isolation is a critical security property. It must be enforced in
multiple layers.

## 7.1 Shared-schema model

Use one PostgreSQL database and shared tables. Every tenant-owned row
includes a non-null `tenant_id`.

Benefits:

-   One migration path.
-   Lower operational overhead than database-per-tenant.
-   Shared connection pool.
-   Efficient resource utilization for early and medium scale.

Trade-offs:

-   RLS and tenant scoping must be implemented correctly.
-   Noisy-neighbor effects are possible.
-   Very large tenants may eventually need partitioning or dedicated
    infrastructure.
-   Shared indexes require careful query and workload testing.

## 7.2 Trusted tenant context

A request must not set its own trusted database tenant context.

The API derives tenant context only after validating the authenticated
principal's active membership.

A conceptual context:

``` rust
pub struct TenantContext {
    pub tenant_id: Uuid,
    pub user_id: Uuid,
    pub role: TenantRole,
    pub permissions: Permissions,
}
```

The context should be immutable after construction and passed explicitly
into application services.

## 7.3 Transaction-local tenant setting

Use an explicit SQL transaction for every tenant-scoped database
operation.

Correct PostgreSQL approach:

``` sql
SELECT set_config(
    'app.current_tenant_id',
    $1,
    true
);
```

The third argument (`true`) makes the setting transaction-local. It is
cleared at transaction end.

Do not use session-scoped `SET app.current_tenant_id` on pooled
connections.

### Rust/SQLx conceptual pattern

``` rust
pub async fn begin_tenant_tx(
    pool: &PgPool,
    tenant_id: Uuid,
) -> Result<Transaction<'static, Postgres>, AppError> {
    let mut tx = pool.begin().await?;

    sqlx::query(
        "SELECT set_config('app.current_tenant_id', $1, true)"
    )
    .bind(tenant_id.to_string())
    .execute(&mut *tx)
    .await?;

    Ok(tx)
}
```

The exact SQLx transaction executor syntax should be verified against
the pinned SQLx version. Keep this helper centralized and test it.

Every query that touches tenant-owned data must execute using the same
transaction on which the tenant setting was established. Do not acquire
a second pooled connection inside a tenant-scoped service and assume it
inherits the setting.

### 7.3.1 Strict Engineering Rule: Micro-Scoped Transactions

> [!IMPORTANT]
> **Problem Prevented: Connection Pool Exhaustion**  
> An open transaction holds a physical connection checked out from `sqlx::PgPool`. External I/O (such as streaming a 100 MB upload to S3/MinIO or awaiting a streaming completion from an LLM provider) can take seconds. **Never hold a PostgreSQL connection/transaction open while waiting for external network I/O.**

#### Document Upload Flow: Storage Before DB Transaction
```text
User uploads document
        |
        v
Stream file to S3 / MinIO
        |
        v
Open PostgreSQL transaction
        |
        +--> Insert document record
        |
        +--> Insert outbox event
        |
        v
Commit transaction
        |
        v
Return response (202 Accepted)
```
*Safeguard for orphaned files:* If the S3/MinIO upload succeeds but the subsequent database transaction fails, an orphaned object remains in storage. The system must implement immediate error-handling deletion and a periodic orphan-reconciliation background job to identify and purge unreferenced objects.

#### RAG Flow: Retrieve Context, Commit, Then Call LLM
```text
User asks a question
        |
        v
Open database transaction (scoped to tenant)
        |
        +--> Retrieve relevant document chunks (hybrid search)
        |
        +--> Retrieve conversation context / metadata
        |
        v
Commit transaction (release connection back to pool immediately)
        |
        v
Call LLM Provider (streaming completion request)
        |
        v
Stream response using SSE
```

## 7.4 RLS policies

Enable RLS on every tenant-owned table.

Example:

``` sql
ALTER TABLE documents ENABLE ROW LEVEL SECURITY;
ALTER TABLE documents FORCE ROW LEVEL SECURITY;

CREATE POLICY documents_tenant_select
ON documents
FOR SELECT
TO app_runtime
USING (
    tenant_id =
    NULLIF(current_setting('app.current_tenant_id', true), '')::uuid
);

CREATE POLICY documents_tenant_insert
ON documents
FOR INSERT
TO app_runtime
WITH CHECK (
    tenant_id =
    NULLIF(current_setting('app.current_tenant_id', true), '')::uuid
);

CREATE POLICY documents_tenant_update
ON documents
FOR UPDATE
TO app_runtime
USING (
    tenant_id =
    NULLIF(current_setting('app.current_tenant_id', true), '')::uuid
)
WITH CHECK (
    tenant_id =
    NULLIF(current_setting('app.current_tenant_id', true), '')::uuid
);

CREATE POLICY documents_tenant_delete
ON documents
FOR DELETE
TO app_runtime
USING (
    tenant_id =
    NULLIF(current_setting('app.current_tenant_id', true), '')::uuid
);
```

Important policy rules:

-   PostgreSQL combines permissive policies with OR and restrictive
    policies with AND.
-   A restrictive policy alone does not grant access; an applicable
    permissive policy is also required.
-   Missing tenant context must fail closed.
-   Use explicit policies for each operation where practical.
-   Test `SELECT`, `INSERT`, `UPDATE`, and `DELETE`.
-   Do not rely only on application-side `WHERE tenant_id = ...`.

## 7.5 Database roles

Use separate roles:

-   `db_owner` / migration role: owns schema and runs migrations.
-   `app_runtime`: non-superuser runtime role, not table owner, no
    `BYPASSRLS`.
-   Optional read-only reporting role with carefully scoped access.

The application runtime role must not be a superuser, must not own
tenant tables, and must not have `BYPASSRLS`.

Revoke broad `PUBLIC` permissions and grant only required operations.

## 7.6 Relational tenant consistency

Use composite foreign keys to prevent cross-tenant relationships.

Example:

``` sql
ALTER TABLE documents
ADD CONSTRAINT documents_tenant_id_id_unique
UNIQUE (tenant_id, id);

ALTER TABLE document_chunks
ADD CONSTRAINT chunks_document_tenant_fk
FOREIGN KEY (tenant_id, document_id)
REFERENCES documents (tenant_id, id)
ON DELETE CASCADE;
```

Apply this pattern to jobs, chunks, and other records that reference
tenant-owned parent records.

## 7.7 Tables requiring tenant controls

At minimum:

-   `memberships` (with carefully designed user visibility rules).
-   `documents`.
-   `document_chunks`.
-   `ingestion_jobs`.
-   `outbox_events` if tenant-scoped payloads are stored there.
-   `api_keys`.
-   `usage_records`.
-   `audit_logs`.
-   Any future conversations, messages, or saved search resources.

Global identity tables such as `users` may have different access rules.
Do not blindly apply tenant RLS to global tables without designing how
users can discover their memberships.

## 7.8 RLS limitations and threat model

A custom PostgreSQL setting such as `app.current_tenant_id` is an
application-controlled context. It protects against many accidental
query-scoping errors, but it is not a cryptographic identity boundary
against an attacker who can execute arbitrary SQL as the runtime role
and change that setting.

Therefore:

-   Prevent SQL injection with parameterized SQL.
-   Do not expose arbitrary SQL execution.
-   Keep runtime DB privileges minimal.
-   Verify membership before setting tenant context.
-   Use separate DB roles for migrations and runtime.
-   Test malicious and malformed tenant context.
-   Consider stronger isolation mechanisms for higher-assurance
    deployments.

Do not describe this as "cryptographically impossible" tenant leakage.
Describe it as layered, database-enforced tenant isolation with a
documented threat model.

------------------------------------------------------------------------

# 8. Data model

The schema below is a logical baseline. Production migrations should add
check constraints, indexes, foreign keys, timestamps, and RLS policies
deliberately.

## 8.1 Entity relationship overview

``` mermaid
erDiagram
    USERS ||--o{ MEMBERSHIPS : has
    TENANTS ||--o{ MEMBERSHIPS : contains
    TENANTS ||--o{ DOCUMENTS : owns
    DOCUMENTS ||--o{ DOCUMENT_CHUNKS : contains
    TENANTS ||--o{ INGESTION_JOBS : owns
    DOCUMENTS ||--o{ INGESTION_JOBS : processed_by
    TENANTS ||--o{ API_KEYS : owns
    TENANTS ||--o{ USAGE_RECORDS : generates
    TENANTS ||--o{ AUDIT_LOGS : records
    TENANTS ||--o{ OUTBOX_EVENTS : emits

    USERS {
        uuid id PK
        text email
        text password_hash
        timestamptz created_at
    }
    TENANTS {
        uuid id PK
        text name
        text slug
        timestamptz created_at
    }
    MEMBERSHIPS {
        uuid id PK
        uuid tenant_id FK
        uuid user_id FK
        text role
        text status
    }
    DOCUMENTS {
        uuid id PK
        uuid tenant_id FK
        text title
        text content_hash
        text object_key
        text status
    }
    DOCUMENT_CHUNKS {
        uuid id PK
        uuid tenant_id FK
        uuid document_id FK
        integer chunk_index
        text content
        vector embedding
    }
    INGESTION_JOBS {
        uuid id PK
        uuid tenant_id FK
        uuid document_id FK
        text status
        integer attempts
    }
```

## 8.2 `users`

Suggested fields:

  -----------------------------------------------------------------------
  Field                   Type                    Notes
  ----------------------- ----------------------- -----------------------
  `id`                    UUID                    Primary key

  `email`                 CITEXT or normalized    Unique according to
                          TEXT                    identity policy

  `password_hash`         TEXT nullable           Null for external-only
                                                  identity

  `email_verified_at`     TIMESTAMPTZ nullable    Verification state

  `status`                TEXT                    Active, disabled,
                                                  pending

  `created_at`            TIMESTAMPTZ             Creation time

  `updated_at`            TIMESTAMPTZ             Last update
  -----------------------------------------------------------------------

Do not store plaintext passwords. Use Argon2id if local password
authentication is implemented.

## 8.3 `tenants`

  Field          Type          Notes
  -------------- ------------- --------------------------------
  `id`           UUID          Primary key
  `name`         TEXT          Display name
  `slug`         TEXT          Unique organization identifier
  `plan`         TEXT          Plan or quota profile
  `status`       TEXT          Active, suspended, deleted
  `created_at`   TIMESTAMPTZ   Creation time
  `updated_at`   TIMESTAMPTZ   Last update

## 8.4 `memberships`

  Field          Type          Notes
  -------------- ------------- ------------------------------
  `id`           UUID          Primary key
  `tenant_id`    UUID          Tenant
  `user_id`      UUID          User
  `role`         TEXT          Owner, Admin, Member, Viewer
  `status`       TEXT          Invited, active, suspended
  `created_at`   TIMESTAMPTZ   Creation time
  `updated_at`   TIMESTAMPTZ   Last update

Constraints:

-   Unique `(tenant_id, user_id)`.
-   Foreign keys to `tenants` and `users`.
-   Role check constraint.
-   Status check constraint.
-   Rules preventing accidental removal of the last active Owner.

## 8.5 `documents`

  Field                 Type                   Notes
  --------------------- ---------------------- -------------------------------
  `id`                  UUID                   Primary key
  `tenant_id`           UUID                   Tenant owner
  `uploaded_by`         UUID                   User who uploaded
  `title`               TEXT                   Display title
  `original_filename`   TEXT                   Sanitized original name
  `content_hash`        CHAR(64)               SHA-256
  `object_key`          TEXT                   Private object storage key
  `mime_type`           TEXT                   Validated media type
  `size_bytes`          BIGINT                 File size
  `status`              TEXT                   Lifecycle status
  `failure_code`        TEXT nullable          Safe machine-readable failure
  `failure_detail`      TEXT nullable          Sanitized diagnostic detail
  `chunk_count`         INTEGER                Persisted chunk count
  `created_at`          TIMESTAMPTZ            Creation time
  `updated_at`          TIMESTAMPTZ            Last update
  `deleted_at`          TIMESTAMPTZ nullable   Soft deletion timestamp

Constraints:

-   Unique `(tenant_id, content_hash)` for tenant-level deduplication.
-   Unique `(tenant_id, id)` for composite references.
-   Non-negative size and chunk count.
-   Status check constraint.

## 8.6 `document_chunks`

  -----------------------------------------------------------------------
  Field                   Type                    Notes
  ----------------------- ----------------------- -----------------------
  `id`                    UUID                    Primary key

  `tenant_id`             UUID                    Tenant owner

  `document_id`           UUID                    Parent document

  `chunk_index`           INTEGER                 Stable order

  `content`               TEXT                    Chunk body

  `token_count`           INTEGER                 Approximate or measured
                                                  token count

  `embedding`             VECTOR(1536)            Dimension must match
                                                  selected model

  `embedding_model`       TEXT                    Model identifier

  `embedding_version`     TEXT                    Pipeline/model version

  `tsv_content`           TSVECTOR                Generated or maintained
                                                  FTS representation

  `metadata`              JSONB                   Page number, section,
                                                  offsets, etc.

  `created_at`            TIMESTAMPTZ             Creation time
  -----------------------------------------------------------------------

Constraints:

-   Unique `(tenant_id, document_id, chunk_index)`.
-   Composite FK `(tenant_id, document_id)` to documents.
-   Tenant RLS.
-   Indexes selected from benchmark results.

Do not assume every embedding model has 1536 dimensions. The database
vector dimension must match the chosen embedding model. A model change
may require a new column/table or a controlled re-embedding migration.

## 8.7 `ingestion_jobs`

Suggested fields:

-   `id`
-   `tenant_id`
-   `document_id`
-   `status`
-   `attempts`
-   `max_attempts`
-   `available_at`
-   `locked_at`
-   `locked_by`
-   `last_error_code`
-   `last_error_at`
-   `created_at`
-   `updated_at`
-   `completed_at`

This table represents durable job state. Redis Streams messages are
delivery notifications, not the authoritative job record.

## 8.8 `outbox_events`

Suggested fields:

-   `id`
-   `tenant_id` nullable for global events
-   `aggregate_type`
-   `aggregate_id`
-   `event_type`
-   `payload` JSONB
-   `created_at`
-   `published_at` nullable
-   `publish_attempts`
-   `last_error`
-   `next_attempt_at`

Do not put secrets, raw file contents, or unnecessary personal data in
event payloads.

## 8.9 `api_keys`

Suggested fields:

-   `id`
-   `tenant_id`
-   `name`
-   `key_prefix`
-   `key_hash`
-   `scopes`
-   `last_used_at`
-   `expires_at`
-   `revoked_at`
-   `created_by`
-   `created_at`

Generate high-entropy keys. Show the secret only once. Store only a
secure verifier/hash, not the original key.

## 8.10 `usage_records`

Suggested fields:

-   `id`
-   `tenant_id`
-   `actor_user_id` nullable
-   `request_id`
-   `operation`
-   `provider`
-   `model`
-   `input_tokens`
-   `output_tokens`
-   `embedding_tokens`
-   `estimated_cost`
-   `duration_ms`
-   `created_at`

Usage records should be append-oriented. Corrections should be
represented by compensating records or an explicit correction mechanism.

## 8.11 `audit_logs`

Suggested fields:

-   `id`
-   `tenant_id`
-   `actor_user_id`
-   `action`
-   `resource_type`
-   `resource_id`
-   `request_id`
-   `source_ip` subject to privacy policy
-   `metadata` JSONB with an allowlist
-   `created_at`

Audit logs should not contain passwords, access tokens, API keys, raw
document content, or sensitive prompt content.

------------------------------------------------------------------------

# 9. Indexing and database performance

## 9.1 Baseline indexes

Create indexes based on actual query patterns. Initial candidates:

``` sql
CREATE INDEX idx_documents_tenant_created
ON documents (tenant_id, created_at DESC);

CREATE INDEX idx_documents_tenant_status
ON documents (tenant_id, status);

CREATE INDEX idx_chunks_tenant_document
ON document_chunks (tenant_id, document_id);

CREATE UNIQUE INDEX uq_chunks_document_position
ON document_chunks (tenant_id, document_id, chunk_index);

CREATE INDEX idx_chunks_tsv
ON document_chunks USING GIN (tsv_content);

CREATE INDEX idx_chunks_embedding
ON document_chunks
USING hnsw (embedding vector_cosine_ops)
WITH (m = 16, ef_construction = 64);
```

These are starting points, not a final indexing prescription.

## 9.2 Tenant filtering and vector indexes: pgvector 0.8.0+

### Version Requirement & Iterative Index Scans

> [!IMPORTANT]
> **Problem Prevented: Poor Search Recall Under Tenant Filtering**  
> In a shared database containing 1,000,000 document chunks across hundreds of organizations, a small tenant might own only 100 chunks. When searching, a standard HNSW vector traversal identifies nearest neighbors across the entire table. If nearby vectors belong to other tenants, applying the tenant filter post-traversal may leave fewer results than the requested `LIMIT` or miss relevant chunks entirely.

To prevent this recall collapse, the platform requires:
- **PostgreSQL 16**
- **pgvector 0.8.0 or newer** (pinned and tested image tag, e.g. `pgvector/pgvector:pg16-v0.8.0`)
- Explicit session/transaction setting:
  ```sql
  SET LOCAL hnsw.iterative_scan = 'relaxed';
  ```

*Why version 0.8.0+ matters:* Iterative index scans were officially introduced in **pgvector 0.8.0** (version 0.7.x does not support this feature). With `relaxed` iterative scans enabled, the index scan dynamically continues traversing the graph until it satisfies the query limit with rows that pass the tenant filter.

**Important Security Distinction:** Iterative scans solve a **retrieval recall and performance problem**; they do not provide tenant security. Tenant filtering in the SQL query and PostgreSQL RLS remain strictly mandatory.

Benchmark checklist:
-   Small tenants (100 chunks) co-located with large tenants (500,000 chunks).
-   Recall@10 with and without `hnsw.iterative_scan = 'relaxed'`.
-   Index build time and memory footprint under multi-tenant volume.
-   Query latency at 50, 100, and 250 concurrent client connections.

Do not partition prematurely. Partitioning by tenant increases operational
complexity and should only be evaluated if benchmark evidence shows index
degradation that iterative scans cannot resolve.

## 9.3 Connection pooling

Use SQLx `PgPool` with:

-   A bounded maximum connection count.
-   Connection acquire timeout.
-   Idle timeout and maximum lifetime configured for the hosting
    environment.
-   Database-side connection limits.
-   Metrics for pool saturation and wait time.

Every tenant-scoped operation must use the transaction that has the
correct tenant context. Do not rely on pool connection affinity between
requests.

------------------------------------------------------------------------

# 10. Document upload and ingestion pipeline

## 10.1 Upload sequence

The API should stream the upload rather than reading an unbounded body
into memory.

``` mermaid
sequenceDiagram
    actor Client
    participant API
    participant Auth as Auth / Membership
    participant S3 as Object Storage
    participant DB as PostgreSQL
    participant Outbox as Outbox Publisher
    participant Redis as Redis Streams
    participant Worker

    Client->>API: POST /api/v1/tenants/{tenant_id}/documents
    API->>Auth: Authenticate and authorize upload
    Auth-->>API: Verified TenantContext
    API->>API: Stream, validate, calculate SHA-256
    API->>DB: Check tenant-scoped duplicate
    alt Duplicate exists
        DB-->>API: Existing document
        API-->>Client: 409 Conflict or existing-resource response
    else New content
        API->>S3: Store object under generated key
        S3-->>API: Object key and checksum metadata
        API->>DB: BEGIN tenant-scoped transaction
        API->>DB: Insert document + ingestion job + outbox event
        API->>DB: COMMIT
        API-->>Client: 202 Accepted with document/job IDs
        Outbox->>DB: Read unpublished event
        Outbox->>Redis: XADD event
        Outbox->>DB: Mark event published
        Redis-->>Worker: Deliver through consumer group
    end
```

## 10.2 Object storage abstraction and database consistency

### 10.2.1 Storage Abstraction (`object_store`)

> [!IMPORTANT]
> **Problem Prevented: Vendor Lock-In and Difficult Testing**  
> Direct coupling to provider SDKs (e.g. `aws-sdk-s3`) bloats build times and makes testing difficult. By abstracting storage behind Apache Arrow's `object_store` crate, the application supports:
> - Local filesystem storage for rapid offline development.
> - MinIO for local Docker Compose integration testing.
> - AWS S3, Cloudflare R2, or Google Cloud Storage in production.

Our Rust domain interface in `crates/common-storage`:

```rust
use async_trait::async_trait;
use bytes::Bytes;
use crate::StorageError;

#[async_trait]
pub trait FileStorage: Send + Sync {
    async fn upload(&self, key: &str, data: Bytes) -> Result<(), StorageError>;
    async fn download(&self, key: &str) -> Result<Bytes, StorageError>;
    async fn delete(&self, key: &str) -> Result<(), StorageError>;
}
```

*Architectural distinction:* This trait is an abstraction for binary object storage, not a replacement for PostgreSQL metadata or the document-processing pipeline.

### 10.2.2 Storage/Database Consistency & Orphan Reconciliation

Object storage and PostgreSQL do not share a distributed transaction.

Use a compensating-action strategy:

1.  Validate the upload and calculate its SHA-256 hash.
2.  Write the binary object to a generated, private object key via `FileStorage`.
3.  Start a PostgreSQL tenant-scoped transaction (micro-scoped).
4.  Insert document record, job, and outbox event.
5.  Commit.
6.  If the database transaction fails:
    - Immediately attempt to delete the uploaded object (`storage.delete(&key)`).
    - If immediate deletion fails (e.g., storage timeout), emit an orphan cleanup task.
7.  A periodic orphan-reconciliation background job compares S3 object keys with active/pending document records in PostgreSQL, safely purging unreferenced objects older than 24 hours.

Do not claim that object storage and PostgreSQL are atomically committed. Always design for eventual consistency and orphan cleanup.

## 10.3 Duplicate upload race

A pre-insert hash check is useful for a fast response but is not
sufficient for correctness. Two concurrent requests can both pass the
check.

The unique constraint on `(tenant_id, content_hash)` is the final
arbiter.

The application should handle the unique-violation case and return a
deterministic response. Decide whether the API returns `409 Conflict` or
the existing document resource; document that contract.

## 10.4 Ingestion job state machine

``` mermaid
stateDiagram-v2
    [*] --> Pending
    Pending --> Processing
    Processing --> Ready
    Processing --> RetryScheduled: transient failure
    RetryScheduled --> Processing
    Processing --> Failed: permanent / attempts exhausted
    Failed --> Pending: authorized manual retry
    Ready --> Deleting
    Failed --> Deleting
    Deleting --> Deleted
```

State transitions must be validated in application/domain logic and
protected against stale workers.

Use a processing lease or claim token so an old worker cannot overwrite
the result of a newer attempt.

## 10.5 Outbox pattern

The API writes the document row, ingestion job row, and outbox event in
one PostgreSQL transaction.

The outbox publisher:

1.  Selects eligible unpublished events in bounded batches.
2.  Claims them safely, for example with row locking and `SKIP LOCKED`.
3.  Publishes to Redis Streams.
4.  Marks successfully published rows.
5.  Retries failures with backoff.
6.  Exposes backlog and oldest-event-age metrics.

There is an unavoidable duplicate window: Redis may accept the event
while the publisher crashes before updating PostgreSQL. The event may be
published again. Consumers must deduplicate by stable event/job ID.

## 10.6 Redis Streams consumer behavior

Use a consumer group.

A worker should:

1.  Read a message.
2.  Validate the event schema/version.
3.  Load the durable ingestion job from PostgreSQL.
4.  Claim the job with a lease/claim token.
5.  Process it.
6.  Persist results and final job/document state.
7.  Acknowledge the Redis message after durable completion.

For worker crashes:

-   Pending messages must be inspected.
-   Stale messages should be reclaimed with the appropriate Redis
    Streams mechanism.
-   A reclaimed message must still pass the database claim check.
-   The worker must be safe when the same event is delivered more than
    once.

## 10.7 Parsing and normalization

Initial supported formats:

-   PDF.
-   Plain text.
-   Markdown.

Pipeline stages:

1.  Verify actual file type; do not trust filename or client MIME
    header.
2.  Enforce maximum file size and page/character limits.
3.  Scan for malware where the deployment supports it.
4.  Parse in a resource-constrained process/container.
5.  Extract text and structural metadata.
6.  Normalize line endings and invalid character sequences.
7.  Preserve useful headings, page numbers, and section metadata.
8.  Reject empty or unsupported content with a safe failure code.

PDF parsers process attacker-controlled input. Apply CPU, memory,
file-size, and execution-time limits. Do not execute embedded scripts or
active content.

## 10.8 Token-Aware Chunking: tiktoken-rs

> [!IMPORTANT]
> **Problem Prevented: Incorrect Chunk Sizes and Embedding Input Limits**  
> Splitting documents naively by character count (e.g. every 1,000 characters) creates volatile token lengths because character-to-token ratios fluctuate dramatically across programming languages, markdown tables, and multi-byte UTF-8 texts. If a chunk exceeds the embedding model's context window, the provider rejects the batch (`400 Bad Request`). Conversely, if chunks are too small, critical semantic context is lost.

Pipeline:
```text
Document
   |
   v
Extracted & normalized text
   |
   v
Model-compatible Tokenizer (tiktoken-rs)
   |
   v
Token-aware sliding window chunks
   |
   v
Generate embeddings (bounded batches)
```

### Initial Configuration Parameters

| Parameter | Initial Value | Notes |
| :--- | :--- | :--- |
| **Target chunk size** | **500 tokens** | Verified below provider input limits |
| **Chunk overlap** | **75 tokens** | Preserves context across window boundaries |
| **Tokenizer library** | **`tiktoken-rs`** | High-performance Rust BPE tokenizer |
| **Tokenizer model** | Model-compatible | e.g. `cl100k_base` (OpenAI text-embedding-3) or `o200k_base` |
| **Embedding batch size** | 32 chunks | Bounded request payload |
| **Maximum document size** | Configurable | Enforced at upload boundary |
| **Maximum chunks per doc** | Configurable | Guardrail against runaway chunking |

*Crucial rule on tokenizers:* The tokenizer **must match the target embedding model**. `cl100k_base` or `o200k_base` should not be blindly assumed correct for every provider. The provider adapter configuration must explicitly bind the matching tokenizer encoding and validate token counts before dispatching API requests.

Chunking heuristics:
-   Prefer paragraph and section boundaries (`\n\n`, markdown headings) over arbitrary mid-sentence cuts.
-   Avoid splitting headings from the text they introduce.
-   Preserve structural metadata (page numbers, section titles, offsets) attached to each chunk in JSONB.
-   Record the chunking version in chunk metadata to support future re-chunking pipelines.

## 10.9 Embedding generation

Use a provider abstraction:

``` rust
#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    async fn embed(
        &self,
        inputs: &[String],
    ) -> Result<Vec<Vec<f32>>, EmbeddingError>;
}
```

The actual implementation should validate:

-   Number of returned vectors equals number of inputs.
-   Every vector has the expected dimension.
-   All values are finite.
-   Provider response is within configured size limits.
-   Model and version are recorded.

Use bounded concurrency and batch requests. Respect provider rate limits
and `Retry-After` where supplied.

## 10.10 Atomic chunk persistence

For one document processing attempt:

1.  Generate and validate all required embeddings for a bounded batch or
    staged generation.
2.  Begin a tenant-scoped transaction.
3.  Confirm the document is still in the expected processing state and
    the worker owns the current lease.
4.  Replace or upsert chunks for the current processing version.
5.  Update chunk count and document state.
6.  Mark the ingestion job complete.
7.  Commit.
8.  Acknowledge the message.

For very large documents, avoid one enormous transaction. Use a
versioned/staging strategy:

-   Generate chunks under an ingestion version.
-   Persist batches as staged/inactive.
-   Verify expected chunk count.
-   Atomically mark the version active and document Ready.
-   Clean up older/incomplete versions asynchronously.

Search must only retrieve chunks belonging to the active document
version.

------------------------------------------------------------------------

# 11. Hybrid search architecture

## 11.1 Why hybrid search

Lexical search is useful for exact words, names, identifiers, acronyms,
and product codes.

Semantic search is useful for meaning and paraphrases.

Combining them generally provides better coverage than relying on either
one alone.

## 11.2 Initial retrieval flow

``` mermaid
flowchart TD
    Q[User query] --> Auth[Authenticate and establish tenant]
    Auth --> E[Generate query embedding]
    E --> V[Vector candidate retrieval]
    Q --> F[PostgreSQL FTS candidate retrieval]
    V --> RRF[Reciprocal Rank Fusion]
    F --> RRF
    RRF --> Filter[Apply authorization and document filters]
    Filter --> Sort[Sort and limit]
    Sort --> Results[Results with citations]
```

Both retrieval branches must operate under the same tenant-scoped
database transaction/context.

## 11.3 PostgreSQL FTS versus BM25

PostgreSQL's `ts_rank` and `ts_rank_cd` are PostgreSQL full-text ranking
functions. They are not BM25.

The initial release uses PostgreSQL FTS plus pgvector, fused through
RRF. This is simpler to operate and provides a strong baseline.

If genuine BM25 is a hard requirement later, evaluate an
extension/search engine such as ParadeDB or a dedicated search service.
Treat that as an architecture change with its own benchmark and
operational evaluation.

## 11.4 Reciprocal Rank Fusion

RRF score:

\[ RRF(d) = `\sum`{=tex}\_{m `\in `{=tex}M}
`\frac{1}{k + rank_m(d)}`{=tex} \]

Where:

-   \(d\) is a candidate chunk.
-   \(M\) is the set of retrieval methods.
-   (rank_m(d)) is the 1-based rank of the chunk from method (m).
-   \(k\) is a smoothing constant, commonly initialized at 60.

RRF combines rankings without requiring raw vector distances and FTS
scores to share a common scale.

## 11.5 Candidate retrieval

Initial configuration:

-   Up to 30 vector candidates.
-   Up to 30 lexical candidates.
-   RRF smoothing constant: 60.
-   Return top 10 results.

Tune these values using relevance evaluation. A larger candidate set can
improve recall but increases database work.

## 11.6 Search SQL considerations

The final SQL should:

-   Use parameterized query values.
-   Filter to the active tenant context.
-   Exclude inactive/deleted document versions.
-   Exclude chunks from deleted documents.
-   Apply optional document filters safely.
-   Avoid returning internal storage keys or sensitive metadata.
-   Use deterministic tie-breaking.
-   Be checked with `EXPLAIN (ANALYZE, BUFFERS)`.

Do not assume a CTE or a single SQL round trip guarantees a fast query.
Verify the actual execution plan.

## 11.7 Search relevance evaluation

Create a small labeled evaluation set containing:

-   Exact keyword queries.
-   Acronyms and identifiers.
-   Paraphrase queries.
-   Questions requiring multiple chunks.
-   Queries with ambiguous terms.
-   Queries across documents with similar content.

Measure:

-   Recall@5 and Recall@10.
-   Precision@k where labels exist.
-   Mean Reciprocal Rank (MRR).
-   nDCG@k where graded relevance labels exist.
-   Search latency percentiles.

Keep the evaluation set versioned so changes can be compared.

------------------------------------------------------------------------

# 12. RAG architecture

## 12.1 Request flow: Micro-Scoped Retrieval

``` mermaid
sequenceDiagram
    actor User
    participant API
    participant Auth
    participant Search
    participant DB as PostgreSQL
    participant LLM
    participant Client

    User->>API: POST /rag/ask/stream
    API->>Auth: Authenticate + authorize tenant
    Auth-->>API: TenantContext
    
    rect rgb(240, 248, 255)
        note over API,DB: Micro-Scoped DB Transaction (Connection released immediately)
        API->>DB: BEGIN tenant-scoped transaction
        API->>Search: Retrieve relevant chunks (hybrid search)
        Search->>DB: Query chunks with tenant RLS + FTS + HNSW
        DB-->>Search: Ranked chunks
        Search-->>API: Context and citations
        API->>DB: COMMIT transaction
    end

    API->>API: Build bounded prompt (System instructions + context)
    
    rect rgb(255, 250, 240)
        note over API,LLM: External I/O (No DB connections held)
        API->>LLM: Streaming completion request
        LLM-->>API: Token/event stream
        API-->>Client: SSE events (message_start, token, citation)
    end
    
    API->>DB: Record usage & audit (separate async write)
```

## 12.2 RAG stages

1.  Validate question length and request options.
2.  Authenticate and authorize the tenant.
3.  Generate query embedding via provider.
4.  **Open micro-scoped tenant database transaction.**
5.  Retrieve tenant-authorized chunks via hybrid search (pgvector + FTS).
6.  Apply document access filters.
7.  Rank and select top chunks.
8.  **Commit database transaction and return connection to pool immediately.**
9.  Enforce context token budget.
10. Build system instructions and assemble user context (distinguishing trusted instructions from untrusted document data).
11. Call selected LLM provider (streaming completion).
12. Stream SSE events to client (`message_start`, `token`, `citation`, `usage`, `message_end`).
13. Record token usage, model, latency, and outcome asynchronously.

## 12.3 Context assembly

The context builder should:

-   Include only retrieved content the user is authorized to access.
-   Enforce a maximum context token budget.
-   Deduplicate near-identical chunks where useful.
-   Preserve document title, page/section metadata, and citation IDs.
-   Avoid blindly concatenating an unbounded number of chunks.
-   Keep trusted system instructions separate from untrusted document
    content.

## 12.4 Prompt injection and untrusted documents

Documents are untrusted data. A document may contain instructions such
as "ignore previous rules" or requests to reveal secrets.

The model prompt must clearly state that retrieved document text is
reference material, not instructions.

Controls:

-   Never put secrets in prompts.
-   Do not let retrieved text change tool permissions.
-   Do not execute commands or tools solely because a document requests
    it.
-   Treat tool use as a separately authorized action.
-   Limit model access to data already authorized for the requesting
    user.
-   Record relevant prompt/model versions without logging sensitive raw
    content by default.

Prompt instructions are not a substitute for application authorization.

## 12.5 Citations

Every citation should reference a stable chunk/document identifier and
safe metadata.

A response should not fabricate citations. If the provider returns text
without a supported citation, the API must not invent one.

Possible response structure:

``` json
{
  "answer": "The policy provides ...",
  "citations": [
    {
      "document_id": "uuid",
      "chunk_id": "uuid",
      "title": "Employee Handbook",
      "page": 12
    }
  ]
}
```

## 12.6 SSE event contract

Suggested events:

``` text
event: message_start
data: {"request_id":"...","model":"..."}

event: token
data: {"text":"The policy "}

event: citation
data: {"document_id":"...","chunk_id":"...","page":12}

event: usage
data: {"input_tokens":1200,"output_tokens":180}

event: message_end
data: {"finish_reason":"stop"}
```

Error events should use a documented safe schema. Once an SSE response
has started, the server cannot change the HTTP status code; it must send
a terminal error event when appropriate.

Handle client disconnects, upstream timeouts, cancellation, and provider
stream errors explicitly.

------------------------------------------------------------------------

# 13. API design

All routes are versioned under `/api/v1`.

## 13.1 API conventions

-   JSON request/response bodies unless streaming or file upload
    requires otherwise.
-   UUID resource identifiers.
-   UTC timestamps in ISO 8601 format.
-   Consistent error envelope.
-   Request ID on every response.
-   Cursor pagination for large lists.
-   Idempotency keys for operations where duplicate submission is
    harmful.
-   Explicit maximum body sizes and query limits.
-   OpenAPI documentation generated from the Rust API contract.

Example error:

``` json
{
  "error": {
    "code": "DOCUMENT_NOT_FOUND",
    "message": "The requested document was not found.",
    "request_id": "..."
  }
}
```

Do not expose SQL errors, stack traces, provider secrets, object keys,
or internal infrastructure details to clients.

## 13.2 Authentication endpoints

  Method   Endpoint           Purpose
  -------- ------------------ -----------------------
  POST     `/auth/register`   Register a user
  POST     `/auth/login`      Authenticate
  POST     `/auth/refresh`    Refresh session/token
  POST     `/auth/logout`     Revoke session
  GET      `/auth/me`         Current identity

If using OIDC, define callback/state/nonce/PKCE behavior and validate
issuer, audience, signature, expiry, and nonce as appropriate.

## 13.3 Tenant and membership endpoints

  ------------------------------------------------------------------------------------------
  Method                  Endpoint                                   Purpose
  ----------------------- ------------------------------------------ -----------------------
  POST                    `/tenants`                                 Create organization

  GET                     `/tenants`                                 List user's
                                                                     organizations

  GET                     `/tenants/{tenant_id}`                     Get organization

  PATCH                   `/tenants/{tenant_id}`                     Update organization

  POST                    `/tenants/{tenant_id}/invitations`         Invite member

  GET                     `/tenants/{tenant_id}/members`             List members

  PATCH                   `/tenants/{tenant_id}/members/{user_id}`   Change role/status

  DELETE                  `/tenants/{tenant_id}/members/{user_id}`   Remove member
  ------------------------------------------------------------------------------------------

## 13.4 Document endpoints

  -------------------------------------------------------------------------------------------------------
  Method                  Endpoint                                                Purpose
  ----------------------- ------------------------------------------------------- -----------------------
  POST                    `/tenants/{tenant_id}/documents`                        Upload document

  GET                     `/tenants/{tenant_id}/documents`                        List documents

  GET                     `/tenants/{tenant_id}/documents/{document_id}`          Get document metadata

  GET                     `/tenants/{tenant_id}/documents/{document_id}/status`   Get processing state

  POST                    `/tenants/{tenant_id}/documents/{document_id}/retry`    Retry eligible failure

  DELETE                  `/tenants/{tenant_id}/documents/{document_id}`          Delete document
  -------------------------------------------------------------------------------------------------------

## 13.5 Search and RAG endpoints

  Method   Endpoint                                Purpose
  -------- --------------------------------------- ----------------------
  POST     `/tenants/{tenant_id}/search`           Hybrid search
  POST     `/tenants/{tenant_id}/rag/ask`          Non-streaming answer
  POST     `/tenants/{tenant_id}/rag/ask/stream`   SSE answer

Example search request:

``` json
{
  "query": "What is the annual leave policy?",
  "limit": 10,
  "document_ids": []
}
```

Example RAG request:

``` json
{
  "question": "What is the annual leave policy?",
  "top_k": 10,
  "filters": {
    "document_ids": []
  },
  "stream": true
}
```

## 13.6 Usage and audit endpoints

  Method   Endpoint                                 Purpose
  -------- ---------------------------------------- ----------------
  GET      `/tenants/{tenant_id}/usage`             Summary usage
  GET      `/tenants/{tenant_id}/usage/documents`   Document usage
  GET      `/tenants/{tenant_id}/usage/ai`          AI usage
  GET      `/tenants/{tenant_id}/audit-logs`        Audit history

## 13.7 Status codes

  Code   Meaning
  ------ -----------------------------------------------
  200    Successful read/action
  201    Resource created
  202    Accepted for asynchronous processing
  204    Successful action with no response body
  400    Invalid request
  401    Missing/invalid authentication
  403    Authenticated but not authorized
  404    Resource not found or intentionally concealed
  409    Conflict, such as duplicate resource
  413    Upload too large
  415    Unsupported media type
  422    Valid syntax but invalid business input
  429    Rate limit exceeded
  500    Unexpected internal error
  503    Dependency temporarily unavailable

Choose consistent resource-concealment behavior. For example, a document
belonging to another tenant may return 404 rather than disclose its
existence.

------------------------------------------------------------------------

# 14. Authentication, secrets, and security controls

## 14.1 Password handling

If local password authentication is supported:

-   Use Argon2id with reviewed parameters.
-   Enforce password policy appropriate to the product.
-   Rate-limit login attempts.
-   Use generic login failure responses to reduce account enumeration.
-   Support password reset through short-lived, single-use tokens.
-   Never log passwords or password-reset tokens.

## 14.2 JWT/OIDC

Validate:

-   Signature using trusted keys.
-   Issuer.
-   Audience.
-   Expiry and not-before.
-   Allowed algorithms.
-   Key rotation.
-   Token type and intended use.

Do not accept unsigned tokens or choose a verification algorithm based
solely on untrusted token headers.

Keep access tokens short-lived. Store refresh-token verifiers securely
if refresh sessions are implemented. Support revocation/rotation and
replay detection.

## 14.3 API keys

-   Generate high-entropy random keys.
-   Display a newly created key only once.
-   Store a secure hash/verifier.
-   Store a non-secret prefix for identification.
-   Support scopes, expiry, rotation, and revocation.
-   Never place full keys in logs or analytics.

## 14.4 PII and logging

Use an allowlist for logged fields.

Safe examples:

-   Request ID.
-   Tenant ID where policy permits.
-   Route name.
-   Status code.
-   Duration.
-   Provider/model identifier.
-   Token counts.

Do not log:

-   Passwords.
-   Authorization headers.
-   API keys.
-   Refresh tokens.
-   Full document contents.
-   Raw prompts by default.
-   Sensitive personal information.

Apply retention and access controls to logs and traces.

## 14.5 Upload security

-   Enforce request and file-size limits at ingress and application
    layers.
-   Stream uploads with bounded buffers.
-   Validate file signatures and parser compatibility.
-   Sanitize filenames.
-   Generate object keys server-side.
-   Store objects privately.
-   Scan for malware where available.
-   Apply parser CPU/memory/time limits.
-   Do not serve uploaded content from the application origin without
    safe content handling.
-   Use signed URLs with short expiry only when direct client access is
    required.

## 14.6 AI data protection

Document:

-   Which providers receive data.
-   Whether prompts/documents are retained by providers.
-   Whether provider data is used for training.
-   Region and residency options.
-   Retention and deletion behavior.
-   Which tenant data can be sent to which provider.

Make provider choice configurable by deployment policy. Do not imply
that sending data to an external LLM is automatically privacy-safe.

------------------------------------------------------------------------

# 15. Rate limiting, quotas, and abuse prevention

Apply limits at multiple scopes:

-   Per IP for unauthenticated endpoints.
-   Per user for authentication and interactive operations.
-   Per API key for programmatic clients.
-   Per tenant for upload, search, and RAG usage.
-   Per provider/model for external AI calls.

Examples of configurable limits:

-   Requests per minute.
-   Concurrent RAG streams.
-   Maximum upload size.
-   Documents per tenant.
-   Total stored bytes.
-   Maximum chunks per document.
-   Embedding tokens per day.
-   LLM input/output tokens per day.

A Redis-backed token bucket or sliding-window approach can be used. The
chosen algorithm must be documented and tested under concurrent API
replicas.

Define graceful behavior when Redis is unavailable. For sensitive or
expensive endpoints, fail closed or use a conservative local fallback
rather than silently allowing unlimited usage.

------------------------------------------------------------------------

# 16. Reliability and failure handling

## 16.1 General rules

-   Set timeouts for every network dependency.
-   Retry only errors that are plausibly transient.
-   Use exponential backoff with jitter.
-   Bound retries and concurrency.
-   Respect provider retry headers.
-   Use circuit breakers for repeatedly failing dependencies.
-   Keep durable state in PostgreSQL.
-   Make consumers idempotent.
-   Expose dependency health and queue backlog metrics.

## 16.2 Example retry schedule

A conceptual retry schedule:

    Attempt   Base delay
  --------- ------------
          1     1 second
          2    2 seconds
          3    4 seconds
          4    8 seconds
          5   16 seconds

Add random jitter and a maximum delay. The actual policy should be
configurable by operation and provider.

## 16.3 Failure matrix

  -----------------------------------------------------------------------
  Failure                             Expected behavior
  ----------------------------------- -----------------------------------
  PostgreSQL unavailable              Return controlled 503; do not claim
                                      durable acceptance

  Redis unavailable                   Preserve outbox rows; pause/retry
                                      publishing

  Embedding provider 429              Respect retry-after; backoff with
                                      jitter

  Embedding provider timeout          Retry within bounded policy

  LLM provider unavailable            Circuit breaker; optional
                                      configured fallback

  Worker crashes                      Reclaim pending message; verify
                                      durable job lease

  Duplicate event                     Idempotent processing

  Object upload succeeds, DB fails    Attempt object cleanup;
                                      reconciliation handles orphan

  DB commit succeeds, Redis publish   Outbox remains unpublished and is
  fails                               retried

  Invalid PDF                         Mark permanent failure with safe
                                      code

  Client disconnects during SSE       Cancel upstream work when safe

  Tenant authorization fails          Deny access; do not reveal resource
                                      details

  Disk/storage quota reached          Reject or pause upload with
                                      controlled error

  Provider returns malformed vectors  Reject batch; do not persist
                                      invalid embeddings
  -----------------------------------------------------------------------

## 16.4 Idempotency

Use stable identifiers and database constraints.

Examples:

-   Upload idempotency key for client retries.
-   Unique tenant/content hash for document deduplication.
-   Stable outbox event ID.
-   Stable ingestion job ID.
-   Unique chunk position per document processing version.
-   Worker claim token/lease.
-   Provider request ID where available.

"Exactly once" processing should not be promised. Design for
at-least-once delivery and idempotent effects.

## 16.5 Graceful shutdown

API process:

1.  Stop accepting new requests.
2.  Mark readiness false.
3.  Allow in-flight requests a bounded drain period.
4.  Close SSE streams or allow a configured grace period.
5.  Cancel remaining work.
6.  Close database and Redis clients cleanly.

Worker process:

1.  Stop reading new messages.
2.  Finish or safely release in-flight jobs within a deadline.
3.  Do not acknowledge unfinished work.
4.  Let leases expire or explicitly release claims.
5.  Flush telemetry.
6.  Exit.

------------------------------------------------------------------------

# 17. Caching strategy

Caching must not weaken tenant isolation.

Potential cache entries:

-   Query embedding by normalized query + embedding model/version.
-   Tenant-specific search result cache.
-   Tenant usage summaries.
-   Short-lived organization metadata.
-   Provider/model capability metadata.

Cache key examples:

``` text
query-embedding:{model}:{query_hash}
search:{tenant_id}:{query_hash}:{filters_hash}
usage:{tenant_id}:{period}
```

Rules:

-   Include tenant ID in every tenant-specific cache key.
-   Include model/index version in AI/search cache keys.
-   Invalidate or version cache entries when documents change.
-   Avoid caching sensitive RAG answers unless policy and authorization
    semantics are clear.
-   Never use a global cache key for tenant-private content.
-   Apply TTLs and bounded memory policies.

Semantic caching is not part of the first core search release. It can be
added after correctness and cache isolation are proven.

------------------------------------------------------------------------

# 18. Observability and operational telemetry

## 18.1 Instrumentation

Use OpenTelemetry-compatible traces, metrics, and structured logs.

Trace important spans:

-   HTTP request.
-   Authentication and membership validation.
-   Database transaction.
-   Object upload/download.
-   Outbox publication.
-   Redis message processing.
-   Document parsing.
-   Chunking.
-   Embedding provider call.
-   Hybrid search.
-   LLM call and stream.
-   Usage persistence.

Propagate a request/correlation ID through asynchronous job metadata
where appropriate.

## 18.2 Metrics

### API metrics

-   Request count by route/status.
-   Request duration histogram.
-   Active requests.
-   4xx/5xx rate.
-   Rate-limit rejections.
-   SSE active streams and duration.

### Database metrics

-   Pool connections in use/idle.
-   Pool acquisition wait time.
-   Query duration.
-   Transaction duration.
-   Deadlocks and lock waits.
-   Slow query count.
-   Database connection errors.

### Ingestion metrics

-   Queue length.
-   Oldest pending event age.
-   Outbox backlog.
-   Jobs by status.
-   Processing duration.
-   Retry count.
-   Failure count by code.
-   Chunks created.
-   Embedding provider latency and 429 count.

### AI metrics

-   Embedding input volume.
-   LLM input/output tokens.
-   Provider latency.
-   Time to first token.
-   Estimated cost.
-   Fallback count.
-   Circuit-breaker state.

Avoid high-cardinality labels such as raw user IDs, document IDs,
request IDs, or arbitrary query strings in Prometheus metrics. Put those
in traces/logs with appropriate privacy controls.

## 18.3 Dashboards

Create dashboards for:

1.  API health and latency.
2.  PostgreSQL and pool health.
3.  Redis Streams and outbox health.
4.  Ingestion throughput and failures.
5.  Embedding provider health.
6.  RAG latency, token usage, and cost.
7.  Tenant usage and quota consumption.

## 18.4 Alerts

Initial alert candidates:

-   API 5xx rate above threshold.
-   P95 latency above SLO for a sustained window.
-   PostgreSQL pool saturation.
-   Outbox oldest-event age too high.
-   Redis pending messages growing.
-   Ingestion failure rate elevated.
-   Embedding/LLM provider error rate elevated.
-   Storage nearing quota.
-   No successful database backup within expected window.

Define thresholds after observing a baseline rather than selecting
arbitrary values.

------------------------------------------------------------------------

# 19. Testing strategy

## 19.1 Unit tests

Test domain rules without external services:

-   Membership role transitions.
-   Authorization decisions.
-   Document state machine.
-   Retry classification.
-   Chunk boundaries.
-   RRF score calculation.
-   Usage aggregation.
-   Quota decisions.

## 19.2 Integration tests

Use disposable PostgreSQL and Redis instances, preferably through
Testcontainers or an equivalent isolated test environment.

Test:

-   Migrations from an empty database.
-   User and tenant creation.
-   Membership checks.
-   RLS behavior.
-   Tenant-scoped transactions.
-   Document deduplication.
-   Outbox publication.
-   Redis Streams consumer behavior.
-   Chunk persistence.
-   Hybrid search query.
-   RAG provider adapter behavior using a mock provider.

## 19.3 Security tests

Create at least two tenants:

-   Tenant A with User A.
-   Tenant B with User B.

Attempt cross-tenant:

-   Reads.
-   Inserts with another tenant ID.
-   Updates.
-   Deletes.
-   Search retrieval.
-   Document status access.
-   Job retry.
-   API key use.
-   Usage and audit reads.

Also test:

-   Missing tenant context.
-   Invalid tenant context.
-   Pool reuse across tenants.
-   Concurrent mixed-tenant requests.
-   Role escalation attempts.
-   Revoked membership.
-   Expired/revoked tokens.
-   SQL injection payloads.
-   Malicious file inputs.
-   Prompt injection in retrieved documents.

Expected result: no cross-tenant data exposure or unauthorized mutation.

## 19.4 Concurrency and failure tests

Simulate:

-   Many concurrent uploads of identical content.
-   Worker crash after message delivery.
-   Worker crash after DB commit but before acknowledgement.
-   Publisher crash after Redis publish but before marking outbox event.
-   Redis restart.
-   Provider 429 and timeout.
-   PostgreSQL connection interruption.
-   Object storage failure.
-   Client disconnect during SSE.
-   Graceful shutdown with active jobs.

## 19.5 Performance tests

Tools:

-   `k6` for HTTP load testing.
-   SQL `EXPLAIN (ANALYZE, BUFFERS)` for database plans.
-   Custom retrieval evaluation harness for Recall@k.
-   Rust benchmarks for CPU-heavy chunking/RRF code where useful.

Benchmark separate paths:

1.  Health endpoint.
2.  Authenticated tenant-scoped read.
3.  Document metadata list.
4.  Upload initiation.
5.  Hybrid search.
6.  RAG retrieval-only.
7.  Full RAG time-to-first-token.
8.  Ingestion throughput.

Report:

-   Hardware and deployment topology.
-   Dataset size and tenant distribution.
-   Request mix.
-   Warm/cold cache conditions.
-   Concurrency.
-   Test duration.
-   P50/P95/P99.
-   Throughput.
-   Error rate.
-   Resource consumption.

Do not report only average latency.

------------------------------------------------------------------------

# 20. Deployment and infrastructure

## 20.1 Local development

Docker Compose should provide:

-   PostgreSQL with pgvector.
-   Redis.
-   MinIO or another S3-compatible local object store.
-   Optional OpenTelemetry Collector.
-   Optional Prometheus and Grafana.

The Rust API and worker can run locally or as Compose services.

Provide `.env.example` with variable names and safe placeholders only.

## 20.2 Container processes

Build at least two runtime targets:

-   `tenant-saas-api`
-   `tenant-saas-worker`

They may use the same source crate with separate binaries or
feature/argument-based process modes. Separate binaries are generally
clearer for deployment and resource limits.

Use multi-stage Docker builds and pin supported base image versions. Do
not promise a specific image size until it has been measured.

## 20.3 Migrations

Run migrations as a controlled deployment job using a migration-capable
database role.

Do not have every API replica race to execute migrations on startup.

Deployment order:

1.  Provision dependencies.
2.  Verify database connectivity.
3.  Run backward-compatible migrations.
4.  Deploy API and worker.
5.  Verify readiness.
6.  Enable new behavior/feature flags.
7.  Remove obsolete schema only in a later compatible migration.

Use expand-and-contract migrations for changes that must coexist with
old and new application versions.

## 20.4 Health endpoints

-   `/health/live`: process is alive.
-   `/health/ready`: process can serve its intended workload.
-   `/health/dependencies`: optional diagnostic endpoint with controlled
    access.

Readiness should check only dependencies required for that process to
perform its job. Avoid making liveness fail simply because a temporary
external provider is unavailable.

## 20.5 CI/CD

Suggested GitHub Actions pipeline:

1.  Format check: `cargo fmt --check`.
2.  Lint: `cargo clippy --all-targets --all-features -- -D warnings`.
3.  Unit tests.
4.  Integration tests with PostgreSQL/Redis.
5.  Migration validation.
6.  Build release binaries.
7.  Build container images.
8.  Container/security scanning.
9.  Publish versioned images.
10. Deploy to staging.
11. Smoke and migration compatibility tests.
12. Manual or policy-controlled production promotion.

Use dependency auditing and secret scanning. Pin action versions and
avoid printing secrets in CI logs.

## 20.6 Backups and recovery

Define:

-   PostgreSQL backup schedule.
-   Point-in-time recovery if supported by the chosen hosting model.
-   Object storage versioning/backup policy.
-   Redis persistence expectations (Redis is not the source of truth).
-   Recovery time objective (RTO).
-   Recovery point objective (RPO).
-   Regular restore drills.

A backup is not verified until a restore has been tested.

------------------------------------------------------------------------

# 21. Configuration and secrets

Use typed configuration loaded at process startup.

Example variable names:

``` dotenv
APP_ENV=development
APP_HOST=0.0.0.0
APP_PORT=8080
RUST_LOG=info

DATABASE_URL=postgres://...
DATABASE_MAX_CONNECTIONS=20
DATABASE_ACQUIRE_TIMEOUT_SECS=5

REDIS_URL=redis://...
REDIS_STREAM_INGESTION=ingestion-events
REDIS_CONSUMER_GROUP=ingestion-workers

OBJECT_STORAGE_ENDPOINT=http://localhost:9000
OBJECT_STORAGE_BUCKET=tenant-documents
OBJECT_STORAGE_REGION=us-east-1
OBJECT_STORAGE_ACCESS_KEY=...
OBJECT_STORAGE_SECRET_KEY=...

JWT_ISSUER=...
JWT_AUDIENCE=...
JWT_PUBLIC_KEY_PATH=...

EMBEDDING_PROVIDER=...
EMBEDDING_MODEL=...
EMBEDDING_DIMENSION=1536
EMBEDDING_API_KEY=...

LLM_PROVIDER=...
LLM_MODEL=...
LLM_API_KEY=...

MAX_UPLOAD_BYTES=20971520
CHUNK_SIZE_TOKENS=512
CHUNK_OVERLAP_TOKENS=64
EMBEDDING_BATCH_SIZE=32
```

This is illustrative. Do not commit real secrets. Validate all
configuration at startup and fail fast on missing or invalid required
values.

Prefer a secret manager or orchestrator-provided secret injection in
production.

------------------------------------------------------------------------

# 22. Architecture Decision Records (ADRs)

Create one ADR per important decision. Each ADR should include:

-   Context.
-   Decision.
-   Alternatives considered.
-   Positive consequences.
-   Negative consequences.
-   Security/operational implications.
-   Revisit conditions.

Initial ADR list:

  ADR       Decision
  --------- -------------------------------------------------------------
  ADR-001   Modular monolith rather than microservices
  ADR-002   Rust + Axum + Tokio
  ADR-003   Shared PostgreSQL schema with RLS and micro-scoped tx
  ADR-004   SQLx for database access
  ADR-005   pgvector 0.8.0+ with relaxed iterative scans
  ADR-006   PostgreSQL FTS for initial lexical retrieval
  ADR-007   RRF for hybrid ranking
  ADR-008   Redis Streams for ingestion transport
  ADR-009   Transactional Outbox as supervised worker task (SKIP LOCKED)
  ADR-010   object_store abstraction for storage (local/MinIO/S3)
  ADR-011   Separate API and worker processes
  ADR-012   SSE for streamed RAG responses
  ADR-013   Provider abstraction for embeddings and LLMs
  ADR-014   OpenTelemetry for observability
  ADR-015   tiktoken-rs for model-compatible token-aware chunking

## 22.1 Production Problems Prevented by Architectural Design

These decisions prevent specific failure modes:

| Engineering Decision | Problem Prevented | Technical Mechanism |
| :--- | :--- | :--- |
| **Micro-Scoped Transactions** | Connection pool exhaustion | External I/O (S3 upload, LLM generation) is strictly decoupled from PostgreSQL transaction boundaries. |
| **Transactional Outbox (Supervised Task)** | Lost background jobs & queue drift | Events persist atomically with domain entities in PostgreSQL; worker polls via `SKIP LOCKED` and publishes to Redis Streams. |
| **Iterative Vector Scans (`pgvector 0.8.0+`)** | Poor search recall under tenant filtering | `SET LOCAL hnsw.iterative_scan = 'relaxed'` ensures nearest-neighbor graph traversal continues until sufficient tenant-matched rows are found. |
| **Storage Abstraction (`object_store`)** | Vendor lock-in & test complexity | `FileStorage` trait allows transparent swapping between local filesystem (dev/unit tests), MinIO (CI), and AWS S3 (production). |
| **Token-Aware Chunking (`tiktoken-rs`)** | Runtime API rejection & context loss | Chunk boundaries are bounded by exact BPE token counts (500 tokens, 75 overlap) matching the embedding model's context window. |

## 22.2 ADR-003 example: shared database with RLS and micro-scoped transactions

**Context:** The platform must support multiple organizations without
operating a separate database for every tenant.

**Decision:** Use shared tables with tenant IDs, PostgreSQL RLS,
tenant-scoped transactions, composite foreign keys, and
application-level membership authorization.

**Alternatives:**

-   Database per tenant.
-   Schema per tenant.
-   Shared tables without RLS.

**Consequences:**

-   Simplifies migrations and early operations.
-   Requires strict transaction and role discipline.
-   Requires cross-tenant security testing.
-   May require partitioning or dedicated databases for very large
    tenants later.

**Revisit when:** Tenant-specific regulatory, residency, performance, or
isolation requirements justify a different topology.

------------------------------------------------------------------------

# 23. Implementation roadmap

Estimated duration: 8--10 weeks at approximately 15--20 focused hours
per week. The estimate assumes one developer and a learning-oriented
implementation. Provider integration, security hardening, and
infrastructure debugging may extend the schedule.

## Phase 1 --- Foundation (Week 1)

Deliverables:

-   Cargo workspace.
-   Rust toolchain pin.
-   Axum application.
-   SQLx pool.
-   PostgreSQL migrations.
-   Typed configuration.
-   Shared error handling.
-   Docker Compose.
-   Structured logging.
-   Liveness/readiness endpoints.
-   CI format, lint, and test checks.

Exit criteria:

-   Fresh clone can start the local dependencies and API.
-   Migrations apply successfully.
-   Health checks work.
-   CI passes.

## Phase 2 --- Identity and tenancy (Week 2)

Deliverables:

-   Users and tenants.
-   Membership model.
-   Registration/login or OIDC integration.
-   Role model.
-   Tenant context.
-   Scoped transaction helper.
-   RLS policies.
-   Cross-tenant integration tests.

Exit criteria:

-   A user can join more than one tenant.
-   Role checks work.
-   Tenant A cannot read or modify Tenant B data.
-   Pool reuse tests show no tenant context leakage.

## Phase 3 --- Document management (Week 3)

Deliverables:

-   Upload endpoint.
-   Streaming upload.
-   File validation.
-   SHA-256 calculation.
-   Object storage adapter.
-   Document metadata.
-   Deduplication constraint.
-   Document status API.
-   Orphan-object cleanup path.

Exit criteria:

-   Valid supported files upload.
-   Invalid/oversized files are rejected.
-   Concurrent duplicate uploads resolve deterministically.
-   Original file remains private.

## Phase 4 --- Reliable ingestion (Weeks 4--5)

Deliverables:

-   Ingestion job table.
-   Transactional outbox.
-   Outbox publisher.
-   Redis Streams consumer group.
-   Worker process.
-   Parser and normalizer.
-   Token-aware chunker.
-   Embedding provider interface.
-   Batch embedding.
-   Idempotent chunk persistence.
-   Retry and recovery logic.

Exit criteria:

-   API acceptance is durable.
-   Publisher/worker crashes do not lose durable work.
-   Duplicate events do not create duplicate logical chunks.
-   Failed jobs can be retried safely.

## Phase 5 --- Hybrid search (Week 6)

Deliverables:

-   pgvector extension and vector schema.
-   PostgreSQL FTS.
-   HNSW baseline index.
-   Vector candidate query.
-   Lexical candidate query.
-   RRF implementation.
-   Search endpoint.
-   Relevance evaluation dataset.
-   Search benchmark report.

Exit criteria:

-   Search results are tenant-scoped.
-   Exact keyword and semantic queries work.
-   Recall@10 and latency are measured.
-   Query plans are reviewed.

## Phase 6 --- RAG (Week 7)

Deliverables:

-   Context retrieval.
-   Context token budget.
-   Prompt builder.
-   LLM provider adapter.
-   SSE streaming.
-   Citations.
-   Provider timeouts and fallback policy.
-   Usage tracking.
-   Prompt-injection handling controls.

Exit criteria:

-   Answers include valid citations where retrieved sources support
    them.
-   No cross-tenant context can enter a request.
-   Streaming handles provider errors and client disconnects.

## Phase 7 --- Production hardening (Weeks 8--9)

Deliverables:

-   Rate limiting.
-   Quotas.
-   Audit events.
-   OpenTelemetry traces/metrics.
-   Dashboards and alerts.
-   Security tests.
-   Load tests.
-   Graceful shutdown.
-   Backup and restore procedure.
-   Failure-injection tests.

Exit criteria:

-   Critical failure scenarios are tested.
-   No known cross-tenant leakage in the defined test suite.
-   Operational signals are visible.
-   Performance results are documented.

## Phase 8 --- Portfolio delivery (Week 10)

Deliverables:

-   Cloud deployment.
-   CI/CD deployment pipeline.
-   Public README.
-   Architecture diagrams.
-   ADRs.
-   API documentation.
-   Runbooks.
-   Benchmark report.
-   Short demo video or walkthrough.
-   Technical case study.

Exit criteria:

-   Another developer can reproduce the environment from the README.
-   A reviewer can understand the architecture and trade-offs.
-   Performance claims are supported by benchmark artifacts.

------------------------------------------------------------------------

# 24. Definition of done

## Product functionality

-   [ ] Users can register/authenticate.
-   [ ] Users can create and join multiple tenants.
-   [ ] Membership roles are enforced.
-   [ ] Documents can be uploaded securely.
-   [ ] Documents are processed asynchronously.
-   [ ] Failed ingestion can be recovered.
-   [ ] Hybrid search works.
-   [ ] RAG answers stream over SSE.
-   [ ] Citations reference real retrieved chunks.
-   [ ] Usage and audit events are recorded.

## Security and correctness

-   [ ] RLS enabled on all tenant-owned tables.
-   [ ] Runtime DB role cannot bypass RLS.
-   [ ] Tenant transaction helper is centralized.
-   [ ] Cross-tenant relational references are constrained.
-   [ ] Concurrent duplicate upload is safe.
-   [ ] Outbox publishing is retryable.
-   [ ] Worker processing is idempotent.
-   [ ] Upload parsing is resource-limited.
-   [ ] Secrets and PII are not leaked in logs.
-   [ ] Prompt injection is treated as untrusted content.
-   [ ] Security tests cover reads, writes, search, and background jobs.

## Engineering quality

-   [ ] Unit tests.
-   [ ] Integration tests.
-   [ ] Security tests.
-   [ ] Load tests.
-   [ ] CI pipeline.
-   [ ] Docker build.
-   [ ] Database migrations.
-   [ ] Structured logs.
-   [ ] Metrics and traces.
-   [ ] Graceful shutdown.
-   [ ] Backup/restore documentation.
-   [ ] API documentation.
-   [ ] Architecture diagrams.
-   [ ] ADRs.
-   [ ] Operational runbooks.

## Portfolio evidence

-   [ ] Public source repository.
-   [ ] Reproducible local setup.
-   [ ] Deployed demonstration.
-   [ ] Architecture case study.
-   [ ] Search relevance report.
-   [ ] Load benchmark report.
-   [ ] Security testing summary.
-   [ ] Documented limitations and future work.

------------------------------------------------------------------------

# 25. Risks, trade-offs, and future evolution

  -----------------------------------------------------------------------
  Risk / trade-off        Initial mitigation      Revisit condition
  ----------------------- ----------------------- -----------------------
  Shared database noisy   Tenant quotas and       Large tenant dominates
  neighbor                workload metrics        resources

  RLS implementation      Central transaction     Higher assurance
  errors                  helper and security     requirements
                          tests                   

  Redis outage            Durable outbox in       Queue
                          PostgreSQL              throughput/retention
                                                  needs exceed Redis

  Embedding provider cost Batching, quotas, usage High volume or provider
                          tracking                cost pressure

  Vector filtering        Benchmark HNSW with     Recall/latency targets
  performance             tenant filters          fail

  PDF parser              Isolation and resource  More formats or higher
  vulnerabilities         limits                  threat exposure

  Model behavior changes  Store model/version     Provider/model
                          metadata and evaluate   migration

  Cache staleness         Versioned keys and      Search/content update
                          invalidation            frequency rises

  Single-region failure   Backups and recovery    Availability
                          drills                  requirements justify
                                                  multi-region
  -----------------------------------------------------------------------

## 25.1 Future enhancements

After the first stable release:

-   Genuine BM25 retrieval through a suitable search engine/extension.
-   Semantic response caching with tenant-aware keys.
-   Document-level ACLs beyond tenant membership.
-   Advanced OCR for scanned PDFs.
-   Re-embedding and zero-downtime index migration.
-   Kafka if event volume/retention/consumer requirements justify it.
-   Dedicated tenant databases for exceptional isolation needs.
-   Kubernetes deployment and autoscaling.
-   Temporal for durable multi-step business workflows.
-   Advanced anomaly detection for usage/cost.
-   Data residency and customer-managed encryption options.

Do not add these simply to increase the technology count. Add them when
a requirement or measured constraint justifies the complexity.

------------------------------------------------------------------------

# 26. Operational runbooks

At minimum, maintain runbooks for:

1.  PostgreSQL unavailable.
2.  Redis unavailable.
3.  Outbox backlog growing.
4.  Redis pending messages stuck.
5.  Worker repeatedly failing one document.
6.  Embedding provider rate-limited.
7.  LLM provider outage.
8.  Tenant isolation/security incident.
9.  Object storage orphan cleanup.
10. Database backup restoration.
11. Embedding model migration.
12. Tenant deletion and data retention.

Each runbook should state:

-   Symptoms and alert.
-   Impact.
-   Diagnostic queries/dashboards.
-   Safe mitigation.
-   Recovery steps.
-   Data integrity checks.
-   Escalation criteria.
-   Follow-up prevention work.

------------------------------------------------------------------------

# 27. Engineering principles to preserve during implementation

1.  **Do not trust client-supplied tenant IDs.** Validate membership and
    construct a trusted context.
2.  **Do not execute tenant queries outside a scoped transaction.**
3.  **Do not hold database transactions open across external I/O.** Keep
    transactions micro-scoped (stream file to S3 first; retrieve context,
    commit, then call LLM).
4.  **Do not make Redis the source of truth.** Persist durable job state
    and outbox events in PostgreSQL.
5.  **Do not assume exactly-once delivery.** Design idempotent
    consumers with worker claim leases.
6.  **Do not deploy a separate service for outbox publishing.** Supervise
    it as a background Tokio task in the worker using `SKIP LOCKED`.
7.  **Do not use pgvector < 0.8.0 with tenant filtering.** Pin pgvector 0.8.0+
    and enable `SET LOCAL hnsw.iterative_scan = 'relaxed'`.
8.  **Do not couple directly to vendor storage SDKs.** Use the `object_store`
    abstraction trait to support local fs, MinIO, and S3 seamlessly.
9.  **Do not chunk documents by character counts.** Use `tiktoken-rs` with
    the exact tokenizer matching the embedding model (500 tokens, 75 overlap).
10. **Do not parse untrusted files in an unconstrained API process.**
11. **Do not label PostgreSQL `ts_rank_cd` as BM25.**
12. **Do not mix embedding models or dimensions without versioning.**
13. **Do not claim performance without reproducible benchmarks.**
14. **Do not start with microservices when a modular monolith is
    sufficient.**
15. **Do not run schema migrations independently from every API
    replica.**
16. **Do not log secrets, raw access tokens, or sensitive document
    content.**
17. **Do not allow retrieved document instructions to override
    application policy.**
18. **Do not acknowledge a background message before its result is
    durably recorded.**
19. **Do not treat a successful backup job as proof of recoverability;
    test restoration.**
20. **Document trade-offs and revisit decisions when workload evidence
    changes.**

------------------------------------------------------------------------

# 28. Reference implementation stack

The following is the intended baseline. Pin exact versions in
`Cargo.toml`, `Cargo.lock`, container images, and infrastructure
configuration at implementation time.

  Layer                   Technology
  ----------------------- -------------------------------------------------------------
  Language                Rust (latest stable, pinned in rust-toolchain.toml)
  HTTP framework          Axum
  Async runtime           Tokio
  Database access         SQLx
  Database                PostgreSQL 16
  Vector retrieval        pgvector 0.8.0+ (hnsw.iterative_scan = 'relaxed')
  Lexical retrieval       PostgreSQL FTS (tsvector, GIN index)
  Ranking                 Reciprocal Rank Fusion (RRF, k=60)
  Cache / job transport   Redis 7
  Messaging / Outbox      Redis Streams + Transactional Outbox in PostgreSQL (SKIP LOCKED)
  Object storage          object_store crate (local filesystem, MinIO, AWS S3)
  Tokenization / Chunker  tiktoken-rs (token-aware sliding window: 500 tokens, 75 overlap)
  Authentication          JWT and/or OIDC
  Password hashing        Argon2id
  API specification       OpenAPI / Utoipa
  Testing                 Cargo Test, SQLx tests, Testcontainers
  Load testing            k6
  Observability           OpenTelemetry, Prometheus, Grafana
  Containerization        Docker (multi-stage with cargo-chef)
  CI/CD                   GitHub Actions
  Future orchestration    Kubernetes / Terraform when justified
  Future orchestration    Kubernetes / Terraform when justified

------------------------------------------------------------------------

# 29. Final architecture summary

The first project is a Rust-based, multi-tenant document intelligence
platform built as a modular monolith with independently deployable API
and worker processes.

Its core engineering story is:

-   **Security:** authenticated membership, RBAC, tenant-scoped
    transactions, PostgreSQL RLS, composite foreign keys, and security
    tests.
-   **Reliability:** PostgreSQL as the source of truth, Transactional
    Outbox, Redis Streams, at-least-once delivery, idempotent workers,
    retries, and recovery.
-   **Search:** pgvector semantic retrieval, PostgreSQL FTS lexical
    retrieval, RRF ranking, and relevance benchmarks.
-   **AI:** provider abstractions, bounded RAG context, SSE streaming,
    citations, prompt-injection controls, and usage tracking.
-   **Operations:** Docker, CI/CD, migrations, OpenTelemetry, metrics,
    logs, traces, backups, runbooks, and load tests.

The goal is not to claim that the project is enterprise-grade because it
uses enterprise technologies. The goal is to demonstrate, through
implementation and evidence, that its security, reliability,
performance, and operational behavior have been engineered and tested
deliberately.

**Build order:** establish identity and tenant isolation first; then
document management; then reliable ingestion; then hybrid search; then
RAG; finally harden, benchmark, deploy, and document the system.
