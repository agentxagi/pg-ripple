//! Proof-tree justification infrastructure (v0.100.0 PROOF-TREE-01).
//!
//! When `pg_ripple.record_derivations = on`, the semi-naive inference engine
//! records, for every newly derived fact:
//!
//! - `derived_sid`     — the statement ID of the inferred triple
//! - `rule_name`       — the rule's stable identity (explicit `@name("label")`
//!   or an auto fingerprint of the rule text, VAL-208); display surfaces
//!   resolve the current text through `_pg_ripple.rules`
//! - `rule_set`        — the rule set name
//! - `antecedent_sids` — SIDs of the body-atom triples that fired the rule
//!
//! The public `justify()` SQL function walks this provenance graph recursively
//! and returns a human-readable JSONB proof tree.

use pgrx::datum::DatumWithOid;
use pgrx::prelude::*;

// ─── Derivation recording ─────────────────────────────────────────────────────

/// Storage locations a rule's derived rows occupy for provenance recording.
pub(crate) struct DerivationTargets {
    /// FROM-able source of this run's newly derived `(s, o, g)` rows: a temp
    /// fixpoint table name (semi-naive) or a parenthesised query over the
    /// canonical delta storage filtered to inferred rows (plain path).
    pub delta_table: String,
    /// Parenthesised query projecting `(i, s, o, g)` of the materialised rows
    /// — the storage the recording joins against to resolve `derived_sid`.
    pub sid_source: String,
}

/// Canonical storage of derived rows for `pred_id`: the promoted HTAP delta
/// when the predicate has a dedicated VP table, `vp_rare` otherwise (VAL-207
/// CANON-TARGET — both inference paths materialise into one storage, so the
/// recording resolves `derived_sid` where the row actually lives).
pub(crate) fn canonical_sid_source(pred_id: i64) -> String {
    match crate::storage::vp_rare_io::get_dedicated_vp_table(pred_id) {
        Some(view) => format!("(SELECT i, s, o, g FROM {view}_delta)"),
        None => format!("(SELECT i, s, o, g FROM _pg_ripple.vp_rare WHERE p = {pred_id})"),
    }
}

/// Record derivation provenance for a single rule invocation.
///
/// `targets_fn` maps the rule's head predicate to the storage pair describing
/// where this run's derived rows sit; returning `None` skips recording for
/// that head (e.g. temp delta table not created).
pub fn record_rule_derivations_with_targets<F>(rule: &super::Rule, rule_set: &str, targets_fn: &F)
where
    F: Fn(i64) -> Option<DerivationTargets>,
{
    if !crate::RECORD_DERIVATIONS.get() {
        return;
    }
    let Some(head) = &rule.head else {
        return;
    };
    let head_pred = match &head.p {
        super::Term::Const(id) => *id,
        _ => return,
    };
    let Some(targets) = targets_fn(head_pred) else {
        return;
    };
    let Some(sql) = compile_antecedent_insert_with_targets(
        rule,
        rule_set,
        &targets.delta_table,
        &targets.sid_source,
    ) else {
        return;
    };
    if let Err(e) = Spi::run_with_args(&sql, &[]) {
        pgrx::warning!("derivation record error for rule '{}': {e}", rule.rule_text);
    }
}

