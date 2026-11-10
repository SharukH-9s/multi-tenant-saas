# ADR-002: Rust + Axum + Tokio

**Status:** Accepted

## Context
The service handles concurrent I/O (HTTP, PostgreSQL, Redis, object storage,
LLM providers) and processes untrusted documents, so memory safety and
predictable resource use matter.

## Decision
Use Rust with Tokio as the async runtime and Axum (with Tower middleware) as
the web framework.

## Alternatives considered
- Go with `net/http` or Gin.
- Python with FastAPI.
- Node.js with NestJS.

## Positive consequences
- Memory safety and strong typing catch many errors at compile time.
- Tower middleware (request IDs, tracing, timeouts) composes cleanly.
- Low, predictable resource use per request.

## Negative consequences
- Longer compile times and a steeper learning curve.
- A smaller ecosystem of AI/ML libraries than Python.

## Security / operational implications
- Fewer classes of memory-safety vulnerabilities.
- The pinned toolchain (`rust-toolchain.toml`) keeps local and CI builds identical.

## Revisit when
Library support for a required AI capability exists only outside Rust and
cannot be reached through an HTTP provider abstraction.
