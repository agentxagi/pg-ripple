-- Migration 0.130.0 → 0.131.0: stable rule identity for derivations (VAL-208).
--
-- `_pg_ripple.derivations.rule_name` used to store the full Datalog rule text:
-- inflated rows, aggregations forced to parse text, and a UNIQUE
-- (derived_sid, rule_name) that silently duplicated the same fact whenever a
-- rule was edited (new text → new "name" → second row).
--
--   RULE-NAME: rules gain a stable `name` — the explicit `@name("label")`
--     annotation (new parser syntax, PT0302 on malformed/duplicate labels) or
--     `auto:<first 12 hex of md5(rule_text)>`.  derivations store that name;
--     justify() proof trees and runtime conflict reports resolve the current
--     rule text through `_pg_ripple.rules` (`rule` keeps the text, the new
--     `rule_name` field carries the identity).
--   RULE-UNIQUE: UNIQUE index on `rules (rule_set, name)`.  Identical
--     duplicated rules collapse to one catalog row (ON CONFLICT DO NOTHING);
--     re-adding a rule under an existing name via add_rule() refreshes its
--     definition.
--   STALE-PRUNE: store_rules() prunes derivations of rules that no longer
--     exist in the set (removed, or edited — an edit is a new identity), so a
--     fact re-derived by the current rules never accumulates rows under
--     vanished names.  Pruned-and-not-rederived facts honestly read as
--     "derived without a recorded derivation".
--
-- Legacy rows are converted in place: derivations whose stored text still
-- matches a current rule of the same set take that rule's name; the remaining
-- texts (rule since removed/edited) map to the same auto fingerprint the new
-- code would compute, preserving the UNIQUE constraint's dedup semantics.

-- ═══════════════════════════════════════════════════════════════════════════════
-- RULE-NAME-1 — rules.name column
-- ═══════════════════════════════════════════════════════════════════════════════

ALTER TABLE _pg_ripple.rules
    ADD COLUMN IF NOT EXISTS name TEXT;

-- ═══════════════════════════════════════════════════════════════════════════════
-- RULE-NAME-2 — backfill: every existing rule gets the auto fingerprint
-- (pre-0.131 rule texts carry no @name annotation — the syntax is new)
-- ═══════════════════════════════════════════════════════════════════════════════

UPDATE _pg_ripple.rules
   SET name = 'auto:' || left(md5(rule_text), 12)
 WHERE name IS NULL;

-- ═══════════════════════════════════════════════════════════════════════════════
-- RULE-UNIQUE-1 — collapse duplicate identities (identical texts), keep the
-- lowest id; store_rules() re-inserts the full set on every load anyway
-- ═══════════════════════════════════════════════════════════════════════════════

DELETE FROM _pg_ripple.rules r
  USING _pg_ripple.rules r2
 WHERE r.rule_set = r2.rule_set
   AND r.name = r2.name
   AND r.id > r2.id;

-- ═══════════════════════════════════════════════════════════════════════════════
-- RULE-UNIQUE-2 — uniqueness of (rule_set, name)
-- ═══════════════════════════════════════════════════════════════════════════════

CREATE UNIQUE INDEX IF NOT EXISTS uq_rules_set_name
    ON _pg_ripple.rules (rule_set, name);

-- ═══════════════════════════════════════════════════════════════════════════════
-- RULE-NAME-3 — convert legacy derivations.rule_name values (rule text → name)
--
-- Step 1: texts that still match a current rule of the same set adopt that
--         rule's name (explicit or auto — future-proof if the set is ever
--         re-loaded with @name annotations).
-- Step 2: remaining texts map to the auto fingerprint of the text itself —
--         the same value the new code computes for that text, so rows keep
--         deduplicating under the UNIQUE (derived_sid, rule_name) constraint.
-- ═══════════════════════════════════════════════════════════════════════════════

UPDATE _pg_ripple.derivations d
   SET rule_name = r.name
  FROM _pg_ripple.rules r
 WHERE r.rule_set = d.rule_set
   AND r.rule_text = d.rule_name
   AND d.rule_name <> r.name;

UPDATE _pg_ripple.derivations
   SET rule_name = 'auto:' || left(md5(rule_name), 12)
 WHERE rule_name NOT LIKE 'auto:%'
   AND rule_name NOT IN (SELECT name FROM _pg_ripple.rules);

-- ═══════════════════════════════════════════════════════════════════════════════
-- RULE-NAME-4 — catalog comments
-- ═══════════════════════════════════════════════════════════════════════════════

COMMENT ON COLUMN _pg_ripple.derivations.rule_name IS
    'Stable rule identity: explicit @name("label") or auto:<md5-12 of rule_text> (VAL-208). '
    'Not the rule text; resolve the current text via _pg_ripple.rules (rule_set, name).';
COMMENT ON COLUMN _pg_ripple.rules.name IS
    'Stable rule identity used by derivations.rule_name: explicit @name("label") or '
    'auto:<first 12 hex of md5(rule_text)>. Unique per rule set (VAL-208).';
