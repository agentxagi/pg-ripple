-- v0.141.0 — graph-scoped justify resolves provenance by triple (VAL-391).
--
-- Regression target: on 0.140.x the graph-scoped justify(s, p, o, g) looked
-- up `_pg_ripple.derivations` strictly by the caller's graph statement ID.
-- A cross-tenant copy of a derived triple (same s/p/o materialised in
-- another graph as an asserted row) therefore justified as `base` FOREVER,
-- even when the asking graph held every antecedent of the recorded rule
-- application — the recorded rows hang off the recording graph's SID, and
-- antecedent validation matched foreign SIDs against the asking graph.
-- Evidence: ValorBrain VAL-187/VAL-391, tenant graphs holding mirrored
-- copies of `kg/indirectly_runs_on` conclusions (kg-explain-proof-types
-- suite, "both owners get the tree" case); the engine mapped the bare base
-- proof to `unverified`.
--
-- Fix under test (build_proof_tree_graph, src/datalog/derivations.rs): when
-- the node's own row carries no derivation, justify falls back to derivation
-- rows recorded for ANY materialisation of the same logical triple, with
-- antecedents re-resolved inside the caller's graph:
--   VAL391-1: the recording graph still justifies as inferred (baseline);
--   VAL391-2: a graph holding ONLY the conclusion gets `base` — an
--             incomplete tree is never returned (leak rule kept);
--   VAL391-3: a graph holding conclusion AND antecedents gets `inferred`
--             with its own SIDs — no re-inference needed;
--   VAL391-4: near-miss antecedents (same predicates, different objects)
--             do not unlock the tree;
--   VAL391-5: the graph-blind 3-arg justify is unchanged.
--
-- Namespace: https://val391.test/

SET client_min_messages = error;
CREATE EXTENSION IF NOT EXISTS pg_ripple;
SET search_path TO pg_ripple, public;

-- Load so _PG_init registers GUCs (no shared_preload_libraries in tests).
LOAD '$libdir/pg_ripple';
SET client_min_messages = DEFAULT;

SET pg_ripple.record_derivations = on;

SELECT pg_ripple.drop_rules('v391_rules') >= 0 AS v391_ruleset_dropped;

-- Rule with a graph-scoped head/body: derivations happen per named graph.
SELECT pg_ripple.load_rules(
    'GRAPH ?g { ?x <https://val391.test/indirect> ?z } :- GRAPH ?g { ?x <https://val391.test/runs> ?y }, GRAPH ?g { ?y <https://val391.test/deps> ?z } .',
    'v391_rules'
) = 1 AS v391_rule_loaded;

-- Graph A: the antecedents and the deriving pass live here.
SELECT pg_ripple.insert_triple(
    '<https://val391.test/a>', '<https://val391.test/runs>', '<https://val391.test/b>',
    '<https://val391.test/graphA>') >= 1 AS v391_a_runs_inserted;
SELECT pg_ripple.insert_triple(
    '<https://val391.test/b>', '<https://val391.test/deps>', '<https://val391.test/c>',
    '<https://val391.test/graphA>') >= 1 AS v391_a_deps_inserted;

SELECT pg_ripple.infer('v391_rules') >= 1 AS v391_a_infer_derived;

-- VAL391-1: recording graph justifies as inferred.
SELECT (pg_ripple.justify(
    'https://val391.test/a', 'https://val391.test/indirect', 'https://val391.test/c',
    'https://val391.test/graphA') ->> 'type') = 'inferred'
    AS v391_1_recording_graph_inferred;

-- VAL391-5: graph-blind justify still infers (lowest-SID row is A's derived
-- row here; unchanged behaviour).
SELECT (pg_ripple.justify(
    'https://val391.test/a', 'https://val391.test/indirect', 'https://val391.test/c'
) ->> 'type') = 'inferred'
    AS v391_5_blind_justify_inferred;

-- Graph B: an asserted copy of the conclusion only.
SELECT pg_ripple.insert_triple(
    '<https://val391.test/a>', '<https://val391.test/indirect>', '<https://val391.test/c>',
    '<https://val391.test/graphB>') >= 1 AS v391_b_conclusion_copy_inserted;

-- VAL391-2: B holds only the conclusion → base, no incomplete tree.
SELECT (pg_ripple.justify(
    'https://val391.test/a', 'https://val391.test/indirect', 'https://val391.test/c',
    'https://val391.test/graphB') ->> 'type') = 'base'
    AS v391_2_conclusion_only_base;

-- VAL391-4: near-miss antecedents in B (different objects) do not unlock.
SELECT pg_ripple.insert_triple(
    '<https://val391.test/a>', '<https://val391.test/runs>', '<https://val391.test/b2>',
    '<https://val391.test/graphB>') >= 1 AS v391_b_nearmiss_runs_inserted;
SELECT pg_ripple.insert_triple(
    '<https://val391.test/b2>', '<https://val391.test/deps>', '<https://val391.test/c2>',
    '<https://val391.test/graphB>') >= 1 AS v391_b_nearmiss_deps_inserted;

