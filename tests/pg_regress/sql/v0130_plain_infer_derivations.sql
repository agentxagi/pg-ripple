-- v0.130.0 Confirmation experiment + regression tests (VAL-207, option A).
--
-- Production write-path shape: the derived predicate has a dedicated VP table
-- (depends_on → vp_534, caused_by → vp_538, indirectly_runs_on → vp_590).
--
-- Covers:
--   VAL207-PLAIN-1: plain infer() materialises the derived fact in the
--                   predicate's HTAP delta with source = 1 (was 0)
--   VAL207-PLAIN-2: plain infer() records the derivation when
--                   pg_ripple.record_derivations = on (was: never recorded)
--   VAL207-PLAIN-3: justify() returns an inferred proof tree for a fact that
--                   lives in {vp}_delta (was: NULL — vp_rare-only lookup)
--   VAL207-PLAIN-4: dedicated SPARQL sees the derived fact (kept from 0.128)
--   VAL207-DUP-1:   infer_with_stats() on a promoted predicate materialises
--                   into the canonical delta — no second copy in vp_rare
--                   (was: same logical fact in two storages, distinct SIDs)
--   VAL207-DUP-2:   re-running both paths over the same rule leaves exactly
--                   one stored copy of the fact
--   VAL207-CONFLICT: rule_conflicts('runtime') still detects conflicts whose
--                   derived rows live in the promoted delta (scan follows the
--                   canonical storage)
--
-- Namespace: https://val207.test/

SET client_min_messages = error;
CREATE EXTENSION IF NOT EXISTS pg_ripple;
SET search_path TO pg_ripple, public;

-- Load so _PG_init registers GUCs (no shared_preload_libraries in tests).
LOAD '$libdir/pg_ripple';
SET client_min_messages = DEFAULT;

SET pg_ripple.record_derivations = on;

-- ── helpers ───────────────────────────────────────────────────────────────────

-- Rows of a predicate's HTAP delta table (optionally inferred-only).
CREATE FUNCTION pg_temp.delta_count(pred TEXT, inferred_only BOOL) RETURNS BIGINT AS $$
DECLARE
    n BIGINT;
    p_id BIGINT;
BEGIN
    SELECT id INTO p_id FROM _pg_ripple.dictionary WHERE value = pred LIMIT 1;
    EXECUTE format(
        'SELECT count(*) FROM _pg_ripple.vp_%s_delta WHERE ($2 = false OR source = 1)',
        p_id)
    INTO n USING p_id, inferred_only;
    RETURN n;
END $$ LANGUAGE plpgsql;

-- Rows of a predicate in vp_rare (optionally inferred-only).
CREATE FUNCTION pg_temp.rare_count(pred TEXT, inferred_only BOOL) RETURNS BIGINT AS $$
DECLARE
    n BIGINT;
    p_id BIGINT;
BEGIN
    SELECT id INTO p_id FROM _pg_ripple.dictionary WHERE value = pred LIMIT 1;
    SELECT count(*) INTO n FROM _pg_ripple.vp_rare
    WHERE p = p_id AND (NOT inferred_only OR source = 1);
    RETURN n;
END $$ LANGUAGE plpgsql;

-- How many stored copies (vp_rare + dedicated delta) a logical fact has.
CREATE FUNCTION pg_temp.fact_copies(subject TEXT, pred TEXT, object TEXT) RETURNS BIGINT AS $$
DECLARE
    n BIGINT;
    s_id BIGINT; p_id BIGINT; o_id BIGINT;
BEGIN
    SELECT id INTO s_id FROM _pg_ripple.dictionary WHERE value = subject LIMIT 1;
    SELECT id INTO p_id FROM _pg_ripple.dictionary WHERE value = pred LIMIT 1;
    SELECT id INTO o_id FROM _pg_ripple.dictionary WHERE value = object LIMIT 1;
    EXECUTE format(
        'SELECT (SELECT count(*) FROM _pg_ripple.vp_rare
                 WHERE p = $1 AND s = $2 AND o = $3)
              + (SELECT count(*) FROM _pg_ripple.vp_%s_delta
                 WHERE s = $2 AND o = $3)',
        p_id)
    INTO n USING p_id, s_id, o_id;
    RETURN n;
END $$ LANGUAGE plpgsql;

-- ── production shape: derived predicate with a dedicated VP table ────────────

SET pg_ripple.vp_promotion_threshold = 100;

DO $$
DECLARE i INT;
BEGIN
    FOR i IN 1..100 LOOP
        PERFORM pg_ripple.insert_triple(
            '<https://val207.test/Svc' || i || '>',
            '<https://val207.test/depends_on>',
            '<https://val207.test/SvcA>');
    END LOOP;
END $$;