/// Build the SQL INSERT into `_pg_ripple.derivations` for one rule.
///
/// `delta_table` restricts the recording to rows derived by this run (the
/// fixpoint temp delta, or the plain path's inferred-only delta query).
/// `sid_source` is the storage holding the materialised rows — the canonical
/// delta for promoted predicates, `vp_rare` for rare ones (VAL-207).  Body
/// atoms are read through [`vp_sid_read_expr`], so antecedents living in
/// dedicated VP tables (production: depends_on → vp_534) resolve to SIDs.
///
/// Returns `None` when the rule cannot be translated (recursive heads use the
/// stub, variable predicates, etc.).
fn compile_antecedent_insert_with_targets(
    rule: &super::Rule,
    rule_set: &str,
    delta_table: &str,
    sid_source: &str,
) -> Option<String> {
    use super::{BodyLiteral, Term};

    let head = rule.head.as_ref()?;

    // Head predicate must be a constant.
    let head_pred = match &head.p {
        Term::Const(id) => *id,
        _ => return None,
    };

    // Skip recursive rules — use delta-aware stub instead.
    let is_recursive = rule.body.iter().any(|lit| {
        if let BodyLiteral::Positive(atom) = lit {
            matches!(&atom.p, Term::Const(p) if *p == head_pred)
        } else {
            false
        }
    });
    if is_recursive {
        return record_recursive_rule_stub(rule, rule_set, delta_table, sid_source);
    }

    // Collect positive body atoms.
    let pos_atoms: Vec<&super::Atom> = rule
        .body
        .iter()
        .filter_map(|lit| {
            if let BodyLiteral::Positive(a) = lit {
                Some(a)
            } else {
                None
            }
        })
        .collect();

    if pos_atoms.is_empty() {
        return None;
    }

    // All body atom predicates must be constants.
    for atom in &pos_atoms {
        if !matches!(atom.p, Term::Const(_)) {
            return None;
        }
    }

    let mut var_map: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut from_join_parts: Vec<String> = Vec::new();
    let mut bid_columns: Vec<String> = Vec::new();

    for (idx, atom) in pos_atoms.iter().enumerate() {
        let pred_id = match &atom.p {
            Term::Const(id) => *id,
            _ => return None,
        };
        let alias = format!("b{idx}");
        let table_expr = format!("{} AS {alias}", super::compiler::vp_sid_read_expr(pred_id));
        bid_columns.push(format!("{alias}.i"));

        let mut join_conds: Vec<String> = Vec::new();

        match &atom.s {
            Term::Var(v) => {
                if let Some(existing) = var_map.get(v.as_str()) {
                    join_conds.push(format!("{alias}.s = {existing}"));
                } else {
                    var_map.insert(v.clone(), format!("{alias}.s"));
                }
            }
            Term::Const(id) => join_conds.push(format!("{alias}.s = {id}")),
            _ => {}
        }

        match &atom.o {
            Term::Var(v) => {
                if let Some(existing) = var_map.get(v.as_str()) {
                    join_conds.push(format!("{alias}.o = {existing}"));
                } else {
                    var_map.insert(v.clone(), format!("{alias}.o"));
                }
            }
            Term::Const(id) => join_conds.push(format!("{alias}.o = {id}")),
            _ => {}
        }

        match &atom.g {
            Term::Var(v) => {
                if let Some(existing) = var_map.get(v.as_str()) {
                    join_conds.push(format!("{alias}.g = {existing}"));
                } else {
                    var_map.insert(v.clone(), format!("{alias}.g"));
                }
            }
            Term::Const(id) => join_conds.push(format!("{alias}.g = {id}")),
            _ => {}
        }

        if idx == 0 {
            from_join_parts.push(format!("FROM {table_expr}"));
        } else if join_conds.is_empty() {
            from_join_parts.push(format!("CROSS JOIN {table_expr}"));
        } else {
            from_join_parts.push(format!("JOIN {table_expr} ON {}", join_conds.join(" AND ")));
        }
    }

    let head_s_sql = match &head.s {
        Term::Var(v) => var_map.get(v.as_str()).cloned()?,
        Term::Const(id) => id.to_string(),
        _ => return None,
    };
    let head_o_sql = match &head.o {
        Term::Var(v) => var_map.get(v.as_str()).cloned()?,
        Term::Const(id) => id.to_string(),
        _ => return None,
    };
    let head_g_sql = match &head.g {
        Term::Var(v) => var_map
            .get(v.as_str())
            .cloned()
            .unwrap_or_else(|| "0".to_owned()),
        Term::Const(id) => id.to_string(),
        Term::DefaultGraph => "0".to_owned(),
        Term::Wildcard => "0".to_owned(),
    };

    let from_join_sql = from_join_parts.join("\n  ");
    let antecedent_array = if bid_columns.is_empty() {
        "ARRAY[]::BIGINT[]".to_owned()
    } else {
        format!("ARRAY[{}]::BIGINT[]", bid_columns.join(", "))
    };

    // VAL-208 (v0.131.0): derivations store the rule's stable identity (the
    // explicit @name label or the auto fingerprint), not the full rule text.
    let rule_name_esc = super::rule_display_name(rule).replace('\'', "''");
    let rule_set_esc = rule_set.replace('\'', "''");

    // Join this run's delta rows against the canonical storage to get SIDs
    // for newly-derived head triples.
    let sql = format!(
        "INSERT INTO _pg_ripple.derivations \
           (derived_sid, rule_name, rule_set, antecedent_sids) \
         SELECT \
           vr_head.i, \
           '{rule_name_esc}'::text, \
           '{rule_set_esc}'::text, \
           {antecedent_array} \
         {from_join_sql} \
         JOIN (SELECT vr.i, vr.s, vr.o, vr.g \
               FROM {delta_table} dt \
               JOIN {sid_source} vr \
                 ON vr.s = dt.s AND vr.o = dt.o AND vr.g = dt.g \
              ) vr_head \
           ON vr_head.s = {head_s_sql} \
          AND vr_head.o = {head_o_sql} \
          AND vr_head.g = {head_g_sql} \
         ON CONFLICT (derived_sid, rule_name) DO NOTHING"
    );

    Some(sql)
}

