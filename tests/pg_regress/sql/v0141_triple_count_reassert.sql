-- pg_regress test: v0.140.1 — insert paths count only real inserts (VAL-371)
--
-- Regression target: on 0.140.0 every insert path incremented
-- _pg_ripple.predicates.triple_count unconditionally while the SQL absorbed
-- duplicates (src/storage/ops/mod.rs):
--   1. insert_triple single-row upsert fast path       (+1 per re-assert)
--   2. insert_encoded_triple single-row fast path      (+1 per re-assert)
--   3. batch_insert_encoded, all batch sub-paths       (+rows.len() per
--      re-assert: VP-delta VALUES, VP-delta UNNEST via bulk_load_use_copy,
--      and the NOT EXISTS-guarded vp_rare branch)
-- Measured in production (04/10/2026): triple_count() 647.255 vs 624.116
-- physical/visible rows (+23.139), concentrated in predicates the ValorBrain
-- engine re-asserts on every load/backfill/re-ingest pass.
--
-- Fix under test: single-row upserts RETURN i, (xmax = 0) and count only
-- fresh rows; batch paths use WITH ins AS (INSERT ... ON CONFLICT DO NOTHING
-- RETURNING 1) SELECT count(*). Re-asserting a triple N times must leave the
-- per-predicate counter at the number of DISTINCT triples.
--
-- Layout: the vp_promotion_threshold GUC bottoms out at 100, so predicates
-- are seeded with 100 DISTINCT triples (generate_series / string_agg keeps
-- the payload compact) to cross the promotion boundary; re-asserts then
-- exercise the dedicated-VP fast paths. Triples live in named graphs
-- (load_nquads), matching the engine's tenant-graph write shape and keeping
-- the any-graph SPARQL count meaningful.

CREATE EXTENSION IF NOT EXISTS pg_ripple;
SELECT pg_ripple.triple_count() >= 0 AS library_loaded;
SET search_path TO pg_ripple, public;

-- Lowest legal value (GUC range is 100 .. 10000000; default 1000).
SET pg_ripple.vp_promotion_threshold = 100;

-- ─── Path 1: insert_triple single-row upsert fast path ────────────────────
-- 100 DISTINCT single inserts cross the promotion threshold on the last one;
-- the 101st distinct insert then takes the dedicated-VP upsert fast path.
SELECT (SELECT count(*) FROM generate_series(1, 100) i
        WHERE pg_ripple.insert_triple(
            'https://val371.test/e' || i,
            'https://val371.test/p1', '"a"',
            'https://val371.test/g1') IS NOT NULL) = 100 AS seed_p1_100_distinct;
-- Guard: p1 must be promoted (dedicated VP table) so the inserts below
-- exercise the upsert fast path, not vp_rare.
SELECT EXISTS (
    SELECT 1 FROM _pg_ripple.predicates p
    JOIN _pg_ripple.dictionary d ON d.id = p.id
    WHERE d.value = 'https://val371.test/p1' AND p.table_oid IS NOT NULL
) AS p1_promoted;
-- One more DISTINCT triple through the fast path (counter 100 → 101).
SELECT pg_ripple.insert_triple(
    'https://val371.test/e0', 'https://val371.test/p1', '"zz"',
    'https://val371.test/g1'
) IS NOT NULL AS p1_fast_path_distinct;
-- Re-assert that same triple 5 more times (0.140.0 leaked +5 here).
SELECT (SELECT count(*) FROM generate_series(1, 5) n
        WHERE pg_ripple.insert_triple(
            'https://val371.test/e0',
            'https://val371.test/p1', '"zz"',
            'https://val371.test/g1') IS NOT NULL) = 5 AS p1_reassert_5x;
-- Counter must equal the number of DISTINCT triples (101), not 106.
SELECT (SELECT p.triple_count FROM _pg_ripple.predicates p
        JOIN _pg_ripple.dictionary d ON d.id = p.id
        WHERE d.value = 'https://val371.test/p1') = 101
    AS p1_counter_101_after_5_reasserts;

-- ─── Path 3a: batch_insert_encoded VP-delta branch (VALUES sub-path) ──────
-- p2: 100 DISTINCT triples in one load_nquads batch; end-of-load promotion
-- moves them to a dedicated VP table. Re-loads of the SAME payload then take
-- the VP-delta batch fast path.
SELECT pg_ripple.load_nquads(
    (SELECT string_agg(format(
        '<https://val371.test/f%s> <https://val371.test/p2> "x" <https://val371.test/g2> .', i),
        E'\n')
     FROM generate_series(1, 100) i), false) = 100 AS p2_load_100_distinct;
SELECT EXISTS (
    SELECT 1 FROM _pg_ripple.predicates p
    JOIN _pg_ripple.dictionary d ON d.id = p.id
    WHERE d.value = 'https://val371.test/p2' AND p.table_oid IS NOT NULL
) AS p2_promoted;
-- Re-assert the whole payload twice with bulk_load_use_copy = off
-- (multi-row VALUES sub-path; 0.140.0 leaked +100 per pass).
SET pg_ripple.bulk_load_use_copy = off;
SELECT pg_ripple.load_nquads(
    (SELECT string_agg(format(
        '<https://val371.test/f%s> <https://val371.test/p2> "x" <https://val371.test/g2> .', i),
        E'\n')
     FROM generate_series(1, 100) i), false) = 100 AS p2_reassert_values_1;
SELECT pg_ripple.load_nquads(
    (SELECT string_agg(format(
        '<https://val371.test/f%s> <https://val371.test/p2> "x" <https://val371.test/g2> .', i),
        E'\n')
     FROM generate_series(1, 100) i), false) = 100 AS p2_reassert_values_2;