SELECT table_oid IS NOT NULL AS val207_dep_has_dedicated_vp
FROM _pg_ripple.predicates
WHERE id = (SELECT id FROM _pg_ripple.dictionary
            WHERE value = 'https://val207.test/depends_on');

-- ── VAL207-PLAIN: infer() records provenance and the fact stays visible ──────

SELECT pg_ripple.drop_rules('v207_plain') >= 0 AS plain_ruleset_dropped;

SELECT pg_ripple.load_rules(
    '?x <https://val207.test/depends_on> <https://val207.test/SvcB> :- ?x <https://val207.test/needs_a> "a" .',
    'v207_plain'
) = 1 AS plain_rule_loaded;

SELECT pg_ripple.insert_triple(
    '<https://val207.test/Node1>',
    '<https://val207.test/needs_a>',
    '"a"'
) >= 1 AS plain_body_fact_inserted;

SELECT pg_ripple.infer('v207_plain') >= 1 AS plain_infer_derived;

-- The derived fact carries source = 1 in the promoted delta (CONFLICT-03 parity).
SELECT pg_temp.delta_count('https://val207.test/depends_on', true) = 1
    AS val207_plain1_delta_source_1;

-- The plain path now records the derivation.
SELECT count(*) >= 1 AS val207_plain2_derivation_recorded
FROM _pg_ripple.derivations WHERE rule_set = 'v207_plain';

-- justify() reaches facts living in {vp}_delta and reports them as inferred.
SELECT (pg_ripple.justify(
    'https://val207.test/Node1',
    'https://val207.test/depends_on',
    'https://val207.test/SvcB'
)->>'type') = 'inferred' AS val207_plain3_justify_inferred;

-- Dedicated SPARQL (predicate with its own VP table) sees the derived fact.
SELECT result AS val207_plain4_dedicated_sparql_sees_fact
FROM pg_ripple.sparql(
    'ASK { <https://val207.test/Node1> <https://val207.test/depends_on> <https://val207.test/SvcB> }'
) AS result;

-- ── VAL207-DUP: no double materialisation for promoted predicates ────────────

-- infer_with_stats() over the same rule set must NOT copy the fact into
-- vp_rare; the canonical storage is the promoted delta.
SELECT (pg_ripple.infer_with_stats('v207_plain') ->> 'derived')::bigint >= 0
    AS stats_infer_runs;

SELECT pg_temp.rare_count('https://val207.test/depends_on', false) = 0
    AS val207_dup1_no_vp_rare_copy;

-- Exactly one stored copy of the fact across both storages.
SELECT pg_temp.fact_copies(
    'https://val207.test/Node1',
    'https://val207.test/depends_on',
    'https://val207.test/SvcB') = 1 AS val207_dup2_single_copy;

-- A fact derived after a re-run over the same rule set is also justified
-- (canonical delta storage, single copy).
SELECT pg_ripple.insert_triple(
    '<https://val207.test/Node2>',
    '<https://val207.test/needs_a>',
    '"a"'
) >= 1 AS stats_body_fact_inserted;

SELECT pg_ripple.infer('v207_plain') >= 0 AS stats_plain_reinfer;

SELECT (pg_ripple.justify(
    'https://val207.test/Node2',
    'https://val207.test/depends_on',
    'https://val207.test/SvcB'
)->>'type') = 'inferred' AS stats_fact_justify_inferred;

-- ── VAL207-CONFLICT: runtime scan follows the canonical delta ────────────────

SELECT pg_ripple.drop_rules('v207_conf') >= 0 AS conf_ruleset_dropped;

SELECT pg_ripple.load_rules(
    '?x <https://val207.test/depends_on> <https://val207.test/SvcD> :- ?x <https://val207.test/needs_d> "d" .
     ?x <https://val207.test/depends_on> <https://val207.test/SvcE> :- ?x <https://val207.test/needs_e> "e" .',
    'v207_conf'
) = 2 AS conf_rules_loaded;

SELECT pg_ripple.insert_triple('<https://val207.test/Node3>', '<https://val207.test/needs_d>', '"d"') >= 1
    AS conf_body_d_inserted;
SELECT pg_ripple.insert_triple('<https://val207.test/Node3>', '<https://val207.test/needs_e>', '"e"') >= 1
    AS conf_body_e_inserted;

SELECT pg_ripple.infer('v207_conf') >= 2 AS conf_infer_derived_pair;

-- Both derived rows must carry source = 1 in the promoted delta.
SELECT pg_temp.delta_count('https://val207.test/depends_on', true) >= 3
    AS conf_delta_rows_source_1;

-- The runtime scan joins derivations with the canonical storage and must find
-- the contradiction (two distinct inferred depends_on values for Node3).
SELECT jsonb_array_length(pg_ripple.rule_conflicts('v207_conf', 'runtime')) > 0
    AS val207_conflict_scan_detects;

RESET pg_ripple.vp_promotion_threshold;