/// Stub for recursive rules when a delta table is available.
fn record_recursive_rule_stub(
    rule: &super::Rule,
    rule_set: &str,
    delta_table: &str,
    sid_source: &str,
) -> Option<String> {
    rule.head.as_ref()?;

    // VAL-208 (v0.131.0): stable rule identity, as above.
    let rule_name_esc = super::rule_display_name(rule).replace('\'', "''");
    let rule_set_esc = rule_set.replace('\'', "''");

    let sql = format!(
        "INSERT INTO _pg_ripple.derivations \
           (derived_sid, rule_name, rule_set, antecedent_sids) \
         SELECT vr.i, '{rule_name_esc}'::text, '{rule_set_esc}'::text, ARRAY[]::BIGINT[] \
         FROM {delta_table} dt \
         JOIN {sid_source} vr \
           ON vr.s = dt.s AND vr.o = dt.o AND vr.g = dt.g \
         ON CONFLICT (derived_sid, rule_name) DO NOTHING"
    );

    Some(sql)
}

// ─── Orphan cleanup (DRed integration) ───────────────────────────────────────

/// Remove derivation rows whose `derived_sid` no longer exists in `vp_rare`
/// or any dedicated VP table.  Called after DRed retraction and optionally
/// exposed via the `vacuum_derivations()` SQL function.
///
/// Returns the number of rows removed.
pub fn vacuum_orphan_derivations() -> i64 {
    // A derived_sid is orphaned when it no longer resolves in any live
    // storage: `vp_rare` or any promoted predicate's `vp_{id}` view
    // (main − tombstones ∪ delta).  VAL-207 CANON-TARGET: derived rows for
    // promoted predicates canonicalise in `{vp}_delta`, so a vp_rare-only
    // check would delete provenance of live facts.
    let live = live_sids_expr();
    let sql = format!(
        "WITH deleted AS ( \
         DELETE FROM _pg_ripple.derivations d \
         WHERE NOT EXISTS ( \
             SELECT 1 FROM {live} lr WHERE lr.i = d.derived_sid \
         ) \
         RETURNING 1 \
     ) SELECT COUNT(*)::bigint FROM deleted"
    );

    Spi::get_one::<i64>(&sql).unwrap_or(None).unwrap_or(0)
}

/// Parenthesised UNION of every live SID source: `vp_rare` plus each promoted
/// predicate's `vp_{id}` view.  Shared by storage-agnostic SID existence
/// checks (derivation vacuum, conflict scan).
pub(crate) fn live_sids_expr() -> String {
    let promoted: Vec<String> = Spi::connect(|c| {
        Ok::<Vec<String>, pgrx::spi::SpiError>(
            c.select(
                "SELECT id::text FROM _pg_ripple.predicates WHERE table_oid IS NOT NULL",
                None,
                &[],
            )?
            .filter_map(|row| row.get::<String>(1).ok().flatten())
            .collect(),
        )
    })
    .unwrap_or_default();
    let mut arms = vec!["SELECT i FROM _pg_ripple.vp_rare".to_owned()];
    for id in promoted {
        arms.push(format!("SELECT i FROM _pg_ripple.vp_{id}"));
    }
    format!("({})", arms.join(" UNION ALL "))
}

/// Parenthesised UNION of every DERIVED row across storages, projecting
/// `(i, s, o, p, g)`: `vp_rare` rows with `source = 1` plus each promoted
/// predicate's `{vp}_delta` rows with `source = 1` (predicate synthesised
/// from the table name — delta tables have no `p` column).
///
/// Consumers of "inferred triples" that must follow the canonical storage
/// (runtime conflict scan, inference explain) read through this expression.
pub(crate) fn derived_rows_expr() -> String {
    let promoted: Vec<String> = Spi::connect(|c| {
        Ok::<Vec<String>, pgrx::spi::SpiError>(
            c.select(
                "SELECT id::text FROM _pg_ripple.predicates WHERE table_oid IS NOT NULL",
                None,
                &[],
            )?
            .filter_map(|row| row.get::<String>(1).ok().flatten())
            .collect(),
        )
    })
    .unwrap_or_default();
    let mut arms = vec!["SELECT i, s, o, p, g FROM _pg_ripple.vp_rare WHERE source = 1".to_owned()];
    for id in promoted {
        arms.push(format!(
            "SELECT i, s, o, {id}, g FROM _pg_ripple.vp_{id}_delta WHERE source = 1"
        ));
    }
    format!("({})", arms.join(" UNION ALL "))
}

