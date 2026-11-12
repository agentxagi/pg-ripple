-- pg_regress test: SPARQL p+ and p* on a cyclic graph return the closure as a
-- set (v0147f).
--
-- Regression for: the property-path CTEs used UNION ALL with
-- CYCLE s, o SET _is_cycle, which keeps one row per simple path.  On a ring of
-- 40 nodes with edges i->i+1 and i->i+2, <n0> <dep>+ ?o exhausted 512 MB of
-- temp files in 3.3 s; the answer is 40 nodes.  Expected counts come from a
-- closure computed here without the extension.

CREATE EXTENSION IF NOT EXISTS pg_ripple;
SELECT pg_ripple.triple_count() >= 0 AS library_loaded;
SET search_path TO pg_ripple, public;

SELECT pg_ripple.drop_graph('https://pp.test/g') >= 0 AS graph_reset;

CREATE TEMP TABLE pp_edge (s text, o text);
INSERT INTO pp_edge
SELECT 'n' || i, 'n' || ((i + d) % 40) FROM generate_series(0, 39) AS i, (VALUES (1), (2)) AS v(d);
SELECT pg_ripple.load_nquads(string_agg(
         format('<https://pp.test/%s> <https://pp.test/dep> <https://pp.test/%s> <https://pp.test/g> .', s, o),
         E'\n')) AS edges_loaded
FROM pp_edge;

CREATE TEMP TABLE pp_closure (s text, o text, PRIMARY KEY (s, o));
INSERT INTO pp_closure SELECT s, o FROM pp_edge;
DO $$
DECLARE
    added bigint;
BEGIN
    LOOP
        INSERT INTO pp_closure
        SELECT DISTINCT a.s, b.o FROM pp_closure a JOIN pp_closure b ON b.s = a.o
        ON CONFLICT DO NOTHING;
        GET DIAGNOSTICS added = ROW_COUNT;
        EXIT WHEN added = 0;
    END LOOP;
END $$;

SET statement_timeout = '30s';
SET temp_file_limit = '256MB';

-- Default max_path_depth (100).
SELECT count(*) AS plus_from_n0,
       (SELECT count(*) FROM pp_closure WHERE s = 'n0') AS expected
FROM pg_ripple.sparql('SELECT ?o WHERE { GRAPH <https://pp.test/g> { <https://pp.test/n0> <https://pp.test/dep>+ ?o } }');
SELECT count(*) AS plus_to_n5,
       (SELECT count(*) FROM pp_closure WHERE o = 'n5') AS expected
FROM pg_ripple.sparql('SELECT ?s WHERE { GRAPH <https://pp.test/g> { ?s <https://pp.test/dep>+ <https://pp.test/n5> } }');
SELECT count(*) AS plus_all_pairs,
       (SELECT count(*) FROM pp_closure) AS expected
FROM pg_ripple.sparql('SELECT ?s ?o WHERE { GRAPH <https://pp.test/g> { ?s <https://pp.test/dep>+ ?o } }');
SELECT count(*) AS star_from_n0,
       (SELECT count(*) FROM (SELECT o FROM pp_closure WHERE s = 'n0' UNION SELECT 'n0') x) AS expected
FROM pg_ripple.sparql('SELECT ?o WHERE { GRAPH <https://pp.test/g> { <https://pp.test/n0> <https://pp.test/dep>* ?o } }');
SELECT count(*) AS plus_graph_var,
       (SELECT count(*) FROM pp_closure WHERE s = 'n0') AS expected
FROM pg_ripple.sparql('SELECT ?g ?o WHERE { GRAPH ?g { <https://pp.test/n0> <https://pp.test/dep>+ ?o } }');

-- Without a depth limit, and with a limit of one hop.
SET pg_ripple.max_path_depth = 0;
SELECT count(*) AS plus_unbounded,
       (SELECT count(*) FROM pp_closure WHERE s = 'n0') AS expected
