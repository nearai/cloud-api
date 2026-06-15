-- Auto-reap idle client sessions at the server so connections orphaned by a
-- crashed or recycled cloud-api instance do not accumulate and exhaust the
-- cluster's max_connections.
--
-- Incident 2026-06-15: after a prod deploy, crash-looping/recycled instances
-- left behind dozens of `idle` client backends (no server-side idle timeout +
-- dead-peer TCP backends linger for minutes). The leader filled to
-- max_connections, so even a single healthy instance could not acquire a
-- connection ("FATAL: sorry, too many clients already"). These settings make
-- Postgres close abandoned sessions on its own.
--
-- Safe with our pooling: deadpool re-validates a connection on checkout
-- (Fast recycling -> is_closed()), so a warm pooled connection that the server
-- reaps after going idle is simply discarded and re-created on next use — no
-- error surfaces to the app. 300s is well above normal inter-query gaps under
-- load, so only genuinely-abandoned sessions get reaped.
--
-- idle_session_timeout requires PostgreSQL >= 14 (the cluster is PG16/Spilo-16);
-- the version guard keeps this migration a no-op rather than an error on any
-- older node, so it can never wedge startup.
DO $$
BEGIN
    IF current_setting('server_version_num')::int >= 140000 THEN
        EXECUTE format(
            'ALTER DATABASE %I SET idle_session_timeout = %L',
            current_database(), '300s'
        );
    END IF;

    -- Available since PG 9.6; reaps sessions stuck idle-in-transaction
    -- (the classic connection leak) much sooner.
    EXECUTE format(
        'ALTER DATABASE %I SET idle_in_transaction_session_timeout = %L',
        current_database(), '60s'
    );
END
$$;