// ─── proof-tree builder ───────────────────────────────────────────────────────

/// Look up the dictionary ID for an IRI/literal string.
/// Returns `None` if not found in the dictionary.
fn dict_id_for(value: &str) -> Option<i64> {
    Spi::get_one_with_args::<i64>(
        "SELECT id FROM _pg_ripple.dictionary WHERE value = $1 LIMIT 1",
        &[DatumWithOid::from(value)],
    )
    .ok()
    .flatten()
}

/// Decode a dictionary ID to its human-readable string value.
#[allow(dead_code)]
fn dict_decode(id: i64) -> String {
    Spi::get_one_with_args::<String>(
        "SELECT value FROM _pg_ripple.dictionary WHERE id = $1",
        &[DatumWithOid::from(id)],
    )
    .ok()
    .flatten()
    .unwrap_or_else(|| format!("<id:{id}>"))
}

/// Batch-decode a list of dictionary IDs to their string values.
/// Returns a HashMap from id → value.
fn batch_decode(ids: &[i64]) -> std::collections::HashMap<i64, String> {
    if ids.is_empty() {
        return std::collections::HashMap::new();
    }
    let id_list: String = ids
        .iter()
        .map(|id| id.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT id, value FROM _pg_ripple.dictionary WHERE id = ANY(ARRAY[{id_list}]::BIGINT[])"
    );
    Spi::connect(|client| {
        client
            .select(&sql, None, &[])
            .unwrap_or_else(|e| pgrx::error!("batch decode SPI error: {e}"))
            .map(|row| {
                let id = row.get::<i64>(1).ok().flatten().unwrap_or(0);
                let val = row.get::<String>(2).ok().flatten().unwrap_or_default();
                (id, val)
            })
            .collect::<std::collections::HashMap<_, _>>()
    })
}

/// Look up the `vp_rare.i` (SID) for a triple `(s_id, p_id, o_id)`.
///
/// VAL-207: checks `vp_rare` first (legacy lookup order), then the predicate's
/// promoted `vp_{id}` view when one exists — the view carries main −
/// tombstones plus the delta inbox, so facts materialised in `{vp}_delta` are
/// reachable by the graph-blind `justify()`, exactly as the graph-scoped
/// variant reads them.
fn sid_for_triple(s_id: i64, p_id: i64, o_id: i64) -> Option<i64> {
    let rare = Spi::get_one_with_args::<i64>(
        "SELECT i FROM _pg_ripple.vp_rare WHERE p = $1 AND s = $2 AND o = $3 LIMIT 1",
        &[
            DatumWithOid::from(p_id),
            DatumWithOid::from(s_id),
            DatumWithOid::from(o_id),
        ],
    )
    .ok()
    .flatten();
    if rare.is_some() {
        return rare;
    }

    let view = crate::storage::vp_rare_io::get_dedicated_vp_table(p_id)?;
    Spi::get_one_with_args::<i64>(
        &format!("SELECT i FROM {view} WHERE s = $1 AND o = $2 ORDER BY i LIMIT 1"),
        &[DatumWithOid::from(s_id), DatumWithOid::from(o_id)],
    )
    .ok()
    .flatten()
}

/// Read derivation rows for a given SID.
/// Returns a list of `(rule_name, rule_set, antecedent_sids)`.
fn derivations_for_sid(sid: i64) -> Vec<(String, String, Vec<i64>)> {
    let sql = "SELECT rule_name, rule_set, antecedent_sids \
               FROM _pg_ripple.derivations \
               WHERE derived_sid = $1";
    Spi::connect(|client| {
        client
            .select(sql, None, &[DatumWithOid::from(sid)])
            .unwrap_or_else(|e| pgrx::error!("derivation lookup SPI error: {e}"))
            .map(|row| {
                let rn = row.get::<String>(1).ok().flatten().unwrap_or_default();
                let rs = row.get::<String>(2).ok().flatten().unwrap_or_default();
                let ant: Vec<i64> = row.get::<Vec<i64>>(3).ok().flatten().unwrap_or_default();
                (rn, rs, ant)
            })
            .collect::<Vec<_>>()
    })
}

