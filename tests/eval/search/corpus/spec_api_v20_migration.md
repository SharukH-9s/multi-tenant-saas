# Migrating from API v2.0 to API v2.1

Most integrations need only small changes when moving from API v2.0 to API v2.1.

## What changed

The field `created` was renamed to `created_at` and now uses ISO 8601 timestamps. Offset pagination was removed in favour of cursors.

## Deprecation timeline

API v2.0 is deprecated and stops working on 30 June 2026. Until then both versions run side by side.

## Testing the migration

Use the sandbox environment, which mirrors API v2.1, to run your integration tests before switching production traffic.
