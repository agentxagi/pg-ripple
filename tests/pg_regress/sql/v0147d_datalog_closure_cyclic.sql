-- pg_regress test: recursive Datalog rules compute the closure as a set on
-- cyclic graphs (v0147d).
--
-- Regression for: compile_recursive_rule emitted
--   WITH RECURSIVE derived_N(...) AS (base UNION step)
--   CYCLE s, o, g SET is_cycle USING cycle_path ... WHERE NOT is_cycle
-- CYCLE adds the path to every row, so UNION kept one row per distinct path:
-- the recursion enumerated every simple path instead of the closure.  On a
-- production depends_on graph with cycles the inference never finished; the
-- same closure without CYCLE took 73 ms.
--
-- Graphs (the same node IRIs in both, so a closure leaking across graphs shows):
--   g/a  dep:   ring of 40 nodes, edges i->i+1 and i->i+2 (strongly connected)
--        cause: ring of 24 nodes, edges i->i+1 and i->i+2
--   g/b  dep:   three rings of 20 nodes (edges +1 and +3), bridges n5->n27, n33->n41
--   runs: s0->n0, s1->n17 in g/a; s0->n2, s2->n45, s3->n25 in g/b
--   dep_p, cause_p, runs_p: copies read by the plain-path (infer) production rules
--   link: a copy of dep, read by the plain-path and bounded-depth rule sets
--   g/c1..c3  ng: S->O in c1, S->Y in c2, Y->S in c3, for a rule without GRAPH
-- The expected sets are computed here from a plain edge table (path doubling
-- and walk expansion), independently of the extension.  statement_timeout and
-- temp_file_limit bound each inference: path enumeration fails them, the
-- closure needs neither.

CREATE EXTENSION IF NOT EXISTS pg_ripple;
SELECT pg_ripple.triple_count() >= 0 AS library_loaded;
SET search_path TO pg_ripple, public;

SELECT bool_and(pg_ripple.drop_rules(r) >= 0) AS rules_reset
FROM unnest(ARRAY['cyc_prod', 'cyc_prodp', 'cyc_plain', 'cyc_depth3', 'cyc_depth30', 'cyc_ng']) AS r;
SELECT bool_and(pg_ripple.drop_graph('https://cyc.test/g/' || g) >= 0) AS graphs_reset
FROM unnest(ARRAY['a', 'b', 'c1', 'c2', 'c3']) AS g;

-- ── Data ──────────────────────────────────────────────────────────────────────

CREATE TEMP TABLE cyc_edge (p text NOT NULL, s text NOT NULL, o text NOT NULL, g text NOT NULL);
INSERT INTO cyc_edge
SELECT 'dep', 'n' || i, 'n' || ((i + d) % 40), 'a'
FROM generate_series(0, 39) AS i, (VALUES (1), (2)) AS v(d)
UNION ALL
SELECT 'dep', 'n' || (b + i), 'n' || (b + (i + d) % 20), 'b'
FROM generate_series(0, 19) AS i, (VALUES (0), (20), (40)) AS w(b), (VALUES (1), (3)) AS v(d)
UNION ALL
SELECT 'dep', s, o, 'b' FROM (VALUES ('n5', 'n27'), ('n33', 'n41')) AS x(s, o)
UNION ALL
SELECT 'cause', 'c' || i, 'c' || ((i + d) % 24), 'a'
FROM generate_series(0, 23) AS i, (VALUES (1), (2)) AS v(d)
UNION ALL
SELECT 'runs', s, o, g
FROM (VALUES ('s0', 'n0', 'a'), ('s1', 'n17', 'a'),
             ('s0', 'n2', 'b'), ('s2', 'n45', 'b'), ('s3', 'n25', 'b')) AS x(s, o, g)
UNION ALL
SELECT 'ng', s, o, g FROM (VALUES ('S', 'O', 'c1'), ('S', 'Y', 'c2'), ('Y', 'S', 'c3')) AS x(s, o, g);
INSERT INTO cyc_edge
SELECT p || '_p', s, o, g FROM cyc_edge WHERE p IN ('dep', 'cause', 'runs')
UNION ALL
SELECT 'link', s, o, g FROM cyc_edge WHERE p = 'dep';