/// Resolve the current rule text for a derivation's `(rule_set, rule_name)`.
///
/// VAL-208: `rule_name` is a stable identity, not the rule text; display
/// surfaces (justify proof trees, runtime conflict reports) resolve the text
/// through the rules catalog.  Returns `None` when the rule no longer exists
/// (removed or edited away) — callers fall back to the raw name.
pub(crate) fn resolve_rule_text(rule_set: &str, rule_name: &str) -> Option<String> {
    Spi::get_one_with_args::<String>(
        "SELECT rule_text FROM _pg_ripple.rules \
          WHERE rule_set = $1 AND name = $2 LIMIT 1",
        &[
            pgrx::datum::DatumWithOid::from(rule_set),
            pgrx::datum::DatumWithOid::from(rule_name),
        ],
    )
    .ok()
    .flatten()
}

/// Get the triple `(p, s, o)` for a given SID from vp_rare.
///
/// VAL-207: delegates to the cross-storage [`triple_ids_for_sid`] — with
/// canonical delta materialisation, a derived SID for a promoted predicate
/// resolves in `{vp}_delta`/main, not `vp_rare`.
fn triple_for_sid(sid: i64) -> Option<(i64, i64, i64)> {
    triple_ids_for_sid(sid)
}

/// Recursively build the JSONB proof tree for a given SID.
///
/// `visited` guards against cycles in the derivation graph.
/// `depth` prevents stack overflow on pathological derivation chains.
/// `node_count` tracks total nodes built to enforce `pg_ripple.proof_tree_max_nodes`.
/// (M16-07 v0.116.0): depth capped by `pg_ripple.proof_tree_max_depth` (PT0480);
/// node count capped by `pg_ripple.proof_tree_max_nodes` (PT0481).
fn build_proof_tree(
    sid: i64,
    visited: &mut std::collections::HashSet<i64>,
    depth: u32,
    node_count: &mut u32,
    max_depth: u32,
    max_nodes: u32,
) -> serde_json::Value {
    // Node-count overflow guard — PT0481.
    *node_count += 1;
    if *node_count > max_nodes {
        pgrx::warning!(
            "PT0481: proof tree exceeded pg_ripple.proof_tree_max_nodes={max_nodes}; \
             truncating further antecedents"
        );
        return serde_json::json!({
            "sid": sid,
            "max_nodes_reached": true
        });
    }

    // Cycle guard.
    if !visited.insert(sid) {
        return serde_json::json!({
            "sid": sid,
            "cycle": true
        });
    }

    // Depth overflow guard — PT0480.
    if depth >= max_depth {
        pgrx::warning!(
            "PT0480: proof tree exceeded pg_ripple.proof_tree_max_depth={max_depth}; \
             truncating at depth {depth}"
        );
        visited.remove(&sid);
        return serde_json::json!({
            "sid": sid,
            "max_depth_reached": true
        });
    }

    // Look up the triple's human-readable labels.
    let triple_label = if let Some((p_id, s_id, o_id)) = triple_for_sid(sid) {
        let decode_map = batch_decode(&[s_id, p_id, o_id]);
        serde_json::json!({
            "subject":   decode_map.get(&s_id).cloned().unwrap_or_else(|| format!("<id:{s_id}>")),
            "predicate": decode_map.get(&p_id).cloned().unwrap_or_else(|| format!("<id:{p_id}>")),
            "object":    decode_map.get(&o_id).cloned().unwrap_or_else(|| format!("<id:{o_id}>"))
        })
    } else {
        serde_json::json!({ "sid": sid })
    };

    let derivation_rows = derivations_for_sid(sid);

    if derivation_rows.is_empty() {
        // Base fact — no derivation recorded.
        visited.remove(&sid);
        return serde_json::json!({
            "type": "base",
            "sid": sid,
            "triple": triple_label
        });
    }

    // Build one entry per derivation rule (a triple may be derived by multiple rules).
    let mut rules_json: Vec<serde_json::Value> = Vec::new();
    for (rule_name, rule_set, antecedent_sids) in &derivation_rows {
        let mut antecedents_json: Vec<serde_json::Value> = Vec::new();
        for &ant_sid in antecedent_sids {
            antecedents_json.push(build_proof_tree(
                ant_sid,
                visited,
                depth + 1,
                node_count,
                max_depth,
                max_nodes,
            ));
        }
        // VAL-208: `rule` keeps the resolved rule text (what the reader
        // wants); `rule_name` is the stable identity derivations store.
        let rule_display =
            resolve_rule_text(rule_set, rule_name).unwrap_or_else(|| rule_name.clone());
        rules_json.push(serde_json::json!({
            "rule": rule_display,
            "rule_name": rule_name,
            "rule_set": rule_set,
            "antecedents": antecedents_json
        }));
    }

    visited.remove(&sid);
    serde_json::json!({
        "type": "inferred",
        "sid": sid,
        "triple": triple_label,
        "derivations": rules_json
    })
}

