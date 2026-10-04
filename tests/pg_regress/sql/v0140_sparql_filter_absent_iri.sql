-- pg_regress test: v0.140.0 — FILTER against a dictionary-absent named node
-- (VAL-358, port of upstream 13aad42c "Fix W3C SPARQL conformance failures")
--
-- Regression target: the ValorBrain engine read path (outgoing/incoming
-- kg_query in src/kg-ripple.ts) filters predicate bindings with
-- `FILTER(?p != <iri>)`.  When the constant IRI is NOT in the dictionary
-- (fresh database, fresh tenant graph, purged dictionary), 0.131.0
-- translated the comparison to `?p != (scalar subquery returning NULL)`,
-- which is NULL for every row, so the filter silently dropped EVERY row.
-- W3C semantics: a bound ?p is != any term it is not identical to, so all
-- rows must survive.

CREATE EXTENSION IF NOT EXISTS pg_ripple;
SELECT pg_ripple.triple_count() >= 0 AS library_loaded;
SET search_path TO pg_ripple, public;

-- Seed: two triples whose predicates ARE in the dictionary.  The filtered
-- IRI <https://val358.test/absent> is intentionally never inserted.
SELECT pg_ripple.insert_triple(
    'https://val358.test/e1', 'https://val358.test/p1', '"o1"',
    'https://val358.test/g1'
) IS NOT NULL AS t01_seed_s1;
SELECT pg_ripple.insert_triple(
    'https://val358.test/e1', 'https://val358.test/p2', '"o2"',
    'https://val358.test/g1'
) IS NOT NULL AS t02_seed_s2;
-- One triple with e1 as OBJECT, for the incoming-neighbors shape.
SELECT pg_ripple.insert_triple(
    'https://val358.test/e0', 'https://val358.test/p0', 'https://val358.test/e1',
    'https://val358.test/g1'
) IS NOT NULL AS t03_seed_incoming;

-- Guard: the filtered IRI must indeed be absent from the dictionary.
SELECT (SELECT COUNT(*) FROM _pg_ripple.dictionary
         WHERE value = 'https://val358.test/absent') = 0 AS t04_iri_absent;

-- Ground truth: both predicate rows visible without the filter.
SELECT (SELECT COUNT(*) FROM pg_ripple.sparql($$
    SELECT ?p ?o WHERE { GRAPH <https://val358.test/g1> {
        <https://val358.test/e1> ?p ?o . } }
$$)) = 2 AS t05_no_filter_two_rows;

-- REGRESSION (was 0 rows on 0.131.0): != against a dictionary-absent IRI
-- must keep every row (workload shape of kg-ripple.ts outgoing neighbors,
-- including the STRSTARTS filter the engine also sends).
SELECT (SELECT COUNT(*) FROM pg_ripple.sparql($$
    SELECT ?p ?o WHERE {
        GRAPH <https://val358.test/g1> {
            <https://val358.test/e1> ?p ?o .
            FILTER(!STRSTARTS(STR(?p), "https://val358.test/confidence"))
            FILTER(?p != <https://val358.test/absent>)
        }
    }
$$)) = 2 AS t06_absent_iri_ne_keeps_rows;

-- Control: != against a dictionary-resident IRI still excludes it.
SELECT (SELECT COUNT(*) FROM pg_ripple.sparql($$
    SELECT ?p ?o WHERE {
        GRAPH <https://val358.test/g1> {
            <https://val358.test/e1> ?p ?o .
            FILTER(?p != <https://val358.test/p1>)
        }
    }
$$)) = 1 AS t07_present_iri_ne_excludes_row;

-- Equality against a dictionary-absent IRI matches nothing (and must not
-- error or match everything after the upsert).
SELECT (SELECT COUNT(*) FROM pg_ripple.sparql($$
    SELECT ?p ?o WHERE {
        GRAPH <https://val358.test/g1> {
            <https://val358.test/e1> ?p ?o .
            FILTER(?p = <https://val358.test/absent2>)
        }
    }
$$)) = 0 AS t08_absent_iri_eq_matches_nothing;

-- Incoming workload shape (kg-ripple.ts incoming neighbors): one triple has
-- e1 as object; the absent-IRI filter must not drop it.
SELECT (SELECT COUNT(*) FROM pg_ripple.sparql($$
    SELECT ?s ?p WHERE {
        GRAPH <https://val358.test/g1> {
            ?s ?p <https://val358.test/e1> .
            FILTER(?p != <https://val358.test/absent3>)
        }
    }
$$)) = 1 AS t09_incoming_absent_iri_keeps_rows;

-- AGG-NUMTYPE: typed numeric aggregates go through
-- pg_ripple.numeric_type_code_spi (new in 0.140.0).
SELECT pg_ripple.insert_triple(
    'https://val358.test/n1', 'https://val358.test/value', '"5"^^<http://www.w3.org/2001/XMLSchema#integer>',
    'https://val358.test/g1'
) IS NOT NULL AS t10_seed_int;
SELECT pg_ripple.insert_triple(
    'https://val358.test/n2', 'https://val358.test/value', '"7"^^<http://www.w3.org/2001/XMLSchema#integer>',
    'https://val358.test/g1'
) IS NOT NULL AS t11_seed_int2;
SELECT (SELECT COUNT(*) FROM pg_ripple.sparql($$
    SELECT (SUM(?v) AS ?total) WHERE {
        GRAPH <https://val358.test/g1> { ?s <https://val358.test/value> ?v . }
    }
$$)) = 1 AS t12_sum_returns_one_group;
SELECT (pg_ripple.sparql($$
    SELECT (SUM(?v) AS ?total) WHERE {
        GRAPH <https://val358.test/g1> { ?s <https://val358.test/value> ?v . }
    }
$$) ->> 'total') = '"12"^^<http://www.w3.org/2001/XMLSchema#integer>' AS t13_sum_value_12;

-- SHA1-NATIVE: SHA1() now resolves without pgcrypto via _pg_ripple.sha1_hex.
SELECT (pg_ripple.sparql($$
    SELECT (SHA1("abc") AS ?h) WHERE { GRAPH <https://val358.test/g1> { ?s ?p ?o . } } LIMIT 1
$$) ->> 'h') = '"a9993e364706816aba3e25717850c26c9cd0d89d"' AS t14_sha1_known_digest;