SELECT p, g, count(*) AS edges FROM cyc_edge GROUP BY p, g ORDER BY p, g;

SELECT pg_ripple.load_nquads(string_agg(
         format('<https://cyc.test/%s> <https://cyc.test/%s> <https://cyc.test/%s> <https://cyc.test/g/%s> .',
                s, p, o, g),
         E'\n')) = count(*) AS edges_loaded
FROM cyc_edge;

-- ── Expected sets, computed without the extension ─────────────────────────────

-- Transitive closure by path doubling: per graph for dep, cause, dep_p, cause_p
-- and link; over all graphs at once (g = '*') for ng.
CREATE TEMP TABLE cyc_closure (p text, s text, o text, g text, PRIMARY KEY (p, s, o, g));
INSERT INTO cyc_closure
SELECT DISTINCT p, s, o, CASE WHEN p = 'ng' THEN '*' ELSE g END
FROM cyc_edge WHERE p IN ('dep', 'cause', 'dep_p', 'cause_p', 'link', 'ng');
DO $$
DECLARE
    added bigint;
BEGIN
    LOOP
        INSERT INTO cyc_closure
        SELECT DISTINCT a.p, a.s, b.o, a.g
        FROM cyc_closure a
        JOIN cyc_closure b ON b.p = a.p AND b.g = a.g AND b.s = a.o
        ON CONFLICT DO NOTHING;
        GET DIAGNOSTICS added = ROW_COUNT;
        EXIT WHEN added = 0;
    END LOOP;
END $$;

-- Pairs of link joined by a walk of at most 4 and at most 31 edges
-- (datalog_max_depth = 3 and 30: depth 0 is the edge itself).
CREATE TEMP TABLE cyc_walk (s text, o text, g text, PRIMARY KEY (s, o, g));
CREATE TEMP TABLE cyc_within (hops int, s text, o text, g text);
INSERT INTO cyc_walk SELECT s, o, g FROM cyc_edge WHERE p = 'link';
DO $$
BEGIN
    FOR k IN 2..31 LOOP
        INSERT INTO cyc_walk
        SELECT DISTINCT e.s, w.o, e.g
        FROM cyc_edge e
        JOIN cyc_walk w ON w.s = e.o AND w.g = e.g
        WHERE e.p = 'link'
        ON CONFLICT DO NOTHING;
        IF k IN (4, 31) THEN
            INSERT INTO cyc_within SELECT k, s, o, g FROM cyc_walk;
        END IF;
    END LOOP;
END $$;

SELECT p, g, count(*) AS closure FROM cyc_closure GROUP BY p, g ORDER BY p, g;
SELECT hops, g, count(*) AS pairs FROM cyc_within GROUP BY hops, g ORDER BY hops, g;

-- Expected contents of g/a and g/b, grown stage by stage: asserted edges first.
CREATE TEMP TABLE cyc_expected AS
SELECT p, s, o, g FROM cyc_edge WHERE g IN ('a', 'b');

-- Snapshot of g/a and g/b through SPARQL.  sparql() caches each translation
-- per backend, and a cached variable-predicate query does not read a dedicated
-- VP table created after it was cached, so the cache is reset first.
CREATE TEMP TABLE cyc_got (p text, s text, o text, g text);
CREATE FUNCTION pg_temp.cyc_snapshot() RETURNS bigint LANGUAGE sql AS $$
    SELECT pg_ripple.plan_cache_reset();
    DELETE FROM cyc_got;
    INSERT INTO cyc_got
    SELECT substring(t.r->>'p' FROM '^<https://cyc[.]test/(.*)>$'),
           substring(t.r->>'s' FROM '^<https://cyc[.]test/(.*)>$'),
           substring(t.r->>'o' FROM '^<https://cyc[.]test/(.*)>$'),
           v.g
    FROM (VALUES ('a'), ('b')) AS v(g),
         LATERAL pg_ripple.sparql('SELECT ?s ?p ?o WHERE { GRAPH <https://cyc.test/g/' || v.g || '> { ?s ?p ?o } }') AS t(r);
    SELECT count(*) FROM (SELECT DISTINCT * FROM cyc_got) x;