/// Public entry point for the `justify()` SQL function.
///
/// Looks up the SID for the triple `(subject, predicate, object)` and calls
/// `build_proof_tree`.  Returns `None` (SQL NULL) when the triple is not found.
pub fn justify_impl(subject: &str, predicate: &str, object: &str) -> Option<serde_json::Value> {
    let s_id = dict_id_for(subject)?;
    let p_id = dict_id_for(predicate)?;
    let o_id = dict_id_for(object)?;
    let sid = sid_for_triple(s_id, p_id, o_id)?;

    let max_depth = crate::gucs::datalog::PROOF_TREE_MAX_DEPTH.get().max(1) as u32;
    let max_nodes = crate::gucs::datalog::PROOF_TREE_MAX_NODES.get().max(10) as u32;
    let mut visited = std::collections::HashSet::new();
    let mut node_count: u32 = 0;
    let tree = build_proof_tree(sid, &mut visited, 0, &mut node_count, max_depth, max_nodes);
    Some(tree)
}

// ─── graph-scoped justification ───────────────────────────────────────────────
//
// `justify(s, p, o)` above is graph-blind: `sid_for_triple` matches across every
// named graph and `build_proof_tree` expands antecedents without checking where
// they live.  With per-tenant named graphs that is a cross-tenant leak — the
// proof tree (and the bare `unverified`/`inferred` distinction) exposes facts
// from graphs the caller has no access to.
//
// The graph-scoped variant validates the root triple AND every antecedent
// against one named graph, reading the same storages the SPARQL engine's
// `GRAPH <g>` evaluation reads: `_pg_ripple.vp_rare`, promoted `vp_{id}` views
// (main − tombstones UNION ALL delta).  The 3-arg SQL signature is preserved.

/// Resolve a named-graph IRI (with or without `<>`) to its dictionary ID
/// without inserting.  Graph IDs follow `insert_triple`'s normalization.
fn graph_id_for(iri: &str) -> Option<i64> {
    dict_id_for(crate::storage::strip_angle_brackets_pub(iri))
}

/// Look up the SID for `(p_id, s_id, o_id)` inside graph `g_id`.
///
/// Checks `_pg_ripple.vp_rare` first (legacy lookup order), then the
/// predicate's promoted `vp_{id}` view when one exists.  The view carries
/// main − tombstones plus the delta inbox, so a triple still in flight from a
/// merge is visible here exactly as the SPARQL engine sees it.
fn sid_for_triple_in_graph(p_id: i64, s_id: i64, o_id: i64, g_id: i64) -> Option<i64> {
    let rare = Spi::get_one_with_args::<i64>(
        "SELECT i FROM _pg_ripple.vp_rare \
         WHERE p = $1 AND s = $2 AND o = $3 AND g = $4 \
         ORDER BY i LIMIT 1",
        &[
            DatumWithOid::from(p_id),
            DatumWithOid::from(s_id),
            DatumWithOid::from(o_id),
            DatumWithOid::from(g_id),
        ],
    )
    .ok()
    .flatten();
    if rare.is_some() {
        return rare;
    }

    let table = crate::storage::vp_rare_io::get_dedicated_vp_table(p_id)?;
    Spi::get_one_with_args::<i64>(
        &format!("SELECT i FROM {table} WHERE s = $1 AND o = $2 AND g = $3 ORDER BY i LIMIT 1"),
        &[
            DatumWithOid::from(s_id),
            DatumWithOid::from(o_id),
            DatumWithOid::from(g_id),
        ],
    )
    .ok()
    .flatten()
}

/// Candidate predicate IDs for a SID, statements range catalog first.
fn predicate_candidates_for_sid(sid: i64) -> Vec<i64> {
    let mut preds: Vec<i64> = Vec::new();

    // Fast path: range mapping catalog.
    let from_catalog: Vec<i64> = Spi::connect(|c| {
        Ok::<Vec<i64>, pgrx::spi::SpiError>(
            c.select(
                "SELECT predicate_id FROM _pg_ripple.statements \
             WHERE sid_min <= $1 AND sid_max >= $1 \
             ORDER BY sid_min DESC",
                None,
                &[DatumWithOid::from(sid)],
            )?
            .filter_map(|row| row.get::<i64>(1).ok().flatten())
            .collect(),
        )
    })
    .unwrap_or_default();
    for id in from_catalog {
        if !preds.contains(&id) {
            preds.push(id);
        }
    }

    // Fallback: every promoted predicate (catalog row may lag for edge cases;
    // mirrors get_statement_by_sid's fallback).
    let all_promoted: Vec<i64> = Spi::connect(|c| {
        Ok::<Vec<i64>, pgrx::spi::SpiError>(
            c.select(
                "SELECT id FROM _pg_ripple.predicates WHERE table_oid IS NOT NULL",
                None,
                &[],
            )?
            .filter_map(|row| row.get::<i64>(1).ok().flatten())
            .collect(),
        )
    })
    .unwrap_or_default();
    for id in all_promoted {
        if !preds.contains(&id) {
            preds.push(id);
        }
    }

    preds
}

