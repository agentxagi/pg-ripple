-- pg_regress test: the subject/object pattern rebuild of a merge cycle leaves an
-- unchanged row alone (v0147g).
--
-- Regression for: rebuild_subject_patterns / rebuild_object_patterns upserted every
-- subject and object with DO UPDATE SET pattern = EXCLUDED.pattern, so each merge
-- cycle rewrote the whole tables even when nothing changed (47 MB and 95 MB of WAL
-- per cycle in production).  compact() runs the same rebuilds as a merge cycle;
-- a row's xmin shows whether it was rewritten.

CREATE EXTENSION IF NOT EXISTS pg_ripple;
SELECT pg_ripple.triple_count() >= 0 AS library_loaded;
SET search_path TO pg_ripple, public;

SELECT pg_ripple.drop_rules('pat_rs') >= 0 AS rules_reset;
SELECT pg_ripple.drop_graph('https://pat.test/g') >= 0 AS graph_reset;

-- Two derived predicates get dedicated VP tables through the plain inference path,
-- so the rebuild has tables of its own to scan.
SELECT pg_ripple.load_nquads(string_agg(
         format('<https://pat.test/s%s> <https://pat.test/p> <https://pat.test/o%s> <https://pat.test/g> .', i, i % 7),
         E'\n')) AS loaded
FROM generate_series(1, 30) AS i;
SELECT pg_ripple.load_rules(
    'GRAPH ?g { ?x <https://pat.test/q> ?y } :- GRAPH ?g { ?x <https://pat.test/p> ?y } .
     GRAPH ?g { ?x <https://pat.test/r> ?y } :- GRAPH ?g { ?x <https://pat.test/p> ?y } .',
    'pat_rs'
) AS rules_loaded;
SET pg_ripple.rule_graph_scope = 'all';
SELECT pg_ripple.infer('pat_rs') >= 0 AS inferred;

CREATE TEMP VIEW pat_rows AS
SELECT 's' AS kind, s AS id, pattern, xmin::text AS x FROM _pg_ripple.subject_patterns
UNION ALL
SELECT 'o', o, pattern, xmin::text FROM _pg_ripple.object_patterns;
CREATE TEMP VIEW pat_ids AS
SELECT d.value AS iri, d.id FROM _pg_ripple.dictionary d WHERE d.value LIKE 'https://pat.test/%';

SELECT pg_ripple.compact() >= 0 AS first_compact;
SELECT count(*) AS subject_rows FROM pat_rows r JOIN pat_ids i ON i.id = r.id WHERE r.kind = 's';
SELECT count(*) AS object_rows FROM pat_rows r JOIN pat_ids i ON i.id = r.id WHERE r.kind = 'o';
CREATE TEMP TABLE pat_before AS SELECT * FROM pat_rows;

-- Nothing changed: the second rebuild rewrites no row.
SELECT pg_ripple.compact() >= 0 AS second_compact;
SELECT count(*) AS rewritten
FROM pat_rows a JOIN pat_before b USING (kind, id)
WHERE a.x <> b.x;

-- A new subject is inserted and a changed pattern is updated; the rest stays put.
SELECT pg_ripple.insert_triple('<https://pat.test/s100>', '<https://pat.test/q>', '<https://pat.test/z>',
                               '<https://pat.test/g>') IS NOT NULL AS s100_q;
SELECT pg_ripple.compact() >= 0 AS third_compact;
SELECT count(*) AS rewritten FROM pat_rows a JOIN pat_before b USING (kind, id) WHERE a.x <> b.x;
DELETE FROM pat_before;
INSERT INTO pat_before SELECT * FROM pat_rows;
SELECT pg_ripple.insert_triple('<https://pat.test/s100>', '<https://pat.test/r>', '<https://pat.test/z>',
                               '<https://pat.test/g>') IS NOT NULL AS s100_r;
SELECT pg_ripple.compact() >= 0 AS fourth_compact;
SELECT i.iri, cardinality(a.pattern) AS predicates
FROM pat_rows a JOIN pat_before b USING (kind, id) JOIN pat_ids i ON i.id = a.id
WHERE a.x <> b.x
ORDER BY 1;

RESET pg_ripple.rule_graph_scope;
SELECT pg_ripple.drop_rules('pat_rs') >= 0 AS rules_cleaned;
SELECT pg_ripple.drop_graph('https://pat.test/g') >= 0 AS graph_cleaned;
