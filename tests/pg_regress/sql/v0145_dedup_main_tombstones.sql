-- pg_regress test: v0.140.5 — dedup (main branch) keeps tombstone_count and
-- rebuilds the HTAP view (VAL-380)
--
-- Regression target: on 0.140.3 (and the #20 base), the dedicated-tables
-- (main) branch of storage::ops::dedup::deduplicate_predicate
-- (src/storage/ops/dedup.rs) inserted tombstones for duplicate (s,o,g)
-- groups but:
--   1. never updated _pg_ripple.predicates.tombstone_count;
--   2. never rebuilt the HTAP view when it was in the tombstone-skip form
--      (no LEFT JOIN), so the new tombstones stayed invisible until some
--      other path rebuilt the view;
--   3. re-runs re-inserted one tombstone per duplicate group (the tombstones
--      table has no unique constraint — the old ON CONFLICT DO NOTHING was
--      inert) and re-counted the same duplicates on every call;
--   4. masked the WHOLE group once the view did honour tombstones: the
--      tombstone join is on (s,o,g), so a group with no delta row vanished
--      from reads and the next merge dropped it permanently. The branch now
--      re-asserts the minimum-SID row into delta in the same statement — the
--      view keeps returning the triple and main collapses to one physical
--      row on the next merge.
--
-- Mechanism: a promoted predicate gets a duplicate (s,o,g) row in main via
-- re-assert + merge (the delta copy folds on top of the existing main row).
-- deduplicate_predicate must then: mask the duplicate immediately (view in
-- tombstone-aware form), keep the triple visible via the delta survivor,
-- make tombstone_count match the tombstones table, decrement triple_count
-- by the removed duplicate, stay idempotent, and collapse main on the next
-- merge with no data loss.
--
-- Hermetic for the shared regress database (v0143 pattern): all IRIs under
-- https://val380.test/, client_min_messages pinned to error, own helper
-- functions dropped at the end, scoped global invariant, no default-graph
-- writes. compact() is the same suite-wide merge entry point
-- deduplication.sql already uses.

SET client_min_messages TO error;

CREATE EXTENSION IF NOT EXISTS pg_ripple;
SELECT pg_ripple.triple_count() >= 0 AS library_loaded;
SET search_path TO pg_ripple, public;

SET pg_ripple.vp_promotion_threshold = 100;

CREATE OR REPLACE FUNCTION val380_id(p_iri text) RETURNS bigint
LANGUAGE sql STABLE AS $$
    SELECT p.id FROM _pg_ripple.predicates p
    JOIN _pg_ripple.dictionary d ON d.id = p.id
    WHERE d.value = p_iri
$$;

CREATE OR REPLACE FUNCTION val380_cnt(p_iri text) RETURNS bigint
LANGUAGE sql STABLE AS $$
    SELECT COALESCE((SELECT p.triple_count FROM _pg_ripple.predicates p
        JOIN _pg_ripple.dictionary d ON d.id = p.id
        WHERE d.value = p_iri), 0)
$$;

CREATE OR REPLACE FUNCTION val380_tombs_cnt(p_iri text) RETURNS bigint
LANGUAGE sql STABLE AS $$
    SELECT COALESCE((SELECT p.tombstone_count FROM _pg_ripple.predicates p
        JOIN _pg_ripple.dictionary d ON d.id = p.id
        WHERE d.value = p_iri), 0)
$$;

-- Physical row count of a predicate's VP relation.
-- kind: 'view' = vp_{id}, 'main' | 'delta' | 'tombstones' = vp_{id}_{kind}.
CREATE OR REPLACE FUNCTION val380_rows(p_iri text, kind text) RETURNS bigint
LANGUAGE plpgsql STABLE AS $$
DECLARE n bigint;
BEGIN
    EXECUTE format('SELECT count(*)::bigint FROM _pg_ripple.%I',
                   'vp_' || val380_id(p_iri)
                   || CASE WHEN kind = 'view' THEN '' ELSE '_' || kind END)
    INTO n;
    RETURN COALESCE(n, 0);
END $$;

-- Number of (s,o,g) groups with more than one physical row in main.
CREATE OR REPLACE FUNCTION val380_dup_groups(p_iri text) RETURNS bigint
LANGUAGE plpgsql STABLE AS $$
DECLARE n bigint;
BEGIN
    EXECUTE format('SELECT count(*)::bigint FROM ( \
         SELECT s, o, g FROM _pg_ripple.%I \
         GROUP BY s, o, g HAVING count(*) > 1) d',
                   'vp_' || val380_id(p_iri) || '_main') INTO n;
    RETURN COALESCE(n, 0);
END $$;

-- True when the HTAP view is in the tombstone-aware (LEFT JOIN) form —
-- only that form references the tombstones table.
CREATE OR REPLACE FUNCTION val380_view_form(p_iri text) RETURNS boolean
LANGUAGE plpgsql STABLE AS $$
DECLARE def text;
BEGIN
    EXECUTE format('SELECT pg_get_viewdef(''_pg_ripple.%I''::regclass, true)',
                   'vp_' || val380_id(p_iri)) INTO def;
    RETURN position('vp_' || val380_id(p_iri) || '_tombstones' IN def) > 0;
END $$;

-- ─── Setup: promote p1 and fold its rows into main ─────────────────────────
SELECT (SELECT count(*) FROM generate_series(1, 100) i
        WHERE pg_ripple.insert_triple(
            'https://val380.test/s' || i,
            'https://val380.test/p1', '"v"',
            'https://val380.test/g1') IS NOT NULL) = 100 AS p1_seed_100_distinct;
SELECT EXISTS (
    SELECT 1 FROM _pg_ripple.predicates p
    JOIN _pg_ripple.dictionary d ON d.id = p.id
    WHERE d.value = 'https://val380.test/p1' AND p.table_oid IS NOT NULL
) AS p1_promoted;
SELECT pg_ripple.compact() >= 0 AS p1_compact_1;
SELECT val380_rows('https://val380.test/p1', 'main') = 100 AS p1_main_100;
SELECT val380_cnt('https://val380.test/p1') = 100 AS p1_counter_100;
-- Bug precondition: after a clean merge the view is in tombstone-skip form.
SELECT val380_view_form('https://val380.test/p1') = false AS p1_view_skip_form;

-- ─── Duplicate in main: re-assert an existing triple, merge again ───────────
-- delta is empty after the merge, so the re-assert is a real insert (counter
-- 101) and the next merge folds it ON TOP of the main row — a physical
-- duplicate (s,o,g) pair, the exact input dedup's main branch targets.
SELECT pg_ripple.insert_triple(
    'https://val380.test/s50',
    'https://val380.test/p1', '"v"',
    'https://val380.test/g1') IS NOT NULL AS p1_reassert_s50;
SELECT val380_cnt('https://val380.test/p1') = 101 AS p1_counter_101_after_reassert;
SELECT pg_ripple.compact() >= 0 AS p1_compact_2;
SELECT val380_rows('https://val380.test/p1', 'main') = 101 AS p1_main_101_with_duplicate;
SELECT val380_cnt('https://val380.test/p1') = 101 AS p1_counter_101_with_duplicate;
SELECT val380_dup_groups('https://val380.test/p1') = 1 AS p1_one_duplicate_group;
-- The view still returns one logical row per group (DISTINCT ON safety net).
SELECT val380_rows('https://val380.test/p1', 'view') = 100 AS p1_view_100_before_dedup;

-- ─── VAL-380: dedup honours tombstones in catalog AND view, immediately ─────
SELECT pg_ripple.deduplicate_predicate('https://val380.test/p1') = 1
    AS p1_dedup_removed_1;
-- 1. tombstone_count matches the tombstones table (0.140.3 kept it at 0).
SELECT val380_rows('https://val380.test/p1', 'tombstones') = 1 AS p1_tombs_table_1;
SELECT val380_tombs_cnt('https://val380.test/p1') = 1 AS p1_tombs_counter_1;
-- 2. The view is tombstone-aware NOW, not after some external rebuild.
SELECT val380_view_form('https://val380.test/p1') = true AS p1_view_tombstone_form;
-- 3. No read loss: the survivor rides delta, the triple stays queryable.
SELECT val380_rows('https://val380.test/p1', 'delta') = 1 AS p1_delta_survivor_1;
SELECT val380_rows('https://val380.test/p1', 'view') = 100 AS p1_view_100_after_dedup;
SELECT (SELECT count(*) FROM pg_ripple.find_triples_in_graph(
            'https://val380.test/s50',
            'https://val380.test/p1',
            '"v"',
            'https://val380.test/g1')) = 1 AS p1_survivor_findable;
-- 4. Counter decremented by the removed duplicate (0.140.3 kept 101).
SELECT val380_cnt('https://val380.test/p1') = 100 AS p1_counter_100_after_dedup;
-- 5. Idempotent: second run removes nothing and does not duplicate tombstones.
SELECT pg_ripple.deduplicate_predicate('https://val380.test/p1') = 0
    AS p1_dedup_idempotent_0;
SELECT val380_rows('https://val380.test/p1', 'tombstones') = 1
    AS p1_tombs_still_1_after_rerun;
SELECT val380_tombs_cnt('https://val380.test/p1') = 1 AS p1_tombs_counter_still_1;

-- ─── Next merge collapses main to one physical row, no data loss ────────────
SELECT pg_ripple.compact() >= 0 AS p1_compact_3;
SELECT val380_rows('https://val380.test/p1', 'main') = 100 AS p1_main_100_after_merge;
SELECT val380_rows('https://val380.test/p1', 'view') = 100 AS p1_view_100_after_merge;
SELECT (SELECT count(*) FROM pg_ripple.find_triples_in_graph(
            'https://val380.test/s50',
            'https://val380.test/p1',
            '"v"',
            'https://val380.test/g1')) = 1 AS p1_survivor_findable_after_merge;
-- Tombstones absorbed: counter reset and view back to the skip form by the
-- merge itself (M15-05) — the dedup-side rebuild is not a one-way street.
SELECT val380_rows('https://val380.test/p1', 'tombstones') = 0
    AS p1_tombs_table_0_after_merge;
SELECT val380_tombs_cnt('https://val380.test/p1') = 0
    AS p1_tombs_counter_0_after_merge;
SELECT val380_cnt('https://val380.test/p1') = 100 AS p1_counter_100_after_merge;

-- ─── Cleanup: DROP GRAPH honours tombstones immediately (same class) ────────
-- The dedicated branch of drop_graph had the same gap as dedup's main
-- branch (tombstones without tombstone_count / view maintenance — its
-- sibling clear_graph_by_id already had it). With the fix, the dropped
-- main rows are masked the moment DROP returns; without it, the
-- tombstone-skip view keeps serving the dropped graph (red on base).
SELECT pg_ripple.drop_graph('https://val380.test/g1') = 100 AS cleanup_g1_dropped_100;
SELECT val380_tombs_cnt('https://val380.test/p1') = 100 AS cleanup_tombs_counter_100;
SELECT val380_view_form('https://val380.test/p1') = true AS cleanup_view_tombstone_form;
SELECT val380_rows('https://val380.test/p1', 'view') = 0 AS cleanup_p1_view_0;
SELECT (SELECT count(*) FROM pg_ripple.sparql($$
        SELECT ?s ?p ?o WHERE {
            GRAPH ?g { ?s ?p ?o . FILTER(STRSTARTS(STR(?s), "https://val380.test/")) }
        }
      $$)) = 0 AS cleanup_sparql_sees_0;
-- Tombstones stay physically pending until a future merge of this predicate
-- (merge_all only folds predicates with delta rows) — the platform's
-- pending-delete design; the invariant is that they are no longer visible.
SELECT val380_rows('https://val380.test/p1', 'tombstones') = 100
    AS cleanup_tombs_pending_100;
SELECT pg_ripple.compact() >= 0 AS cleanup_compact;
SELECT val380_rows('https://val380.test/p1', 'view') = 0 AS cleanup_p1_view_0_after_compact;
SELECT val380_tombs_cnt('https://val380.test/p1') = 100
    AS cleanup_tombs_counter_still_100;
SELECT val380_cnt('https://val380.test/p1') = 0 AS cleanup_p1_counter_0;

-- Scoped global invariant (HERMETIC, v0141/v0143 shape): catalog and any-
-- graph SPARQL physical must agree over this test's namespace only.
SELECT (SELECT COALESCE(SUM(p.triple_count), 0) FROM _pg_ripple.predicates p
        JOIN _pg_ripple.dictionary d ON d.id = p.id
        WHERE d.value LIKE 'https://val380.test/%')
    = (SELECT count(*) FROM pg_ripple.sparql($$
        SELECT ?s ?p ?o WHERE {
            GRAPH ?g { ?s ?p ?o . FILTER(STRSTARTS(STR(?s), "https://val380.test/")) }
        }
      $$)) AS g_scoped_catalog_equals_physical;

DROP FUNCTION IF EXISTS val380_id(text);
DROP FUNCTION IF EXISTS val380_cnt(text);
DROP FUNCTION IF EXISTS val380_tombs_cnt(text);
DROP FUNCTION IF EXISTS val380_rows(text, text);
DROP FUNCTION IF EXISTS val380_dup_groups(text);
DROP FUNCTION IF EXISTS val380_view_form(text);
