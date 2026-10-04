-- pg_regress test: v0.140.3 — delete paths decrement triple_count (VAL-376)
--
-- Regression target: on 0.140.2 every mass-cleanup path that removed vp_rare
-- rows did so WITHOUT decrementing _pg_ripple.predicates.triple_count:
--   1. storage::clear_graph_by_id (src/storage/ops/scan.rs) — backs SPARQL
--      CLEAR GRAPH / CLEAR DEFAULT / CLEAR NAMED / CLEAR ALL and the
--      pg_ripple.clear_graph API
--   2. storage::drop_graph — backs SPARQL DROP GRAPH and pg_ripple.drop_graph
--   3. security_api::erase_subject — the GDPR erase deleted vp_rare, delta
--      and main rows with no counter maintenance at all
--   4. storage::ops::dedup::deduplicate_predicate — physical duplicate
--      removal left the catalog counting rows that no longer exist
-- The dedicated-VP half of (1)/(2) always decremented; only the vp_rare half
-- leaked.
--
-- Mechanism (bisected on the shared regress database): w3c_sparql_update_
-- conformance's wholesale CLEAR wipes vp_rare; rare predicates keep their
-- inflated counters with zero physical rows, so the next re-assert of the
-- same triple counts as a REAL insert (the NOT EXISTS guard finds nothing)
-- and re-drifts catalog above physical — re-running v0141 moved its p3
-- counter 1 -> 2 while the physical count stayed at 1 (VAL-371 validation
-- finding, 04/10/2026).
--
-- Fix under test: each path now decrements the per-predicate counter by the
-- rows it actually removes (delete+aggregate UPDATE in one statement for the
-- vp_rare branches; per-table GREATEST(0, triple_count - n) elsewhere).
--
-- Layout mirrors v0141: rare predicates stay below the promotion threshold
-- (vp_promotion_threshold bottoms out at 100); one PROMOTED predicate is
-- Deterministic output in the shared regress database: the lazy-load
-- WARNING and the cdc trigger NOTICEs would otherwise interleave
-- differently depending on whether the extension already exists.
SET client_min_messages TO error;
-- cleared as a control that its pre-existing delta/tombstone decrement still
-- holds and is not double-applied. All invariants are namespace-scoped: the
-- suite runs in one shared database, so absolute global totals never hold.
-- No default-graph writes: CLEAR DEFAULT would erase other tests' data.

CREATE EXTENSION IF NOT EXISTS pg_ripple;
SELECT pg_ripple.triple_count() >= 0 AS library_loaded;
SET search_path TO pg_ripple, public;

SET pg_ripple.vp_promotion_threshold = 100;

CREATE OR REPLACE FUNCTION val376_cnt(p_iri text) RETURNS bigint
LANGUAGE sql STABLE AS $$
    SELECT COALESCE((SELECT p.triple_count FROM _pg_ripple.predicates p
        JOIN _pg_ripple.dictionary d ON d.id = p.id
        WHERE d.value = p_iri), 0)
$$;

-- ─── Path 1: SPARQL CLEAR GRAPH on a rare predicate ────────────────────────
-- pr1: 3 DISTINCT triples stay below the threshold, so every row lives in
-- vp_rare — exactly the half of clear_graph_by_id that used to leak.
SELECT pg_ripple.load_nquads(
    (SELECT string_agg(format(
        '<https://val376.test/e%s> <https://val376.test/pr1> "c" <https://val376.test/gc1> .', i),
        E'\n')
     FROM generate_series(1, 3) i), false) = 3 AS pr1_load_3_distinct;