$$;
CREATE TEMP VIEW cyc_check AS
SELECT (SELECT count(*) FROM (SELECT * FROM cyc_expected EXCEPT SELECT * FROM cyc_got) x) AS missing,
       (SELECT count(*) FROM (SELECT * FROM cyc_got EXCEPT SELECT * FROM cyc_expected) x) AS unexpected,
       (SELECT count(*) FROM cyc_got) - (SELECT count(*) FROM (SELECT DISTINCT * FROM cyc_got) x) AS duplicates;

SET pg_ripple.rule_graph_scope = 'all';
SET statement_timeout = '30s';
SET temp_file_limit = '256MB';

-- ── Production rule set, semi-naive (infer_with_stats) ────────────────────────

SELECT pg_ripple.load_rules(
    'GRAPH ?g { ?x <https://cyc.test/dep> ?z } :-
       GRAPH ?g { ?x <https://cyc.test/dep> ?y },
       GRAPH ?g { ?y <https://cyc.test/dep> ?z } .
     GRAPH ?g { ?s <https://cyc.test/indirect> ?i } :-
       GRAPH ?g { ?s <https://cyc.test/runs> ?h },
       GRAPH ?g { ?h <https://cyc.test/dep> ?i } .
     GRAPH ?g { ?x <https://cyc.test/cause> ?z } :-
       GRAPH ?g { ?x <https://cyc.test/cause> ?y },
       GRAPH ?g { ?y <https://cyc.test/cause> ?z } .',
    'cyc_prod'
) AS prod_rules_loaded;

-- The compiled recursive rules are recursive CTEs without a CYCLE clause.
SELECT count(*) AS rules,
       count(*) FILTER (WHERE r->>'sql' LIKE 'WITH RECURSIVE%') AS recursive_ctes,
       count(*) FILTER (WHERE r->>'sql' ~ 'CYCLE|is_cycle|cycle_path') AS with_cycle_clause
FROM jsonb_array_elements(pg_ripple.explain_datalog('cyc_prod')->'sql_per_rule') AS r;

INSERT INTO cyc_expected
SELECT p, s, o, g FROM cyc_closure WHERE p IN ('dep', 'cause')
UNION
SELECT 'indirect', r.s, c.o, r.g
FROM cyc_edge r
JOIN cyc_closure c ON c.p = 'dep' AND c.g = r.g AND c.s = r.o
WHERE r.p = 'runs'
EXCEPT
SELECT * FROM cyc_expected;

-- First pass: everything in the closure that was not asserted is derived.
SELECT (pg_ripple.infer_with_stats('cyc_prod')->>'derived')::bigint AS first_pass_derived,
       (SELECT count(*) FROM cyc_expected) - (SELECT count(*) FROM cyc_edge WHERE g IN ('a', 'b'))
           AS expected_derived;
SELECT pg_temp.cyc_snapshot() AS triples_read;
SELECT p, g, count(*) AS triples FROM cyc_got WHERE p IN ('dep', 'cause', 'indirect') GROUP BY p, g ORDER BY p, g;
SELECT * FROM cyc_check;

-- Second pass over the same data derives nothing.
SELECT (pg_ripple.infer_with_stats('cyc_prod')->>'derived')::bigint AS second_pass_derived;
SELECT pg_temp.cyc_snapshot() AS triples_read;
SELECT * FROM cyc_check;

-- ── The same production rules through the plain path (infer) ──────────────────

SELECT pg_ripple.load_rules(
    'GRAPH ?g { ?x <https://cyc.test/dep_p> ?z } :-
       GRAPH ?g { ?x <https://cyc.test/dep_p> ?y },
       GRAPH ?g { ?y <https://cyc.test/dep_p> ?z } .
     GRAPH ?g { ?s <https://cyc.test/indirect_p> ?i } :-
       GRAPH ?g { ?s <https://cyc.test/runs_p> ?h },
       GRAPH ?g { ?h <https://cyc.test/dep_p> ?i } .
     GRAPH ?g { ?x <https://cyc.test/cause_p> ?z } :-
       GRAPH ?g { ?x <https://cyc.test/cause_p> ?y },
       GRAPH ?g { ?y <https://cyc.test/cause_p> ?z } .',
    'cyc_prodp'
) AS prodp_rules_loaded;

INSERT INTO cyc_expected
SELECT p, s, o, g FROM cyc_closure WHERE p IN ('dep_p', 'cause_p')
UNION
SELECT 'indirect_p', r.s, c.o, r.g
FROM cyc_edge r
JOIN cyc_closure c ON c.p = 'dep_p' AND c.g = r.g AND c.s = r.o
WHERE r.p = 'runs_p'
EXCEPT
SELECT * FROM cyc_expected;

-- The plain path runs each rule once, and its strata put the non-recursive
-- indirect_p rule (stratum 0) before the recursive dep_p rule: the first pass
-- completes dep_p and cause_p, and indirect_p only joins the asserted dep_p
-- edges (190 short).  The second pass completes it; the third adds nothing.
-- Its inferred copies of asserted dep_p and cause_p rows also show twice in a
-- variable-predicate scan (duplicates = 250).  Both predate this fix.
SELECT pg_ripple.infer('cyc_prodp') >= 0 AS plain_first_pass;
SELECT pg_temp.cyc_snapshot() - (SELECT count(*) FROM cyc_expected) AS short_of_expected;
SELECT p, g, count(DISTINCT (s, o)) AS triples FROM cyc_got WHERE p LIKE '%\_p' GROUP BY p, g ORDER BY p, g;
SELECT * FROM cyc_check;

SELECT pg_ripple.infer('cyc_prodp') >= 0 AS plain_second_pass;
SELECT pg_temp.cyc_snapshot() - (SELECT count(*) FROM cyc_expected) AS short_of_expected;
SELECT * FROM cyc_check;

-- Third plain pass: not one triple added.
SELECT pg_ripple.infer('cyc_prodp') >= 0 AS plain_third_pass;
SELECT pg_temp.cyc_snapshot() - (SELECT count(*) FROM cyc_expected) AS third_pass_added;

-- ── Plain path on link (rules with a base atom) and bounded depth ─────────────

SELECT pg_ripple.load_rules(
    'GRAPH ?g { ?x <https://cyc.test/reach> ?y } :- GRAPH ?g { ?x <https://cyc.test/link> ?y } .
     GRAPH ?g { ?x <https://cyc.test/reach> ?z } :-
       GRAPH ?g { ?x <https://cyc.test/link> ?y },
       GRAPH ?g { ?y <https://cyc.test/reach> ?z } .',
    'cyc_plain'
) AS plain_rules_loaded;
SELECT pg_ripple.load_rules(
    'GRAPH ?g { ?x <https://cyc.test/reach3> ?y } :- GRAPH ?g { ?x <https://cyc.test/link> ?y } .
     GRAPH ?g { ?x <https://cyc.test/reach3> ?z } :-
       GRAPH ?g { ?x <https://cyc.test/link> ?y },
       GRAPH ?g { ?y <https://cyc.test/reach3> ?z } .',
    'cyc_depth3'
) AS depth3_rules_loaded;
SELECT pg_ripple.load_rules(
    'GRAPH ?g { ?x <https://cyc.test/reach30> ?y } :- GRAPH ?g { ?x <https://cyc.test/link> ?y } .
     GRAPH ?g { ?x <https://cyc.test/reach30> ?z } :-
       GRAPH ?g { ?x <https://cyc.test/link> ?y },
       GRAPH ?g { ?y <https://cyc.test/reach30> ?z } .',
    'cyc_depth30'
) AS depth30_rules_loaded;

SET pg_ripple.datalog_max_depth = 30;
SELECT count(*) FILTER (WHERE r->>'sql' LIKE 'WITH RECURSIVE%') AS recursive_ctes,
       count(*) FILTER (WHERE r->>'sql' ~ 'CYCLE|is_cycle|cycle_path') AS with_cycle_clause
FROM jsonb_array_elements(pg_ripple.explain_datalog('cyc_depth30')->'sql_per_rule') AS r;

SET pg_ripple.datalog_max_depth = 0;
SELECT pg_ripple.infer('cyc_plain') >= 0 AS plain_inferred;
SET pg_ripple.datalog_max_depth = 3;
SELECT pg_ripple.infer('cyc_depth3') >= 0 AS depth3_inferred;
SET pg_ripple.datalog_max_depth = 30;
SELECT pg_ripple.infer('cyc_depth30') >= 0 AS depth30_inferred;
SET pg_ripple.datalog_max_depth = 0;

INSERT INTO cyc_expected
SELECT 'reach', s, o, g FROM cyc_closure WHERE p = 'link'
UNION ALL
SELECT 'reach3', s, o, g FROM cyc_within WHERE hops = 4
UNION ALL
SELECT 'reach30', s, o, g FROM cyc_within WHERE hops = 31;

SELECT pg_temp.cyc_snapshot() AS triples_read;
SELECT p, g, count(DISTINCT (s, o)) AS triples FROM cyc_got WHERE p LIKE 'reach%' GROUP BY p, g ORDER BY p, g;
SELECT * FROM cyc_check;

-- A second plain pass adds nothing.
SELECT pg_ripple.infer('cyc_plain') >= 0 AS plain_inferred_again;
SELECT pg_temp.cyc_snapshot() - (SELECT count(*) FROM cyc_expected) AS second_pass_added;

-- ── Rule without GRAPH: rows differ by graph, not only by (s, o) ──────────────
-- The derived triple takes the graph of the first edge of its path.  S->Y (c2),
-- Y->S (c3), S->O (c1) derive S->O in c2: the old CYCLE clause, which compared
-- only (s, o), dropped it as a repeat of S->O (c1).

SELECT pg_ripple.load_rules(
    '?x <https://cyc.test/ng> ?z :- ?x <https://cyc.test/ng> ?y , ?y <https://cyc.test/ng> ?z .',
    'cyc_ng'
) AS ng_rules_loaded;
SELECT pg_ripple.infer('cyc_ng') >= 0 AS ng_inferred;

CREATE TEMP TABLE cyc_ng_expected AS
SELECT s, o, g FROM cyc_edge WHERE p = 'ng'
UNION
SELECT e.s, c.o, e.g FROM cyc_edge e JOIN cyc_closure c ON c.p = 'ng' AND c.s = e.o WHERE e.p = 'ng';
CREATE TEMP TABLE cyc_ng_got AS
SELECT substring(t.r->>'s' FROM '^<https://cyc[.]test/(.*)>$') AS s,
       substring(t.r->>'o' FROM '^<https://cyc[.]test/(.*)>$') AS o,
       substring(t.r->>'g' FROM '^<https://cyc[.]test/g/(.*)>$') AS g
FROM pg_ripple.sparql('SELECT ?g ?s ?o WHERE { GRAPH ?g { ?s <https://cyc.test/ng> ?o } }') AS t(r);

SELECT s, o, g FROM cyc_ng_got ORDER BY g, s, o;
SELECT (SELECT count(*) FROM (SELECT * FROM cyc_ng_expected EXCEPT SELECT * FROM cyc_ng_got) x) AS missing,
       (SELECT count(*) FROM (SELECT * FROM cyc_ng_got EXCEPT SELECT * FROM cyc_ng_expected) x) AS unexpected;

-- ── Cleanup ───────────────────────────────────────────────────────────────────

RESET temp_file_limit;
RESET statement_timeout;
RESET pg_ripple.datalog_max_depth;
RESET pg_ripple.rule_graph_scope;
SELECT bool_and(pg_ripple.drop_rules(r) >= 0) AS rules_cleaned
FROM unnest(ARRAY['cyc_prod', 'cyc_prodp', 'cyc_plain', 'cyc_depth3', 'cyc_depth30', 'cyc_ng']) AS r;
SELECT bool_and(pg_ripple.drop_graph('https://cyc.test/g/' || g) >= 0) AS graphs_cleaned
FROM unnest(ARRAY['a', 'b', 'c1', 'c2', 'c3']) AS g;