FROM pg_ripple.sparql('SELECT ?o WHERE { GRAPH <https://pp.test/g> { <https://pp.test/n0> <https://pp.test/dep>+ ?o } }');
SET pg_ripple.max_path_depth = 1;
SELECT count(*) AS plus_one_hop,
       (SELECT count(*) FROM pp_edge WHERE s = 'n0') AS expected
FROM pg_ripple.sparql('SELECT ?o WHERE { GRAPH <https://pp.test/g> { <https://pp.test/n0> <https://pp.test/dep>+ ?o } }');

RESET pg_ripple.max_path_depth;

-- pg:confPath filters each edge by its confidence (none stored here: 1.0), so it
-- is the same closure.  It used UNION ALL with CYCLE too; with a bound subject it
-- failed with "missing FROM-clause entry for table edge_anchor", and on a DAG it
-- returned an object once per path.  The alternative forces the path compiler.
SELECT pg_ripple.load_nquads(
    '<https://pp.test/a> <https://pp.test/dag> <https://pp.test/b> <https://pp.test/g> .
     <https://pp.test/a> <https://pp.test/dag> <https://pp.test/c> <https://pp.test/g> .
     <https://pp.test/b> <https://pp.test/dag> <https://pp.test/d> <https://pp.test/g> .
     <https://pp.test/c> <https://pp.test/dag> <https://pp.test/d> <https://pp.test/g> .') AS dag_loaded;
SELECT count(*) AS conf_from_n0,
       (SELECT count(*) FROM pp_closure WHERE s = 'n0') AS expected
FROM pg_ripple.sparql('SELECT ?o WHERE { GRAPH <https://pp.test/g> { <https://pp.test/n0> (<http://pg-ripple.org/conf_path/https://pp.test/dep/0.5>|<https://pp.test/none>) ?o } }');
SELECT count(*) AS conf_all_pairs,
       (SELECT count(*) FROM pp_closure) AS expected
FROM pg_ripple.sparql('SELECT ?s ?o WHERE { GRAPH <https://pp.test/g> { ?s (<http://pg-ripple.org/conf_path/https://pp.test/dep/0.5>|<https://pp.test/none>) ?o } }');
SELECT count(*) AS conf_graph_var,
       (SELECT count(*) FROM pp_closure WHERE s = 'n0') AS expected
FROM pg_ripple.sparql('SELECT ?g ?o WHERE { GRAPH ?g { <https://pp.test/n0> (<http://pg-ripple.org/conf_path/https://pp.test/dep/0.5>|<https://pp.test/none>) ?o } }');
SELECT count(*) AS conf_dag_from_a, count(DISTINCT r->>'o') AS distinct_objects
FROM pg_ripple.sparql('SELECT ?o WHERE { GRAPH <https://pp.test/g> { <https://pp.test/a> (<http://pg-ripple.org/conf_path/https://pp.test/dag/0.5>|<https://pp.test/none>) ?o } }') AS t(r);
-- p -> q -> r with p -> r: three pairs; one row per path gave four.
SELECT pg_ripple.load_nquads(
    '<https://pp.test/p> <https://pp.test/tri> <https://pp.test/q> <https://pp.test/g> .
     <https://pp.test/q> <https://pp.test/tri> <https://pp.test/r> <https://pp.test/g> .
     <https://pp.test/p> <https://pp.test/tri> <https://pp.test/r> <https://pp.test/g> .') AS tri_loaded;
SELECT count(*) AS conf_tri_pairs, count(DISTINCT (r->>'s', r->>'o')) AS distinct_pairs
FROM pg_ripple.sparql('SELECT ?s ?o WHERE { GRAPH <https://pp.test/g> { ?s (<http://pg-ripple.org/conf_path/https://pp.test/tri/0.5>|<https://pp.test/none>) ?o } }') AS t(r);
RESET temp_file_limit;
RESET statement_timeout;
SELECT pg_ripple.drop_graph('https://pp.test/g') >= 0 AS graph_cleaned;
