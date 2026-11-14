-- Migration: 005_ingestion_sweeper.sql
-- Purpose: sweep_ingestion_jobs(), the one cross-tenant recovery function.
-- Runs as db_owner. Forward-only (no down migration).
--
-- Design: ADR-007 §4 and phase4_implementation_plan.md (D1, D2, D4).
--
-- The function is SECURITY DEFINER and owned by db_owner. db_owner owns the
-- tables and they do not use FORCE ROW LEVEL SECURITY (D2), so the function
-- sees every tenant's rows while app_runtime stays fully bound by RLS.
-- Same template as the definer functions in 002_identity.sql.
--
-- Returns job ids and an action only. Never document content.

CREATE FUNCTION sweep_ingestion_jobs(p_batch integer, p_pending_stale_secs integer)
RETURNS TABLE (job_id uuid, tenant_id uuid, action text)
LANGUAGE plpgsql VOLATILE SECURITY DEFINER
SET search_path = pg_catalog, public
AS $$
#variable_conflict use_column
BEGIN
    -- Rule 0: active jobs whose document was deleted -> cancelled
    RETURN QUERY
    WITH c AS (
        SELECT j.id FROM ingestion_jobs j
          JOIN documents d ON d.tenant_id = j.tenant_id AND d.id = j.document_id
         WHERE j.status IN ('pending', 'processing', 'retry_scheduled')
           AND d.deleted_at IS NOT NULL
         LIMIT p_batch FOR UPDATE OF j SKIP LOCKED),
    jc AS (
        UPDATE ingestion_jobs j
           SET status = 'cancelled', lease_token = NULL, locked_until = NULL,
               locked_by = NULL, completed_at = now()
          FROM c WHERE j.id = c.id
        RETURNING j.id, j.tenant_id)
    SELECT jc.id, jc.tenant_id, 'cancelled'::text FROM jc;

    -- Rule A: expired lease, attempts exhausted -> failed (job and document)
    RETURN QUERY
    WITH c AS (
        SELECT j.id, j.tenant_id, j.document_id FROM ingestion_jobs j
          JOIN documents d ON d.tenant_id = j.tenant_id AND d.id = j.document_id
         WHERE j.status = 'processing' AND j.locked_until < now()
           AND j.attempts >= j.max_attempts AND d.deleted_at IS NULL
         ORDER BY j.locked_until LIMIT p_batch
           FOR UPDATE OF j, d SKIP LOCKED),
    jf AS (
        UPDATE ingestion_jobs j
           SET status = 'failed', lease_token = NULL, locked_until = NULL,
               locked_by = NULL, last_error_code = 'ATTEMPTS_EXHAUSTED',
               last_error_at = now(), completed_at = now()
          FROM c WHERE j.id = c.id
        RETURNING j.id, j.tenant_id),
    df AS (
        UPDATE documents d
           SET status = 'failed', failure_code = 'ATTEMPTS_EXHAUSTED',
               failure_detail = 'worker did not finish within the lease on the final attempt'
          FROM c WHERE d.tenant_id = c.tenant_id AND d.id = c.document_id)
    SELECT jf.id, jf.tenant_id, 'failed'::text FROM jf;

    -- Rule B: expired lease with attempts left, due retry, or stale pending
    --         -> pending + a new outbox event
    RETURN QUERY
    WITH c AS (
        SELECT j.id, j.tenant_id, j.document_id, (j.status = 'processing') AS was_leased
          FROM ingestion_jobs j
          JOIN documents d ON d.tenant_id = j.tenant_id AND d.id = j.document_id
         WHERE d.deleted_at IS NULL
           AND (   (j.status = 'processing'      AND j.locked_until < now())
                OR (j.status = 'retry_scheduled' AND j.available_at <= now())
                OR (j.status = 'pending'         AND j.last_enqueued_at
                                                     < now() - make_interval(secs => p_pending_stale_secs)))
           AND NOT EXISTS (SELECT 1 FROM outbox_events o
                            WHERE o.aggregate_id = j.id
                              AND o.published_at IS NULL AND o.dead_lettered_at IS NULL)
         LIMIT p_batch FOR UPDATE OF j, d SKIP LOCKED),
    jr AS (
        UPDATE ingestion_jobs j
           SET status = 'pending', lease_token = NULL, locked_until = NULL,
               locked_by = NULL, last_enqueued_at = now()
          FROM c WHERE j.id = c.id
        RETURNING j.id, j.tenant_id, j.document_id),
    dr AS (
        UPDATE documents d SET status = 'retry_scheduled'
          FROM c WHERE c.was_leased AND d.tenant_id = c.tenant_id AND d.id = c.document_id),
    ob AS (
        INSERT INTO outbox_events (tenant_id, aggregate_type, aggregate_id, event_type, payload)
        SELECT jr.tenant_id, 'ingestion_job', jr.id, 'ingestion.requested',
               jsonb_build_object('v', 1, 'job_id', jr.id, 'tenant_id', jr.tenant_id,
                                  'document_id', jr.document_id)
          FROM jr)
    SELECT jr.id, jr.tenant_id, 'requeued'::text FROM jr;
END $$;

REVOKE EXECUTE ON FUNCTION sweep_ingestion_jobs(integer, integer) FROM PUBLIC;
GRANT  EXECUTE ON FUNCTION sweep_ingestion_jobs(integer, integer) TO app_runtime;