SELECT val376_cnt('https://val376.test/pr1') = 3 AS pr1_counter_3_before_clear;
SELECT pg_ripple.sparql_update($$CLEAR GRAPH <https://val376.test/gc1>$$) = 3
    AS clear_graph_removed_3;
SELECT val376_cnt('https://val376.test/pr1') = 0
    AS pr1_counter_0_after_clear;
SELECT (SELECT count(*) FROM _pg_ripple.vp_rare r
        JOIN _pg_ripple.dictionary d ON d.id = r.p
        WHERE d.value = 'https://val376.test/pr1') = 0
    AS pr1_physical_0_after_clear;
-- The v0141 re-execution scenario, end to end: after the leaked clear the
-- re-load was a REAL insert on top of a counter that never went down —
-- catalog drifted above physical. With the fix the counter returns to the
-- exact number of DISTINCT triples.
SELECT pg_ripple.load_nquads(
    (SELECT string_agg(format(
        '<https://val376.test/e%s> <https://val376.test/pr1> "c" <https://val376.test/gc1> .', i),
        E'\n')
     FROM generate_series(1, 3) i), false) = 3 AS pr1_reload_after_clear;
SELECT val376_cnt('https://val376.test/pr1') = 3
    AS pr1_counter_exactly_3_after_reload;

-- ─── Path 2: pg_ripple.drop_graph API on a rare predicate ──────────────────
SELECT pg_ripple.load_nquads(
    (SELECT string_agg(format(
        '<https://val376.test/f%s> <https://val376.test/pr2> "d" <https://val376.test/gd1> .', i),
        E'\n')
     FROM generate_series(1, 2) i), false) = 2 AS pr2_load_2_distinct;
SELECT val376_cnt('https://val376.test/pr2') = 2 AS pr2_counter_2_before_drop;
SELECT pg_ripple.drop_graph('https://val376.test/gd1') = 2
    AS drop_graph_removed_2;
SELECT val376_cnt('https://val376.test/pr2') = 0
    AS pr2_counter_0_after_drop;

-- ─── Control: PROMOTED predicate clear keeps its correct decrement ──────────
-- pp1 crosses the promotion threshold (100), then one more distinct triple
-- rides the dedicated-VP upsert fast path. CLEAR must remove all 101 and
-- zero the counter exactly once — proving the fix neither skips nor doubles
-- the dedicated-VP half that already decremented before VAL-376.
SELECT (SELECT count(*) FROM generate_series(1, 100) i
        WHERE pg_ripple.insert_triple(
            'https://val376.test/g' || i,
            'https://val376.test/pp1', '"p"',
            'https://val376.test/gp1') IS NOT NULL) = 100 AS pp1_seed_100_distinct;
SELECT EXISTS (
    SELECT 1 FROM _pg_ripple.predicates p
    JOIN _pg_ripple.dictionary d ON d.id = p.id
    WHERE d.value = 'https://val376.test/pp1' AND p.table_oid IS NOT NULL
) AS pp1_promoted;
SELECT pg_ripple.insert_triple(
    'https://val376.test/g0', 'https://val376.test/pp1', '"pz"',
    'https://val376.test/gp1') IS NOT NULL AS pp1_fast_path_distinct;
SELECT val376_cnt('https://val376.test/pp1') = 101 AS pp1_counter_101_before_clear;
SELECT pg_ripple.sparql_update($$CLEAR GRAPH <https://val376.test/gp1>$$) = 101
    AS clear_promoted_removed_101;
SELECT val376_cnt('https://val376.test/pp1') = 0
    AS pp1_counter_0_after_clear;

-- ─── Path 3: erase_subject on rare predicates ──────────────────────────────
-- Subject es1 has rows under two different rare predicates; the GDPR erase
-- removed them physically while the counters kept counting ghosts.
SELECT pg_ripple.load_nquads(
    '<https://val376.test/es1> <https://val376.test/pe1> "x" <https://val376.test/ge1> .
     <https://val376.test/es2> <https://val376.test/pe1> "y" <https://val376.test/ge1> .
     <https://val376.test/es1> <https://val376.test/pe2> "z" <https://val376.test/ge1> .',
    false) = 3 AS pe_load_3_distinct;
SELECT val376_cnt('https://val376.test/pe1') = 2 AS pe1_counter_2_before_erase;
SELECT val376_cnt('https://val376.test/pe2') = 1 AS pe2_counter_1_before_erase;
SELECT (SELECT rows_deleted FROM pg_ripple.erase_subject('https://val376.test/es1'::text)
        WHERE relation = '_pg_ripple.vp_rare') = 2
    AS erase_subject_removed_2;
SELECT val376_cnt('https://val376.test/pe1') = 1
    AS pe1_counter_1_after_erase;
SELECT val376_cnt('https://val376.test/pe2') = 0
    AS pe2_counter_0_after_erase;

-- ─── Path 4: SPARQL ADD into a rare predicate increments the counter ──────
-- ADD's vp_rare branch used to insert rows silently (no counter bump) — a
-- fourth unmaintained insert path, exposed by the delete-side fix: MOVE now
-- decrements a source graph that the ADD phase had never incremented.
SELECT pg_ripple.load_nquads(
    '<https://val376.test/ea1> <https://val376.test/pa1> "q" <https://val376.test/ga1> .',
    false) = 1 AS pa1_load_1_distinct;
SELECT val376_cnt('https://val376.test/pa1') = 1 AS pa1_counter_1_before_add;
SELECT pg_ripple.sparql_update($$ADD <https://val376.test/ga1> TO <https://val376.test/ga2>$$) = 1
    AS add_copied_1;
SELECT val376_cnt('https://val376.test/pa1') = 2
    AS pa1_counter_2_after_add;
SELECT pg_ripple.drop_graph('https://val376.test/ga1') = 1 AS cleanup_ga1;
SELECT val376_cnt('https://val376.test/pa1') = 1
    AS pa1_counter_1_after_drop;
SELECT pg_ripple.drop_graph('https://val376.test/ga2') = 1 AS cleanup_ga2;

-- ─── Scoped global invariant (HERMETIC) ────────────────────────────────────
-- Everything this test left behind lives under its own namespace: catalog
-- and any-graph SPARQL physical must agree, ignoring other tests' triples
-- on both sides (same shape as v0141 G2/G3).
SELECT (SELECT COALESCE(SUM(p.triple_count), 0) FROM _pg_ripple.predicates p
        JOIN _pg_ripple.dictionary d ON d.id = p.id
        WHERE d.value LIKE 'https://val376.test/%')
    = (SELECT count(*) FROM pg_ripple.sparql($$
        SELECT ?s ?p ?o WHERE {
            GRAPH ?g { ?s ?p ?o . FILTER(STRSTARTS(STR(?s), "https://val376.test/")) }
        }
      $$))
    AS g_scoped_catalog_equals_physical;

-- Cleanup: remove the re-loaded pr1 graph (exercises drop_graph once more)
-- and the helper function; leave nothing behind for later tests.
SELECT pg_ripple.drop_graph('https://val376.test/gc1') = 3 AS cleanup_gc1;
DROP FUNCTION IF EXISTS val376_cnt(text);