/// Does SID `sid` live in graph `g_id`?
///
/// Rare predicates are answered by `vp_rare (i, g)`; promoted ones by their
/// `vp_{id}` view — which already excludes tombstoned main rows.
fn sid_in_graph(sid: i64, g_id: i64) -> bool {
    let rare = Spi::get_one_with_args::<bool>(
        "SELECT EXISTS (SELECT 1 FROM _pg_ripple.vp_rare WHERE i = $1 AND g = $2)",
        &[DatumWithOid::from(sid), DatumWithOid::from(g_id)],
    )
    .ok()
    .flatten();
    if rare == Some(true) {
        return true;
    }

    for p_id in predicate_candidates_for_sid(sid) {
        let Some(table) = crate::storage::vp_rare_io::get_dedicated_vp_table(p_id) else {
            continue;
        };
        let hit = Spi::get_one_with_args::<bool>(
            &format!("SELECT EXISTS (SELECT 1 FROM {table} WHERE i = $1 AND g = $2)"),
            &[DatumWithOid::from(sid), DatumWithOid::from(g_id)],
        )
        .ok()
        .flatten();
        if hit == Some(true) {
            return true;
        }
    }
    false
}

/// Get `(p_id, s_id, o_id)` for a SID across storages (vp_rare, then promoted
/// views).  Used for label lookup only — callers must have validated the SID
/// against the target graph first.
fn triple_ids_for_sid(sid: i64) -> Option<(i64, i64, i64)> {
    if let Some(triple) = Spi::connect(|c| {
        c.select(
            "SELECT p, s, o FROM _pg_ripple.vp_rare WHERE i = $1 LIMIT 1",
            Some(1),
            &[DatumWithOid::from(sid)],
        )
        .ok()
        .and_then(|rows| {
            rows.filter_map(|row| {
                let p = row.get::<i64>(1).ok().flatten()?;
                let s = row.get::<i64>(2).ok().flatten()?;
                let o = row.get::<i64>(3).ok().flatten()?;
                Some((p, s, o))
            })
            .next()
        })
    }) {
        return Some(triple);
    }

    for p_id in predicate_candidates_for_sid(sid) {
        let Some(table) = crate::storage::vp_rare_io::get_dedicated_vp_table(p_id) else {
            continue;
        };
        if let Some(triple) = Spi::connect(|c| {
            c.select(
                &format!("SELECT s, o FROM {table} WHERE i = $1 LIMIT 1"),
                Some(1),
                &[DatumWithOid::from(sid)],
            )
            .ok()
            .and_then(|rows| {
                rows.filter_map(|row| {
                    let s = row.get::<i64>(1).ok().flatten()?;
                    let o = row.get::<i64>(2).ok().flatten()?;
                    Some((p_id, s, o))
                })
                .next()
            })
        }) {
            return Some(triple);
        }
    }
    None
}