SELECT (pg_ripple.justify(
    'https://val391.test/a', 'https://val391.test/indirect', 'https://val391.test/c',
    'https://val391.test/graphB') ->> 'type') = 'base'
    AS v391_4_nearmiss_antecedents_base;

-- VAL391-3: the real antecedents land in B — the tree follows, with B's own
-- statement IDs, WITHOUT any re-inference pass.
SELECT pg_ripple.insert_triple(
    '<https://val391.test/a>', '<https://val391.test/runs>', '<https://val391.test/b>',
    '<https://val391.test/graphB>') >= 1 AS v391_b_runs_inserted;
SELECT pg_ripple.insert_triple(
    '<https://val391.test/b>', '<https://val391.test/deps>', '<https://val391.test/c>',
    '<https://val391.test/graphB>') >= 1 AS v391_b_deps_inserted;

SELECT (pg_ripple.justify(
    'https://val391.test/a', 'https://val391.test/indirect', 'https://val391.test/c',
    'https://val391.test/graphB') ->> 'type') = 'inferred'
    AS v391_3_both_antecedents_inferred;

-- The rebuilt tree names only B's rows: two base antecedents, one rule.
SELECT jsonb_array_length(
    pg_ripple.justify(
        'https://val391.test/a', 'https://val391.test/indirect', 'https://val391.test/c',
        'https://val391.test/graphB') -> 'derivations') = 1
    AS v391_3_one_derivation_branch;
SELECT jsonb_array_length(
    pg_ripple.justify(
        'https://val391.test/a', 'https://val391.test/indirect', 'https://val391.test/c',
        'https://val391.test/graphB') -> 'derivations' -> 0 -> 'antecedents') = 2
    AS v391_3_two_antecedent_nodes;
SELECT (pg_ripple.justify(
    'https://val391.test/a', 'https://val391.test/indirect', 'https://val391.test/c',
    'https://val391.test/graphB') -> 'derivations' -> 0 -> 'antecedents' -> 0 ->> 'type') = 'base'
    AS v391_3_antecedents_are_base_nodes;

-- Reading the tree repeatedly stays inferred (no first-read-only window).
SELECT (pg_ripple.justify(
    'https://val391.test/a', 'https://val391.test/indirect', 'https://val391.test/c',
    'https://val391.test/graphB') ->> 'type') = 'inferred'
    AS v391_3_reread_still_inferred;

-- ── cleanup: leave no fixture behind in the shared regress database ──────────
SELECT pg_ripple.delete_triple_from_graph(
    '<https://val391.test/a>', '<https://val391.test/indirect>', '<https://val391.test/c>',
    '<https://val391.test/graphA>') >= 0 AS v391_cleanup_a_conclusion;
SELECT pg_ripple.delete_triple_from_graph(
    '<https://val391.test/a>', '<https://val391.test/runs>', '<https://val391.test/b>',
    '<https://val391.test/graphA>') >= 0 AS v391_cleanup_a_runs;
SELECT pg_ripple.delete_triple_from_graph(
    '<https://val391.test/b>', '<https://val391.test/deps>', '<https://val391.test/c>',
    '<https://val391.test/graphA>') >= 0 AS v391_cleanup_a_deps;
SELECT pg_ripple.delete_triple_from_graph(
    '<https://val391.test/a>', '<https://val391.test/indirect>', '<https://val391.test/c>',
    '<https://val391.test/graphB>') >= 0 AS v391_cleanup_b_conclusion;
SELECT pg_ripple.delete_triple_from_graph(
    '<https://val391.test/a>', '<https://val391.test/runs>', '<https://val391.test/b2>',
    '<https://val391.test/graphB>') >= 0 AS v391_cleanup_b_nearmiss_runs;
SELECT pg_ripple.delete_triple_from_graph(
    '<https://val391.test/b2>', '<https://val391.test/deps>', '<https://val391.test/c2>',
    '<https://val391.test/graphB>') >= 0 AS v391_cleanup_b_nearmiss_deps;
SELECT pg_ripple.delete_triple_from_graph(
    '<https://val391.test/a>', '<https://val391.test/runs>', '<https://val391.test/b>',
    '<https://val391.test/graphB>') >= 0 AS v391_cleanup_b_runs;
SELECT pg_ripple.delete_triple_from_graph(
    '<https://val391.test/b>', '<https://val391.test/deps>', '<https://val391.test/c>',
    '<https://val391.test/graphB>') >= 0 AS v391_cleanup_b_deps;

DELETE FROM _pg_ripple.derivations WHERE rule_set = 'v391_rules';
SELECT pg_ripple.drop_rules('v391_rules') >= 0 AS v391_ruleset_dropped_again;
SELECT count(*) = 0 AS v391_no_derivation_rows_left
FROM _pg_ripple.derivations WHERE rule_set = 'v391_rules';

RESET pg_ripple.record_derivations;
