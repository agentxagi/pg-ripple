-- pg_regress test: a SPARQL query never writes the dictionary for a literal (FB-0278)
--
-- Production defect (ValorBrain, 25/10 onwards): an ASK with a literal object,
--     ASK { GRAPH <g> { <s> <p> "literal" } }
-- run by the app role (SELECT only on _pg_ripple) failed with
--     ERROR: permission denied for table dictionary
-- about 40 times a day.  The translator encoded the literal with
-- dictionary::encode, whose cache miss runs INSERT ... ON CONFLICT DO NOTHING:
-- the INSERT privilege is checked even when the literal is already stored.
-- IRIs were already looked up read-only (a never-stored IRI matches nothing).
--
-- The fix looks literals up the same way in triple patterns: plain,
-- language-tagged and typed (inline encoding first), and a literal that was
-- never stored matches no triple.  This test pins it: stored literals still
-- match, unknown ones answer false, the dictionary does not grow, and a
-- read-only role gets an answer instead of the error.

-- Deterministic library load (v0145 pattern).
SET client_min_messages TO error;
CREATE EXTENSION IF NOT EXISTS pg_ripple;
SELECT pg_ripple.triple_count() >= 0 AS library_loaded;
SET search_path TO pg_ripple, public;

SELECT pg_ripple.load_ntriples_into_graph(
  E'<http://fb0278.test/s> <http://fb0278.test/plain> "valor gravado" .\n' ||
  E'<http://fb0278.test/s> <http://fb0278.test/lang> "ola"@pt .\n' ||
  E'<http://fb0278.test/s> <http://fb0278.test/typed> "3.5"^^<http://www.w3.org/2001/XMLSchema#decimal> .\n' ||
  E'<http://fb0278.test/s> <http://fb0278.test/int> "42"^^<http://www.w3.org/2001/XMLSchema#integer> .\n',
  'http://fb0278.test/g'
) AS triples_loaded;

-- Stored literals still match, each kind on its own path.
SELECT pg_ripple.sparql_ask($$ASK { GRAPH <http://fb0278.test/g> { <http://fb0278.test/s> <http://fb0278.test/plain> "valor gravado" } }$$) AS plain_known;
SELECT pg_ripple.sparql_ask($$ASK { GRAPH <http://fb0278.test/g> { <http://fb0278.test/s> <http://fb0278.test/lang> "ola"@pt } }$$) AS lang_known;
SELECT pg_ripple.sparql_ask($$ASK { GRAPH <http://fb0278.test/g> { <http://fb0278.test/s> <http://fb0278.test/typed> "3.5"^^<http://www.w3.org/2001/XMLSchema#decimal> } }$$) AS typed_known;
SELECT pg_ripple.sparql_ask($$ASK { GRAPH <http://fb0278.test/g> { <http://fb0278.test/s> <http://fb0278.test/int> 42 } }$$) AS inline_known;

-- Unknown literals answer false and leave the dictionary as it was.
CREATE TEMP TABLE fb0278_dict_before AS SELECT count(*) AS n FROM _pg_ripple.dictionary;
SELECT pg_ripple.sparql_ask($$ASK { GRAPH <http://fb0278.test/g> { <http://fb0278.test/s> <http://fb0278.test/plain> "valor que nunca foi gravado" } }$$) AS plain_unknown;
SELECT pg_ripple.sparql_ask($$ASK { GRAPH <http://fb0278.test/g> { <http://fb0278.test/s> <http://fb0278.test/lang> "ola"@en } }$$) AS lang_unknown;
SELECT pg_ripple.sparql_ask($$ASK { GRAPH <http://fb0278.test/g> { <http://fb0278.test/s> <http://fb0278.test/typed> "7.25"^^<http://www.w3.org/2001/XMLSchema#decimal> } }$$) AS typed_unknown;
SELECT pg_ripple.sparql_ask($$ASK { GRAPH <http://fb0278.test/g> { <http://fb0278.test/s> <http://fb0278.test/int> 43 } }$$) AS inline_unknown;
SELECT (SELECT count(*) FROM _pg_ripple.dictionary) - n AS dictionary_growth FROM fb0278_dict_before;

-- The production case: a role that only reads _pg_ripple asks for a literal
-- it has never seen.  Before the fix: ERROR permission denied for table dictionary.
-- (Only the unknown literal: what the reader sees of stored triples depends on
-- graph access state left by earlier tests in the shared regress database.)
CREATE ROLE fb0278_reader NOLOGIN;
GRANT USAGE ON SCHEMA pg_ripple, _pg_ripple TO fb0278_reader;
GRANT SELECT ON ALL TABLES IN SCHEMA _pg_ripple TO fb0278_reader;
SET ROLE fb0278_reader;
SELECT pg_ripple.sparql_ask($$ASK { GRAPH <http://fb0278.test/g> { <http://fb0278.test/s> <http://fb0278.test/plain> "outro valor nunca gravado" } }$$) AS reader_unknown;
RESET ROLE;
REVOKE SELECT ON ALL TABLES IN SCHEMA _pg_ripple FROM fb0278_reader;
REVOKE USAGE ON SCHEMA pg_ripple, _pg_ripple FROM fb0278_reader;
DROP ROLE fb0278_reader;