/// Graph-scoped twin of [`build_proof_tree`].
///
/// Returns `None` when the node's proof cannot be completed inside graph
/// `g_id`: the SID is not in the graph, or every derivation branch references
/// an antecedent outside it.  `None` propagates — a partial tree would let a
/// caller present an under-proven fact as proven, and a foreign antecedent
/// must never surface, not even as "exists elsewhere".
fn build_proof_tree_graph(
    sid: i64,
    g_id: i64,
    visited: &mut std::collections::HashSet<i64>,
    depth: u32,
    node_count: &mut u32,
    max_depth: u32,
    max_nodes: u32,
) -> Option<serde_json::Value> {
    // Node-count overflow guard — PT0481 (same truncation marker as the
    // graph-blind builder; the SID is in-graph, so the marker leaks nothing).
    *node_count += 1;
    if *node_count > max_nodes {
        pgrx::warning!(
            "PT0481: proof tree exceeded pg_ripple.proof_tree_max_nodes={max_nodes}; \
             truncating further antecedents"
        );
        return Some(serde_json::json!({
            "sid": sid,
            "max_nodes_reached": true
        }));
    }

    // Cycle guard.
    if !visited.insert(sid) {
        return Some(serde_json::json!({
            "sid": sid,
            "cycle": true
        }));
    }

    // Depth overflow guard — PT0480.
    if depth >= max_depth {
        pgrx::warning!(
            "PT0480: proof tree exceeded pg_ripple.proof_tree_max_depth={max_depth}; \
             truncating at depth {depth}"
        );
        visited.remove(&sid);
        return Some(serde_json::json!({
            "sid": sid,
            "max_depth_reached": true
        }));
    }

    // Graph membership: a node outside the caller's graph is invisible here.
    if !sid_in_graph(sid, g_id) {
        visited.remove(&sid);
        return None;
    }

    // Look up the triple's human-readable labels (SID already validated above).
    let triple_label = if let Some((p_id, s_id, o_id)) = triple_ids_for_sid(sid) {
        let decode_map = batch_decode(&[s_id, p_id, o_id]);
        serde_json::json!({
            "subject":   decode_map.get(&s_id).cloned().unwrap_or_else(|| format!("<id:{s_id}>")),
            "predicate": decode_map.get(&p_id).cloned().unwrap_or_else(|| format!("<id:{p_id}>")),
            "object":    decode_map.get(&o_id).cloned().unwrap_or_else(|| format!("<id:{o_id}>"))
        })
    } else {
        serde_json::json!({ "sid": sid })
    };

    let derivation_rows = derivations_for_sid(sid);

    if derivation_rows.is_empty() {
        // Base fact — no derivation recorded.
        visited.remove(&sid);
        return Some(serde_json::json!({
            "type": "base",
            "sid": sid,
            "triple": triple_label
        }));
    }

    // One entry per derivation rule.  A derivation whose proof reaches outside
    // the graph is dropped entirely; if no derivation survives the node's proof
    // is incomplete and `None` propagates to the root.
    let mut rules_json: Vec<serde_json::Value> = Vec::new();
    for (rule_name, rule_set, antecedent_sids) in &derivation_rows {
        let mut antecedents_json: Vec<serde_json::Value> = Vec::new();
        let mut complete = true;
        for &ant_sid in antecedent_sids {
            match build_proof_tree_graph(
                ant_sid,
                g_id,
                visited,
                depth + 1,
                node_count,
                max_depth,
                max_nodes,
            ) {
                Some(subtree) => antecedents_json.push(subtree),
                None => {
                    complete = false;
                    break;
                }
            }
        }
        if complete {
            // VAL-208: resolved text + stable identity, as in the graph-blind
            // builder above.
            let rule_display =
                resolve_rule_text(rule_set, rule_name).unwrap_or_else(|| rule_name.clone());
            rules_json.push(serde_json::json!({
                "rule": rule_display,
                "rule_name": rule_name,
                "rule_set": rule_set,
                "antecedents": antecedents_json
            }));
        }
    }

    visited.remove(&sid);
    if rules_json.is_empty() {
        return None;
    }
    Some(serde_json::json!({
        "type": "inferred",
        "sid": sid,
        "triple": triple_label,
        "derivations": rules_json
    }))
}

/// Public entry point for the graph-scoped `justify(s, p, o, graph)` overload.
///
/// Resolves the graph IRI the same way `insert_triple` does (angle brackets
/// stripped, IRIs dictionary-encoded).  Returns `NULL` (SQL NULL) when:
///
/// - the graph IRI is unknown to the dictionary;
/// - the triple is not present in that graph (`vp_rare`, promoted VP views,
///   delta);
/// - the triple is present but its recorded proof cannot be completed inside
///   the graph (some antecedent lives outside it).
///
/// Callers that need to separate "absent" from "present but under-proven"
/// must ask existence separately (SPARQL `ASK GRAPH <g>`, which reads the
/// same storages); `justify` deliberately returns the same NULL for both so
/// the proof tree can never leak another graph's existence.
pub fn justify_in_graph_impl(
    subject: &str,
    predicate: &str,
    object: &str,
    graph: &str,
) -> Option<serde_json::Value> {
    let g_id = graph_id_for(graph)?;
    let s_id = dict_id_for(subject)?;
    let p_id = dict_id_for(predicate)?;
    let o_id = dict_id_for(object)?;
    let sid = sid_for_triple_in_graph(p_id, s_id, o_id, g_id)?;

    let max_depth = crate::gucs::datalog::PROOF_TREE_MAX_DEPTH.get().max(1) as u32;
    let max_nodes = crate::gucs::datalog::PROOF_TREE_MAX_NODES.get().max(10) as u32;
    let mut visited = std::collections::HashSet::new();
    let mut node_count: u32 = 0;
    build_proof_tree_graph(
        sid,
        g_id,
        &mut visited,
        0,
        &mut node_count,
        max_depth,
        max_nodes,
    )
}
