-- Migration 0.131.0 → 0.140.0: W3C SPARQL conformance fixes (VAL-358, port of
-- upstream trickle-labs 13aad42c under ADR-001).
--
-- Version jumps 0.131 → 0.140 per the fork's independent numbering policy:
-- the 0.132–0.136 numbers were double-spent by the upstream line with
-- different content, so this line skips past them.
--
-- New SQL surface required by the patched SPARQL engine:
--   FILTER-IRI: comparisons against a named-node constant that is ABSENT from
--     the dictionary now translate to a deterministic dictionary id
--     (dictionary::encode) instead of a NULL scalar subquery.  Previously
--     `FILTER(?p != <absent-iri>)` evaluated as `?p != NULL` → NULL → the
--     filter silently dropped EVERY row (reproduced on 0.131.0 with the
--     engine's outgoing/incoming kg_query shape; regression test
--     tests/pg_regress/sql/v0140_sparql_filter_absent_iri.sql).
--   AGG-NUMTYPE: SUM/AVG/MIN/MAX over typed numerics decode through
--     pg_ripple.decode_numeric_spi and type through the new
--     pg_ripple.numeric_type_code_spi (xsd:decimal → 1, integer family → 0,
--     float/double → 2, else -1), replacing per-row dictionary subqueries
--     that missed xsd:long/int/short/byte and mistyped unknown datatypes.
--   SHA1-NATIVE: SPARQL SHA1() maps to _pg_ripple.sha1_hex (IMMUTABLE,
--     STRICT, PARALLEL SAFE) instead of pgcrypto's digest(); SHA384/SHA512
--     map to the native sha384()/sha512() built-ins.
--
-- The function bodies live in the shared library.  The generated
-- pg_ripple--0.140.0.sql already carries these signatures for fresh installs;
-- this script registers them on databases installed as 0.131.0.  Run via
-- `ALTER EXTENSION pg_ripple UPDATE TO '0.140.0'`.

CREATE OR REPLACE FUNCTION pg_ripple."numeric_type_code_spi"(
	"id" bigint
) RETURNS INT
STRICT
LANGUAGE c
AS 'MODULE_PATHNAME', 'numeric_type_code_spi_wrapper';

CREATE OR REPLACE FUNCTION _pg_ripple."sha1_hex"(
	"value" TEXT
) RETURNS TEXT
IMMUTABLE STRICT PARALLEL SAFE
LANGUAGE c
AS 'MODULE_PATHNAME', 'sha1_hex_wrapper';

-- Membership guard: inside `ALTER EXTENSION pg_ripple UPDATE` the statements
-- above are absorbed as members automatically and the explicit ADD would
-- fail with "already a member"; executed manually (e.g. re-running the DDL
-- on an install that predates the version bump), they record membership so
-- pg_dump/restore carries the functions.
DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM pg_depend d
          JOIN pg_proc p ON p.oid = d.objid
         WHERE d.refobjid = (SELECT oid FROM pg_extension WHERE extname = 'pg_ripple')
           AND d.deptype = 'e'
           AND p.proname = 'numeric_type_code_spi'
    ) THEN
        ALTER EXTENSION pg_ripple ADD FUNCTION pg_ripple.numeric_type_code_spi(bigint);
    END IF;

    IF NOT EXISTS (
        SELECT 1
          FROM pg_depend d
          JOIN pg_proc p ON p.oid = d.objid
         WHERE d.refobjid = (SELECT oid FROM pg_extension WHERE extname = 'pg_ripple')
           AND d.deptype = 'e'
           AND p.proname = 'sha1_hex'
           AND p.pronamespace = (SELECT oid FROM pg_namespace WHERE nspname = '_pg_ripple')
    ) THEN
        ALTER EXTENSION pg_ripple ADD FUNCTION _pg_ripple.sha1_hex(TEXT);
    END IF;
END
$$;
