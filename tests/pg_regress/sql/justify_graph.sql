-- Graph-scoped justify() regression tests
-- Covers:
--   JUSTIFY-GRAPH-01: 4-arg justify(s, p, o, graph) finds facts and builds the
--                     proof tree strictly inside the caller's graph
--   JUSTIFY-GRAPH-02: cross-tenant isolation — fact only in graph B is NULL
--                     when asked with graph A (no proof, no existence leak)
--   JUSTIFY-GRAPH-03: proof tree never contains triples from another graph
--   JUSTIFY-GRAPH-04: derivation referencing an out-of-graph antecedent yields
--                     NULL (incomplete proof), never a partial tree
--   JUSTIFY-GRAPH-05: unknown graph / unknown triple → NULL
--   JUSTIFY-GRAPH-06: base (asserted) facts keep type=base inside the graph
--   JUSTIFY-GRAPH-07: 3-arg justify() keeps its legacy global signature

SET client_min_messages = warning;
CREATE EXTENSION IF NOT EXISTS pg_ripple;
SET client_min_messages = DEFAULT;
SET search_path TO pg_ripple, public;

-- Load library so _PG_init registers GUs (required when shared_preload_libraries is not set).
LOAD '$libdir/pg_ripple';

-- ─── Fixture: two tenant graphs, transitive rule scoped by GRAPH ?g ──────────
--
-- On 0.128 the derivation recorder writes STUB rows (empty antecedent_sids)
-- for every rule-head match during an infer pass — asserted facts on the rule
-- predicate carry derivation rows too (production's 564 rows are all stubs).
-- The out-of-graph-antecedent scenario (JUSTIFY-GRAPH-04) and the base-fact
-- assertion (JUSTIFY-GRAPH-06) therefore use `meta_attached`, a predicate no
-- rule touches, so derivation rows exist only where the test fabricates them.

SELECT pg_ripple.drop_rules('test_justify_graph') IS NOT DISTINCT FROM NULL AS rules_dropped;

SELECT pg_ripple.load_rules(
    'GRAPH ?g { ?x <http://test.org/dep> ?z } :- GRAPH ?g { ?x <http://test.org/dep> ?y }, GRAPH ?g { ?y <http://test.org/dep> ?z } .',
    'test_justify_graph'
) > 0 AS rules_loaded;

-- Tenant A chain: aT1 -> aT2 -> aT3  (derives aT1 -> aT3 inside graph A)
SELECT pg_ripple.insert_triple(
    '<http://test.org/gA_aT1>', '<http://test.org/dep>', '<http://test.org/gA_aT2>',
    '<http://test.org/graph/A>'
) IS NOT DISTINCT FROM NULL AS a_edge_1;
SELECT pg_ripple.insert_triple(
    '<http://test.org/gA_aT2>', '<http://test.org/dep>', '<http://test.org/gA_aT3>',
    '<http://test.org/graph/A>'
) IS NOT DISTINCT FROM NULL AS a_edge_2;

-- Tenant B chain: bT1 -> bT2 -> bT3  (derives bT1 -> bT3 inside graph B)
SELECT pg_ripple.insert_triple(
    '<http://test.org/gB_bT1>', '<http://test.org/dep>', '<http://test.org/gB_bT2>',
    '<http://test.org/graph/B>'
) IS NOT DISTINCT FROM NULL AS b_edge_1;
SELECT pg_ripple.insert_triple(
    '<http://test.org/gB_bT2>', '<http://test.org/dep>', '<http://test.org/gB_bT3>',
    '<http://test.org/graph/B>'
) IS NOT DISTINCT FROM NULL AS b_edge_2;

-- Asserted fact on a rule-free predicate (graph A only).
SELECT pg_ripple.insert_triple(
    '<http://test.org/gA_cT1>', '<http://test.org/meta_attached>', '<http://test.org/gA_cT2>',
    '<http://test.org/graph/A>'
) IS NOT DISTINCT FROM NULL AS c_edge;

-- Record derivations during inference.
SET pg_ripple.record_derivations = on;
SELECT (pg_ripple.infer_with_stats('test_justify_graph')->>'derived')::int >= 2 AS inference_derived_both_graphs;
SET pg_ripple.record_derivations = off;

-- Both derived triples must exist, each strictly inside its own graph.
SELECT count(*) = 2 AS derived_triples_present
FROM _pg_ripple.vp_rare
WHERE p = (SELECT id FROM _pg_ripple.dictionary WHERE value = 'http://test.org/dep')
  AND s IN (SELECT id FROM _pg_ripple.dictionary WHERE value IN ('http://test.org/gA_aT1', 'http://test.org/gB_bT1'))
  AND o IN (SELECT id FROM _pg_ripple.dictionary WHERE value IN ('http://test.org/gA_aT3', 'http://test.org/gB_bT3'));

-- ─── JUSTIFY-GRAPH-01: scoped justify builds the tree inside the graph ───────

SELECT (pg_ripple.justify(
    'http://test.org/gA_aT1', 'http://test.org/dep', 'http://test.org/gA_aT3',
    'http://test.org/graph/A'
)->>'type') = 'inferred' AS scoped_justify_inferred_in_graph_a;

SELECT (pg_ripple.justify(
    'http://test.org/gB_bT1', 'http://test.org/dep', 'http://test.org/gB_bT3',
    'http://test.org/graph/B'
)->>'type') = 'inferred' AS scoped_justify_inferred_in_graph_b;

-- ─── JUSTIFY-GRAPH-02: cross-tenant ask is NULL, no existence leak ───────────

-- bT1 -> bT3 exists ONLY in graph B.  Asked with graph A it must be NULL.
SELECT pg_ripple.justify(
    'http://test.org/gB_bT1', 'http://test.org/dep', 'http://test.org/gB_bT3',
    'http://test.org/graph/A'
) IS NULL AS cross_tenant_ask_is_null;

-- Unknown triple inside a known graph is NULL as well.
SELECT pg_ripple.justify(
    'http://test.org/gA_aT1', 'http://test.org/dep', 'http://test.org/nowhere',
    'http://test.org/graph/A'
) IS NULL AS unknown_triple_is_null;

-- Unknown graph is NULL (graph IRI never inserted).
SELECT pg_ripple.justify(
    'http://test.org/gA_aT1', 'http://test.org/dep', 'http://test.org/gA_aT2',
    'http://test.org/graph/does-not-exist'
) IS NULL AS unknown_graph_is_null;

-- ─── JUSTIFY-GRAPH-03: no foreign-graph triple in the proof tree ─────────────

-- The scoped tree for graph A must not contain any graph-B IRI anywhere
-- (conclusion, antecedents, labels included).
SELECT (pg_ripple.justify(
    'http://test.org/gA_aT1', 'http://test.org/dep', 'http://test.org/gA_aT3',
    'http://test.org/graph/A'
)::text NOT LIKE '%gB_%') AS tree_a_has_no_graph_b_triples;

SELECT (pg_ripple.justify(
    'http://test.org/gB_bT1', 'http://test.org/dep', 'http://test.org/gB_bT3',
    'http://test.org/graph/B'
)::text NOT LIKE '%gA_%') AS tree_b_has_no_graph_a_triples;

-- And the antecedent chain of the derivation for graph A is present (the
-- recorder writes a derivation row — stub semantics keep antecedents empty on
-- 0.128, so the assertion is on the derivations array, not its contents).
SELECT jsonb_array_length(
    pg_ripple.justify(
        'http://test.org/gA_aT1', 'http://test.org/dep', 'http://test.org/gA_aT3',
        'http://test.org/graph/A'
    )->'derivations'
) >= 1 AS tree_a_has_derivations;

-- ─── JUSTIFY-GRAPH-06: asserted facts stay base inside their graph ──────────

-- The cT1 -> cT2 fact is asserted on a rule-free predicate: no derivation row
-- exists for it, so the scoped justify answers a base node.
SELECT (pg_ripple.justify(
    'http://test.org/gA_cT1', 'http://test.org/meta_attached', 'http://test.org/gA_cT2',
    'http://test.org/graph/A'
)->>'type') = 'base' AS asserted_fact_is_base_in_graph;

-- ─── JUSTIFY-GRAPH-04: out-of-graph antecedent ⇒ NULL, never partial tree ────

-- Fabricate a derivation row for the ASSERTED rule-free graph-A fact claiming
-- an antecedent that only exists in graph B (simulates a corrupted/legacy row
-- with a non-empty antecedent list, unlike the stub rows above).
INSERT INTO _pg_ripple.derivations (derived_sid, rule_name, rule_set, antecedent_sids)
SELECT rare_a.i, 'fabricated_cross_graph_rule', 'test_justify_graph',
       ARRAY[rare_b.i]
FROM _pg_ripple.vp_rare rare_a,
     _pg_ripple.vp_rare rare_b
WHERE rare_a.p = (SELECT id FROM _pg_ripple.dictionary WHERE value = 'http://test.org/meta_attached')
  AND rare_a.s = (SELECT id FROM _pg_ripple.dictionary WHERE value = 'http://test.org/gA_cT1')
  AND rare_a.o = (SELECT id FROM _pg_ripple.dictionary WHERE value = 'http://test.org/gA_cT2')
  AND rare_a.g = (SELECT id FROM _pg_ripple.dictionary WHERE value = 'http://test.org/graph/A')
  AND rare_b.p = (SELECT id FROM _pg_ripple.dictionary WHERE value = 'http://test.org/dep')
  AND rare_b.s = (SELECT id FROM _pg_ripple.dictionary WHERE value = 'http://test.org/gB_bT1')
  AND rare_b.o = (SELECT id FROM _pg_ripple.dictionary WHERE value = 'http://test.org/gB_bT2')
  AND rare_b.g = (SELECT id FROM _pg_ripple.dictionary WHERE value = 'http://test.org/graph/B');

-- The scoped justify must NOT present the fact with a proof reaching into
-- graph B: NULL (incomplete proof), even though the triple itself is in A.
SELECT pg_ripple.justify(
    'http://test.org/gA_cT1', 'http://test.org/meta_attached', 'http://test.org/gA_cT2',
    'http://test.org/graph/A'
) IS NULL AS incomplete_proof_is_null;

-- Remove the fabricated row before the legacy-signature assertions.
DELETE FROM _pg_ripple.derivations
WHERE rule_name = 'fabricated_cross_graph_rule'
  AND rule_set = 'test_justify_graph';

-- ─── JUSTIFY-GRAPH-07: legacy 3-arg signature unchanged ──────────────────────

-- The 3-arg justify keeps its global semantics: it still answers for the
-- graph-B fact (documented backward-compat; engine callers gate by graph).
SELECT pg_ripple.justify(
    'http://test.org/gB_bT1', 'http://test.org/dep', 'http://test.org/gB_bT3'
) IS NOT NULL AS legacy_justify_still_global;

-- ─── Cleanup ──────────────────────────────────────────────────────────────────

SELECT pg_ripple.drop_rules('test_justify_graph') IS NOT DISTINCT FROM NULL AS cleanup_rules;

SELECT count(*) = 0 AS cleanup_fabricated_derivation_gone
FROM _pg_ripple.derivations
WHERE rule_set = 'test_justify_graph'
  AND rule_name = 'fabricated_cross_graph_rule';
