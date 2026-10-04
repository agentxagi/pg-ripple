-- pg_regress test: v0.140.4 — re-assert keeps the existing SID and does not
-- rewrite the delta row (VAL-377)
--
-- Regression target: on 0.140.3 the single-row upserts of
-- src/storage/ops/mod.rs (insert_triple, insert_encoded_triple) ran
--     INSERT INTO vp_{id}_delta (s, o, g) VALUES (...)
--     ON CONFLICT (s, o, g) DO UPDATE SET i = EXCLUDED.i
-- over an `i` column that defaults to nextval('_pg_ripple.statement_id_seq').
-- The conflict path therefore OVERWROTE the existing row's statement ID with
-- a fresh sequence value and physically rewrote the row on every pass —
-- SID 641→642→643 observed over three re-asserts of one triple in production
-- (found during the VAL-371 delivery review, PR #18). Cost: WAL/bloat churn
-- on the engine's hot re-assert paths (load-ripple.sh,
-- backfill-ripple-sync.ts, entity re-ingest) and an unstable identity for a
-- return value pg_ripple.insert_triples() already hands to SQL callers.
--
-- Fix under test: both paths upsert with ON CONFLICT (s, o, g) DO NOTHING
-- inside a CTE whose second arm returns the EXISTING row's SID. Re-asserts
-- must (a) return the same SID every time, (b) leave the physical delta
-- row's `i` AND `xmin` untouched (xmin equality proves no tuple rewrite),
-- and (c) keep the VAL-371 distinct-triple counting invariant.
--
-- Layout: predicates are seeded past the promotion threshold
-- (vp_promotion_threshold bottoms out at 100) so every probed insert takes
-- the dedicated-VP delta fast path. Path 1 exercises insert_triple through
-- the dictionary API; path 2 exercises insert_encoded_triple through
-- SPARQL UPDATE INSERT DATA (mutation journal → insert_triple_by_ids).
-- Everything lives in the https://val377.test/ namespace so the scoped
-- invariants are immune to foreign triples in the shared regress database.

CREATE EXTENSION IF NOT EXISTS pg_ripple;
SELECT pg_ripple.triple_count() >= 0 AS library_loaded;
SET search_path TO pg_ripple, public;

-- Lowest legal value (GUC range is 100 .. 10000000; default 1000).
SET pg_ripple.vp_promotion_threshold = 100;

-- ─── Path 1: insert_triple single-row upsert (dictionary API) ────────────
-- 100 DISTINCT single inserts cross the promotion threshold on the last one.
SELECT (SELECT count(*) FROM generate_series(1, 100) i
        WHERE pg_ripple.insert_triple(
            'https://val377.test/e' || i,
            'https://val377.test/p1', 'https://val377.test/o' || i,
            'https://val377.test/g1') IS NOT NULL) = 100 AS seed_p1_100_distinct;
-- Guard: p1 must be promoted (dedicated VP table) so the probed inserts
-- below exercise the delta upsert fast path, not vp_rare.
SELECT EXISTS (
    SELECT 1 FROM _pg_ripple.predicates p
    JOIN _pg_ripple.dictionary d ON d.id = p.id
    WHERE d.value = 'https://val377.test/p1' AND p.table_oid IS NOT NULL
) AS p1_promoted;
-- One more DISTINCT triple through the fast path; capture its SID.
SELECT pg_ripple.insert_triple(
    'https://val377.test/e0', 'https://val377.test/p1',
    'https://val377.test/o0', 'https://val377.test/g1') AS p1_sid \gset
SELECT 'vp_' || p.id || '_delta' AS p1_delta
FROM _pg_ripple.predicates p
JOIN _pg_ripple.dictionary d ON d.id = p.id
WHERE d.value = 'https://val377.test/p1' \gset
CREATE TEMP TABLE val377_p1_row_before AS
    SELECT i AS sid, xmin::text AS row_xmin FROM _pg_ripple.:p1_delta WHERE i = :p1_sid;
SELECT (SELECT count(*) FROM val377_p1_row_before) = 1 AS p1_row_present;
-- Re-assert the same triple 5 more times (0.140.3 minted a fresh SID on
-- every one of these calls).
SELECT (SELECT count(*) FROM generate_series(1, 5) n
        WHERE pg_ripple.insert_triple(
            'https://val377.test/e0', 'https://val377.test/p1',
            'https://val377.test/o0', 'https://val377.test/g1') = :p1_sid) = 5
    AS p1_reassert_returns_same_sid_5x;
-- Physical proof: same SID still present AND xmin unchanged ⇒ the row was
-- never rewritten (no new tuple version, no WAL churn).
SELECT (SELECT count(*) FROM _pg_ripple.:p1_delta WHERE i = :p1_sid) = 1
   AND (SELECT xmin::text FROM _pg_ripple.:p1_delta WHERE i = :p1_sid)
        = (SELECT row_xmin FROM val377_p1_row_before)
    AS p1_row_not_rewritten;
-- Counter still counts DISTINCT triples only (VAL-371 invariant kept).
SELECT (SELECT p.triple_count FROM _pg_ripple.predicates p
        JOIN _pg_ripple.dictionary d ON d.id = p.id
        WHERE d.value = 'https://val377.test/p1') = 101
    AS p1_counter_101_after_reasserts;