SELECT (SELECT p.triple_count FROM _pg_ripple.predicates p
        JOIN _pg_ripple.dictionary d ON d.id = p.id
        WHERE d.value = 'https://val371.test/p2') = 100
    AS p2_counter_100_after_values_reasserts;

-- ─── G1 capture: global total before the remaining re-assert passes ───────
-- Everything after this point is a pure re-assert (2× COPY-sub-path re-loads
-- of the p2 payload + the p3 loads). The global catalog total must not move
-- by even one row. Captured to a TEMP table because the absolute value is
-- NOT deterministic: cargo pgrx regress runs the whole suite in one shared
-- database, so other tests' triples are already counted in triple_count().
CREATE TEMP TABLE val371_global_before AS
    SELECT pg_ripple.triple_count() AS n;
SELECT pg_ripple.triple_count() >= 0 AS g1_capture_point;

-- ─── Path 3b: batch_insert_encoded VP-delta branch (UNNEST/COPY sub-path) ─
SET pg_ripple.bulk_load_use_copy = on;
SELECT pg_ripple.load_nquads(
    (SELECT string_agg(format(
        '<https://val371.test/f%s> <https://val371.test/p2> "x" <https://val371.test/g2> .', i),
        E'\n')
     FROM generate_series(1, 100) i), false) = 100 AS p2_reassert_copy_1;
SELECT pg_ripple.load_nquads(
    (SELECT string_agg(format(
        '<https://val371.test/f%s> <https://val371.test/p2> "x" <https://val371.test/g2> .', i),
        E'\n')
     FROM generate_series(1, 100) i), false) = 100 AS p2_reassert_copy_2;
SELECT (SELECT p.triple_count FROM _pg_ripple.predicates p
        JOIN _pg_ripple.dictionary d ON d.id = p.id
        WHERE d.value = 'https://val371.test/p2') = 100
    AS p2_counter_100_after_copy_reasserts;
SET pg_ripple.bulk_load_use_copy = off;

-- ─── Path 3c: batch_insert_encoded vp_rare branch ─────────────────────────
-- p3: ONE distinct triple loaded three times stays below the promotion
-- threshold, so every load takes the vp_rare batch branch (NOT EXISTS guard).
-- This is the literal acceptance probe: re-assert N× → counter = 1.
SELECT pg_ripple.load_nquads(
    '<https://val371.test/e9> <https://val371.test/p3> "w" <https://val371.test/g3> .',
    false) = 1 AS p3_load_1_distinct;
SELECT pg_ripple.load_nquads(
    '<https://val371.test/e9> <https://val371.test/p3> "w" <https://val371.test/g3> .',
    false) = 1 AS p3_reassert_rare_1;
SELECT pg_ripple.load_nquads(
    '<https://val371.test/e9> <https://val371.test/p3> "w" <https://val371.test/g3> .',
    false) = 1 AS p3_reassert_rare_2;
-- Guard: p3 really stayed in vp_rare (branch coverage for this assertion).
SELECT EXISTS (
    SELECT 1 FROM _pg_ripple.predicates p
    JOIN _pg_ripple.dictionary d ON d.id = p.id
    WHERE d.value = 'https://val371.test/p3' AND p.table_oid IS NULL
) AS p3_still_vp_rare;
SELECT (SELECT p.triple_count FROM _pg_ripple.predicates p
        JOIN _pg_ripple.dictionary d ON d.id = p.id
        WHERE d.value = 'https://val371.test/p3') = 1
    AS p3_counter_1_after_3_loads;

-- ─── Global invariants (HERMETIC) ─────────────────────────────────────────
-- cargo pgrx regress runs the WHOLE suite in one shared database, so other
-- tests' triples are already present when this test runs: absolute totals
-- like `triple_count() = 202` can never hold in the canonical harness (the
-- first 0.140.1 CI run failed exactly there while every per-predicate
-- assertion passed). The invariants below are relative or namespace-scoped,
-- immune to foreign triples on both sides of the comparison.

-- G1: after the capture point the suite requests 203 more rows (2×100
-- COPY-batch rows re-asserting the p2 payload + 3 vp_rare loads of the one
-- p3 triple), but exactly ONE is a real insert (p3's first load). The global
-- catalog total must move by exactly that 1 — on 0.140.0 logic it moved by
-- the full 203 requested.
SELECT (SELECT pg_ripple.triple_count() - n FROM val371_global_before) = 1
    AS g1_global_total_moves_only_by_real_inserts;

-- G2: scoped catalog == scoped physical. The catalog side sums triple_count
-- over THIS test's predicates only; the physical side counts any-graph
-- SPARQL solutions whose subject belongs to this test's namespace. Both
-- sides ignore every other test's data: 101 + 100 + 1 = 202 expected.
SELECT (SELECT COALESCE(SUM(p.triple_count), 0) FROM _pg_ripple.predicates p
        JOIN _pg_ripple.dictionary d ON d.id = p.id
        WHERE d.value LIKE 'https://val371.test/%')
    = (SELECT count(*) FROM pg_ripple.sparql($$
        SELECT ?s ?p ?o WHERE {
            GRAPH ?g { ?s ?p ?o . FILTER(STRSTARTS(STR(?s), "https://val371.test/")) }
        }
      $$))
    AS g2_scoped_catalog_equals_physical;

-- G3: the scoped physical count itself is exactly the 202 DISTINCT triples
-- this test created (101 p1 + 100 p2 + 1 p3) — a positive control that the
-- G2 filter is not vacuously matching nothing.
SELECT (SELECT count(*) FROM pg_ripple.sparql($$
    SELECT ?s ?p ?o WHERE {
        GRAPH ?g { ?s ?p ?o . FILTER(STRSTARTS(STR(?s), "https://val371.test/")) }
    }
  $$)) = 202
    AS g3_scoped_physical_is_202;
