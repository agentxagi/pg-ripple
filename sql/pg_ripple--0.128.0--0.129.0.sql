-- Migration 0.128.0 → 0.129.0: graph-scoped justify(s, p, o, graph) overload
--
-- New features:
--   JUSTIFY-GRAPH: pg_ripple.justify(subject, predicate, object, graph) —
--     backward-chaining proof tree (root fact and every antecedent, at every
--     depth) built strictly from one named graph, reading the same storages
--     the SPARQL engine's GRAPH evaluation reads (vp_rare, promoted vp_{id}
--     views = main − tombstones UNION ALL delta).  A derivation whose proof
--     reaches outside the graph is dropped; if none survives, justify returns
--     NULL — never a partial tree.  The 3-arg justify keeps its legacy global
--     semantics.  Backed by tests/pg_regress/sql/justify_graph.sql.
--
-- The function body lives in the shared library (justify_in_graph_wrapper).
-- The generated pg_ripple--0.129.0.sql already carries this signature for
-- fresh installs; this script registers it on databases installed as
-- 0.128.0.  Run via `ALTER EXTENSION pg_ripple UPDATE TO '0.129.0'`.

CREATE OR REPLACE FUNCTION pg_ripple."justify"(
	"subject" TEXT,
	"predicate" TEXT,
	"object" TEXT,
	"graph" TEXT
) RETURNS jsonb
STRICT
LANGUAGE c
AS 'MODULE_PATHNAME', 'justify_in_graph_wrapper';

-- Membership guard: inside `ALTER EXTENSION pg_ripple UPDATE` the statement
-- above is absorbed as a member automatically and the explicit ADD would
-- fail with "already a member"; executed manually (e.g. re-running the DDL
-- on an install that predates the version bump), it records membership so
-- pg_dump/restore carries the overload.
DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM pg_depend d
          JOIN pg_proc p ON p.oid = d.objid
         WHERE d.refobjid = (SELECT oid FROM pg_extension WHERE extname = 'pg_ripple')
           AND d.deptype = 'e'
           AND p.proname = 'justify'
           AND pg_get_function_identity_arguments(p.oid) =
               'subject text, predicate text, object text, graph text'
    ) THEN
        ALTER EXTENSION pg_ripple ADD FUNCTION pg_ripple.justify(TEXT, TEXT, TEXT, TEXT);
    END IF;
END
$$;
