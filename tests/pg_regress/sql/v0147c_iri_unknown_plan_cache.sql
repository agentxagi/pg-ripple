-- pg_regress test: an IRI not stored yet does not freeze FALSE in the plan cache
--
-- The translator looks IRIs up read-only: a subject, object, predicate or
-- graph that has never been stored matches nothing, and its pattern becomes
-- FALSE in the generated SQL.  That is the right answer when the query runs,
-- but pg_ripple.sparql() caches the SQL per backend by query text: once the
-- IRI is stored, the same query on the same connection kept answering from
-- the cached FALSE.  ValorBrain's one-hop query, `SELECT ?p ?o WHERE { GRAPH
-- <g> { <entity> ?p ?o } }`, runs on pooled connections that live for hours:
-- an entity asked about before it was synced stayed without neighbours.
-- FB-0278 (#29) made unknown literals a lookup that runs with the query; this
-- test pins the IRI side: a translation that saw an unknown IRI is not cached.

-- Deterministic library load (v0145 pattern).
SET client_min_messages TO error;
CREATE EXTENSION IF NOT EXISTS pg_ripple;
SELECT pg_ripple.triple_count() >= 0 AS library_loaded;
SET search_path TO pg_ripple, public;

SELECT pg_ripple.load_ntriples_into_graph(
  E'<http://v0147c.test/s> <http://v0147c.test/p> <http://v0147c.test/o> .\n',
  'http://v0147c.test/g'
) AS triples_loaded;

-- A fresh connection, as a pooled backend that has not seen these IRIs.
\c
SET client_min_messages TO error;
SET search_path TO pg_ripple, public;

-- Each query runs twice before the load, so its SQL is in the plan cache,
-- and once after.  Unknown subject, object, predicate and graph, in turn.
SELECT count(*) AS subject_before FROM pg_ripple.sparql($$SELECT ?o WHERE { GRAPH <http://v0147c.test/g> { <http://v0147c.test/s-late> <http://v0147c.test/p> ?o } }$$);
SELECT count(*) AS subject_before_cached FROM pg_ripple.sparql($$SELECT ?o WHERE { GRAPH <http://v0147c.test/g> { <http://v0147c.test/s-late> <http://v0147c.test/p> ?o } }$$);
SELECT count(*) AS object_before FROM pg_ripple.sparql($$SELECT ?s WHERE { GRAPH <http://v0147c.test/g> { ?s <http://v0147c.test/p> <http://v0147c.test/o-late> } }$$);
SELECT count(*) AS object_before_cached FROM pg_ripple.sparql($$SELECT ?s WHERE { GRAPH <http://v0147c.test/g> { ?s <http://v0147c.test/p> <http://v0147c.test/o-late> } }$$);
SELECT count(*) AS predicate_before FROM pg_ripple.sparql($$SELECT ?s WHERE { GRAPH <http://v0147c.test/g> { ?s <http://v0147c.test/p-late> ?o } }$$);
SELECT count(*) AS predicate_before_cached FROM pg_ripple.sparql($$SELECT ?s WHERE { GRAPH <http://v0147c.test/g> { ?s <http://v0147c.test/p-late> ?o } }$$);
SELECT count(*) AS graph_before FROM pg_ripple.sparql($$SELECT ?s WHERE { GRAPH <http://v0147c.test/g-late> { ?s <http://v0147c.test/p> ?o } }$$);
SELECT count(*) AS graph_before_cached FROM pg_ripple.sparql($$SELECT ?s WHERE { GRAPH <http://v0147c.test/g-late> { ?s <http://v0147c.test/p> ?o } }$$);
-- ASK through sparql() is translated each time (no plan cache): a control.
SELECT result::text AS ask_before FROM pg_ripple.sparql($$ASK WHERE { GRAPH <http://v0147c.test/g> { <http://v0147c.test/s-late> <http://v0147c.test/p> <http://v0147c.test/o> . } }$$);
SELECT result::text AS ask_before_cached FROM pg_ripple.sparql($$ASK WHERE { GRAPH <http://v0147c.test/g> { <http://v0147c.test/s-late> <http://v0147c.test/p> <http://v0147c.test/o> . } }$$);

SELECT pg_ripple.load_ntriples_into_graph(
  E'<http://v0147c.test/s-late> <http://v0147c.test/p> <http://v0147c.test/o> .\n' ||
  E'<http://v0147c.test/s> <http://v0147c.test/p> <http://v0147c.test/o-late> .\n' ||
  E'<http://v0147c.test/s> <http://v0147c.test/p-late> <http://v0147c.test/o> .\n',
  'http://v0147c.test/g'
) AS late_load;
SELECT pg_ripple.load_ntriples_into_graph(
  E'<http://v0147c.test/s> <http://v0147c.test/p> <http://v0147c.test/o> .\n',
  'http://v0147c.test/g-late'
) AS late_graph_load;

-- Same connection, same query text: the stored IRIs now match.
SELECT count(*) AS subject_after FROM pg_ripple.sparql($$SELECT ?o WHERE { GRAPH <http://v0147c.test/g> { <http://v0147c.test/s-late> <http://v0147c.test/p> ?o } }$$);
SELECT count(*) AS object_after FROM pg_ripple.sparql($$SELECT ?s WHERE { GRAPH <http://v0147c.test/g> { ?s <http://v0147c.test/p> <http://v0147c.test/o-late> } }$$);
SELECT count(*) AS predicate_after FROM pg_ripple.sparql($$SELECT ?s WHERE { GRAPH <http://v0147c.test/g> { ?s <http://v0147c.test/p-late> ?o } }$$);
SELECT count(*) AS graph_after FROM pg_ripple.sparql($$SELECT ?s WHERE { GRAPH <http://v0147c.test/g-late> { ?s <http://v0147c.test/p> ?o } }$$);
SELECT result::text AS ask_after FROM pg_ripple.sparql($$ASK WHERE { GRAPH <http://v0147c.test/g> { <http://v0147c.test/s-late> <http://v0147c.test/p> <http://v0147c.test/o> . } }$$);

-- A query whose IRIs are all stored is still cached: a second run is a hit.
SELECT pg_ripple.plan_cache_reset();
SELECT count(*) AS known_first FROM pg_ripple.sparql($$SELECT ?o WHERE { GRAPH <http://v0147c.test/g> { <http://v0147c.test/s> <http://v0147c.test/p> ?o } }$$);
SELECT count(*) AS known_second FROM pg_ripple.sparql($$SELECT ?o WHERE { GRAPH <http://v0147c.test/g> { <http://v0147c.test/s> <http://v0147c.test/p> ?o } }$$);
SELECT hits >= 1 AS known_query_cached FROM pg_ripple.plan_cache_stats();

-- Cleanup.
SELECT pg_ripple.drop_graph('http://v0147c.test/g') >= 0 AS g_dropped;
SELECT pg_ripple.drop_graph('http://v0147c.test/g-late') >= 0 AS g_late_dropped;
