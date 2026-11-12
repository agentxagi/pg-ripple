-- pg_regress test: an inference pass does not copy into the head's HTAP delta
-- the rows the head's view already returns (v0147h).
--
-- Regression for: the plain path (infer, rows with source = 1) and the
-- semi-naive materialisation (infer_with_stats) insert into vp_{id}_delta with
-- ON CONFLICT DO NOTHING, which only sees the delta.  After a merge moved the
-- derived rows to main, every pass copied them back: 24,293 rows per pass on one
-- production predicate with an empty delta, one merge cycle per pass.  The
-- passes now skip what the view vp_{id} returns (main minus tombstones, plus
-- delta), so:
--   * a pass over merged rows writes nothing to the delta;
--   * a deleted (tombstoned) but still derivable row is rederived into the
--     delta and survives the next merge.
--
-- Data: a chain n0 -> n1 -> ... -> n30 of link edges in one named graph.
-- vis_rs: reach = transitive closure of link (465 pairs: a non-recursive and a
-- recursive rule), back = link reversed (30 rows, non-recursive).

CREATE EXTENSION IF NOT EXISTS pg_ripple;
SELECT pg_ripple.triple_count() >= 0 AS library_loaded;
SET search_path TO pg_ripple, public;

SELECT pg_ripple.drop_rules('vis_rs') >= 0 AS rules_reset;
SELECT pg_ripple.drop_graph('https://vis.test/g') >= 0 AS graph_reset;

SELECT pg_ripple.load_nquads(string_agg(
         format('<https://vis.test/n%s> <https://vis.test/link> <https://vis.test/n%s> <https://vis.test/g> .',
                i, i + 1),
         E'\n')) = 30 AS edges_loaded
FROM generate_series(0, 29) AS i;

SELECT pg_ripple.load_rules(
    'GRAPH ?g { ?x <https://vis.test/reach> ?y } :- GRAPH ?g { ?x <https://vis.test/link> ?y } .
     GRAPH ?g { ?x <https://vis.test/reach> ?z } :-
       GRAPH ?g { ?x <https://vis.test/link> ?y },
       GRAPH ?g { ?y <https://vis.test/reach> ?z } .
     GRAPH ?g { ?y <https://vis.test/back> ?x } :- GRAPH ?g { ?x <https://vis.test/link> ?y } .',
    'vis_rs'
) AS rules_loaded;

-- Row counts of a head's view, main, delta and tombstones.
CREATE FUNCTION pg_temp.vis_rows(head text, part text) RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE
  pid bigint;
  n bigint;
BEGIN
  SELECT d.id INTO STRICT pid FROM _pg_ripple.dictionary d WHERE d.value = 'https://vis.test/' || head;
  EXECUTE format('SELECT count(*) FROM _pg_ripple.%I', 'vp_' || pid || part) INTO n;
  RETURN n;
END $$;
CREATE FUNCTION pg_temp.vis_has(head text, s text, o text) RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE
  pid bigint;
  sid bigint;
  oid_ bigint;
  n bigint;
BEGIN
  SELECT d.id INTO STRICT pid FROM _pg_ripple.dictionary d WHERE d.value = 'https://vis.test/' || head;
  SELECT d.id INTO STRICT sid FROM _pg_ripple.dictionary d WHERE d.value = 'https://vis.test/' || s;
  SELECT d.id INTO STRICT oid_ FROM _pg_ripple.dictionary d WHERE d.value = 'https://vis.test/' || o;
  EXECUTE format('SELECT count(*) FROM _pg_ripple.%I WHERE s = $1 AND o = $2', 'vp_' || pid)
    INTO n USING sid, oid_;
  RETURN n;
END $$;
CREATE TEMP VIEW vis_state AS
SELECT h AS head,
       pg_temp.vis_rows(h, '') AS visible,
       pg_temp.vis_rows(h, '_main') AS main,
       pg_temp.vis_rows(h, '_delta') AS delta,
       pg_temp.vis_rows(h, '_tombstones') AS tombstones
FROM unnest(ARRAY['back', 'reach']) AS h;

-- ── Plain path ────────────────────────────────────────────────────────────────

-- First pass: every derived row goes to the delta.  (main and tombstones are left
-- out here: a previous run in the same database leaves tombstoned rows in main,
-- which compact() only applies once the delta has rows.)
SELECT pg_ripple.infer('vis_rs') >= 0 AS plain_first_pass;
SELECT head, visible, delta FROM vis_state ORDER BY head;

-- After a merge, a pass over the merged rows writes nothing to the delta (the
-- delta's ON CONFLICT alone copied all 495 rows back).
SELECT pg_ripple.compact() >= 0 AS merged;
SELECT * FROM vis_state ORDER BY head;
SELECT pg_ripple.infer('vis_rs') >= 0 AS plain_pass_after_merge;
SELECT * FROM vis_state ORDER BY head;

-- ── Semi-naive path ───────────────────────────────────────────────────────────

-- The same merged rows: nothing derived, nothing copied.
SELECT (pg_ripple.infer_with_stats('vis_rs')->>'derived')::bigint AS seminaive_derived_after_merge;
SELECT * FROM vis_state ORDER BY head;

-- ── Deleted but still derivable ───────────────────────────────────────────────

-- The tombstone hides the merged row from the view, so the next pass rederives
-- it into the delta, and the rederived row survives the merge.
SELECT pg_ripple.delete_triple_from_graph('<https://vis.test/n0>', '<https://vis.test/reach>',
                                          '<https://vis.test/n5>', 'https://vis.test/g') AS plain_deleted;
SELECT * FROM vis_state WHERE head = 'reach';
SELECT pg_ripple.infer('vis_rs') >= 0 AS plain_pass_after_delete;
SELECT * FROM vis_state WHERE head = 'reach';
SELECT pg_ripple.compact() >= 0 AS merged_after_plain;
SELECT * FROM vis_state WHERE head = 'reach';
SELECT pg_temp.vis_has('reach', 'n0', 'n5') AS n0_reaches_n5;

SELECT pg_ripple.delete_triple_from_graph('<https://vis.test/n1>', '<https://vis.test/reach>',
                                          '<https://vis.test/n9>', 'https://vis.test/g') AS seminaive_deleted;
SELECT (pg_ripple.infer_with_stats('vis_rs')->>'derived')::bigint AS seminaive_rederived;
SELECT * FROM vis_state WHERE head = 'reach';
SELECT pg_ripple.compact() >= 0 AS merged_after_seminaive;
SELECT * FROM vis_state WHERE head = 'reach';
SELECT pg_temp.vis_has('reach', 'n1', 'n9') AS n1_reaches_n9;

SELECT pg_ripple.drop_rules('vis_rs') >= 0 AS rules_cleaned;
SELECT pg_ripple.drop_graph('https://vis.test/g') >= 0 AS graph_cleaned;
