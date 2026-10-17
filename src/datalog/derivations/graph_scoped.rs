//! Graph-scoped proof-tree justification (VAL-391).
//!
//! Split out of `derivations.rs` (file-size gate Q13-04): the 4-arg
//! `justify(s, p, o, graph)` path that validates the root triple and every
//! antecedent against one named graph, resolving provenance by logical triple.

use pgrx::datum::DatumWithOid;
use pgrx::prelude::*;

use super::{batch_decode, derivations_for_sid, dict_id_for, resolve_rule_text};

// ─── graph-scoped justification ───────────────────────────────────────────────
//
// `justify(s, p, o)` (parent module) is graph-blind: `sid_for_triple` matches across every
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
pub(super) fn triple_ids_for_sid(sid: i64) -> Option<(i64, i64, i64)> {
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

/// Graph-scoped twin of [`super::build_proof_tree`].
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
    let node_ids = triple_ids_for_sid(sid);
    let triple_label = if let Some((p_id, s_id, o_id)) = node_ids {
        let decode_map = batch_decode(&[s_id, p_id, o_id]);
        serde_json::json!({
            "subject":   decode_map.get(&s_id).cloned().unwrap_or_else(|| format!("<id:{s_id}>")),
            "predicate": decode_map.get(&p_id).cloned().unwrap_or_else(|| format!("<id:{p_id}>")),
            "object":    decode_map.get(&o_id).cloned().unwrap_or_else(|| format!("<id:{o_id}>"))
        })
    } else {
        serde_json::json!({ "sid": sid })
    };

    // VAL-391: provenance is triple-keyed.  When THIS row carries no recorded
    // derivation, fall back to derivations recorded for another
    // materialisation of the same logical triple — rebuilt with antecedents
    // resolved inside the caller's graph, so the tree only ever names triples
    // the graph already holds.  When no branch survives (or the triple cannot
    // be resolved), the node stays a plain base fact, exactly as before.
    let derivation_rows: Vec<(String, String, Vec<i64>)> = match derivations_for_sid(sid) {
        rows if !rows.is_empty() => rows,
        _ => match node_ids {
            Some((p_id, s_id, o_id)) => {
                let fallback = cross_graph_derivations_in_graph(s_id, p_id, o_id, g_id);
                if fallback.is_empty() {
                    visited.remove(&sid);
                    return Some(serde_json::json!({
                        "type": "base",
                        "sid": sid,
                        "triple": triple_label
                    }));
                }
                fallback
            }
            None => {
                visited.remove(&sid);
                return Some(serde_json::json!({
                    "type": "base",
                    "sid": sid,
                    "triple": triple_label
                }));
            }
        },
    };

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
            // builder in the parent module.
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

/// All SIDs materialising the logical triple `(s_id, p_id, o_id)` across
/// storages and graphs — `vp_rare` plus the predicate's dedicated VP view
/// when one exists (VAL-391).
fn sids_for_triple_any_graph(p_id: i64, s_id: i64, o_id: i64) -> Vec<i64> {
    let mut sids: Vec<i64> = Spi::connect(|c| {
        Ok::<_, pgrx::spi::SpiError>(
            c.select(
                "SELECT i FROM _pg_ripple.vp_rare WHERE p = $1 AND s = $2 AND o = $3",
                None,
                &[
                    DatumWithOid::from(p_id),
                    DatumWithOid::from(s_id),
                    DatumWithOid::from(o_id),
                ],
            )?
            .filter_map(|row| row.get::<i64>(1).ok().flatten())
            .collect(),
        )
    })
    .unwrap_or_default();

    if let Some(view) = crate::storage::vp_rare_io::get_dedicated_vp_table(p_id) {
        let promoted: Vec<i64> = Spi::connect(|c| {
            Ok::<_, pgrx::spi::SpiError>(
                c.select(
                    &format!("SELECT i FROM {view} WHERE s = $1 AND o = $2"),
                    None,
                    &[DatumWithOid::from(s_id), DatumWithOid::from(o_id)],
                )?
                .filter_map(|row| row.get::<i64>(1).ok().flatten())
                .collect(),
            )
        })
        .unwrap_or_default();
        sids.extend(promoted);
    }
    sids
}

/// Map antecedent SIDs recorded against a foreign materialisation of the
/// triple to the SIDs of the SAME triples inside `g_id`.  `None` when any
/// antecedent triple is absent from the graph — the branch stays incomplete,
/// never a partial tree (same rule the in-graph builder applies to own rows).
fn antecedents_in_graph(antecedent_sids: &[i64], g_id: i64) -> Option<Vec<i64>> {
    let mut mapped = Vec::with_capacity(antecedent_sids.len());
    for &ant in antecedent_sids {
        let (p_id, s_id, o_id) = triple_ids_for_sid(ant)?;
        mapped.push(sid_for_triple_in_graph(p_id, s_id, o_id, g_id)?);
    }
    Some(mapped)
}

/// Derivation rows recorded for ANY materialisation of the logical triple
/// `(s_id, p_id, o_id)`, rebuilt so every antecedent resolves inside `g_id`
/// (VAL-391).  Provenance is keyed by triple values + rule application, not
/// by row identity: a graph holding the conclusion AND all antecedents of a
/// recorded rule application gets the same tree the recording graph gets,
/// with its own SIDs; a graph missing any antecedent gets no tree (no leak).
fn cross_graph_derivations_in_graph(
    s_id: i64,
    p_id: i64,
    o_id: i64,
    g_id: i64,
) -> Vec<(String, String, Vec<i64>)> {
    let mut rows: Vec<(String, String, Vec<i64>)> = Vec::new();
    for cand in sids_for_triple_any_graph(p_id, s_id, o_id) {
        for (rule_name, rule_set, antecedent_sids) in derivations_for_sid(cand) {
            if let Some(mapped) = antecedents_in_graph(&antecedent_sids, g_id) {
                rows.push((rule_name, rule_set, mapped));
            }
        }
    }
    rows
}