-- ─── Path 2: insert_encoded_triple (SPARQL UPDATE INSERT DATA) ───────────
-- p2: 100 DISTINCT triples in one load_nquads batch; end-of-load promotion
-- moves them to a dedicated VP table.
SELECT pg_ripple.load_nquads(
    (SELECT string_agg(format(
        '<https://val377.test/f%s> <https://val377.test/p2> "x" <https://val377.test/g2> .', i),
        E'\n')
     FROM generate_series(1, 100) i), false) = 100 AS p2_load_100_distinct;
SELECT EXISTS (
    SELECT 1 FROM _pg_ripple.predicates p
    JOIN _pg_ripple.dictionary d ON d.id = p.id
    WHERE d.value = 'https://val377.test/p2' AND p.table_oid IS NOT NULL
) AS p2_promoted;
-- One DISTINCT triple through the SPARQL UPDATE path (no pre-existence
-- check: every quad reaches insert_triple_by_ids → insert_encoded_triple).
SELECT pg_ripple.sparql_update(
    'INSERT DATA { GRAPH <https://val377.test/g2> {
        <https://val377.test/s2> <https://val377.test/p2> "zz" } }'
) = 1 AS p2_insert_data_distinct;
SELECT 'vp_' || p.id || '_delta' AS p2_delta
FROM _pg_ripple.predicates p
JOIN _pg_ripple.dictionary d ON d.id = p.id
WHERE d.value = 'https://val377.test/p2' \gset
-- The SPARQL path does not return SIDs; this insert minted the newest
-- sequence value in p2's delta table, which identifies the row.
SELECT max(i) AS p2_sid FROM _pg_ripple.:p2_delta \gset
CREATE TEMP TABLE val377_p2_row_before AS
    SELECT i AS sid, xmin::text AS row_xmin FROM _pg_ripple.:p2_delta WHERE i = :p2_sid;
SELECT (SELECT count(*) FROM val377_p2_row_before) = 1 AS p2_row_present;
-- Re-execute the SAME INSERT DATA three times (0.140.3 rewrote the row —
-- and its `i` — on every execution).
SELECT pg_ripple.sparql_update(
    'INSERT DATA { GRAPH <https://val377.test/g2> {
        <https://val377.test/s2> <https://val377.test/p2> "zz" } }'
) = 1 AS p2_reassert_1;
SELECT pg_ripple.sparql_update(
    'INSERT DATA { GRAPH <https://val377.test/g2> {
        <https://val377.test/s2> <https://val377.test/p2> "zz" } }'
) = 1 AS p2_reassert_2;
SELECT pg_ripple.sparql_update(
    'INSERT DATA { GRAPH <https://val377.test/g2> {
        <https://val377.test/s2> <https://val377.test/p2> "zz" } }'
) = 1 AS p2_reassert_3;
-- Physical proof: row still carries the captured SID with unchanged xmin,
-- and the table still holds exactly the 101 DISTINCT triples (no duplicate
-- row with a re-minted SID was left behind).
SELECT (SELECT count(*) FROM _pg_ripple.:p2_delta WHERE i = :p2_sid) = 1
   AND (SELECT xmin::text FROM _pg_ripple.:p2_delta WHERE i = :p2_sid)
        = (SELECT row_xmin FROM val377_p2_row_before)
   AND (SELECT count(*) FROM _pg_ripple.:p2_delta) = 101
    AS p2_row_not_rewritten_and_table_still_101;
SELECT (SELECT p.triple_count FROM _pg_ripple.predicates p
        JOIN _pg_ripple.dictionary d ON d.id = p.id
        WHERE d.value = 'https://val377.test/p2') = 101
    AS p2_counter_101_after_reasserts;

-- ─── Global invariants (HERMETIC) ─────────────────────────────────────────
-- cargo pgrx regress runs the WHOLE suite in one shared database, so other
-- tests' triples may already be present: invariants are namespace-scoped on
-- both sides. Scoped catalog == scoped physical, 101 + 101 expected.
SELECT (SELECT COALESCE(SUM(p.triple_count), 0) FROM _pg_ripple.predicates p
        JOIN _pg_ripple.dictionary d ON d.id = p.id
        WHERE d.value LIKE 'https://val377.test/%')
    = (SELECT count(*) FROM pg_ripple.sparql($$
        SELECT ?s ?p ?o WHERE {
            GRAPH ?g { ?s ?p ?o . FILTER(STRSTARTS(STR(?s), "https://val377.test/")) }
        }
      $$))
    AS g1_scoped_catalog_equals_physical;
-- Positive control: the scoped physical count is exactly the 202 DISTINCT
-- triples this test created (101 p1 + 101 p2).
SELECT (SELECT count(*) FROM pg_ripple.sparql($$
    SELECT ?s ?p ?o WHERE {
        GRAPH ?g { ?s ?p ?o . FILTER(STRSTARTS(STR(?s), "https://val377.test/")) }
    }
  $$)) = 202
    AS g2_scoped_physical_is_202;
