//! Rule-to-SQL compilation entry points and the non-recursive rule compiler.
//!
//! Split out of `compiler/mod.rs` (VAL-350, file-size gate Q13-04). `mod.rs`
//! keeps the shared SQL-expression, costing and guard helpers; recursive-rule
//! compilation lives in `sql.rs`.

use super::{
    VarMap, arith_op_sql, build_antijoin_on_cond, build_join_cond, build_not_exists_conds,
    compile_guard_sql, const_sql, cost_order_atoms, estimate_pred_cardinality, guard_fully_bound,
    has_dedicated_vp, is_recursive_rule, render_comparison_term, resolve_predicate_id,
    resolve_term_ref, vp_read_expr, vp_table,
};
use crate::datalog::{Atom, BodyLiteral, Rule, Term};

// ─── Main compiler ────────────────────────────────────────────────────────────

/// Compile a slice of rules to SQL INSERT statements.
///
/// Rules in the slice are assumed to be from the same stratum.
/// Recursive rules within the slice share a `WITH RECURSIVE` CTE.
pub fn compile_rule_set(rules: &[Rule]) -> Result<Vec<String>, String> {
    let mut sqls = Vec::new();
    for rule in rules {
        if rule.head.is_none() {
            // Constraint rule — not materialized here; use compile_constraint_check.
            continue;
        }
        let sql = compile_single_rule(rule)?;
        sqls.push(sql);
    }
    Ok(sqls)
}

/// Compile a single rule inserting into `target` (with columns `(s, o, g)`).
/// Used by semi-naive inference to target temp tables instead of HTAP delta tables.
pub fn compile_single_rule_to(rule: &Rule, target: &str) -> Result<String, String> {
    compile_single_rule_inner(rule, target, false)
}

/// Compile a single derivation rule to a SQL INSERT statement.
///
/// The insert target is the head predicate's HTAP delta table (created
/// on-demand for predicates without a dedicated VP table), and rows are
/// materialised as inferred (`source = 1`) — the plain inference path writes
/// the same storage and provenance class as the semi-naive fixpoint (VAL-207).
pub fn compile_single_rule(rule: &Rule) -> Result<String, String> {
    let head = rule
        .head
        .as_ref()
        .ok_or_else(|| "cannot compile constraint rule as INSERT".to_owned())?;

    let head_pred = match &head.p {
        Term::Const(id) => *id,
        Term::Var(_) => return Err("variable predicate in rule head is not supported".to_owned()),
        _ => return Err("invalid predicate term in rule head".to_owned()),
    };

    // For new predicates without a dedicated VP table, create the HTAP split
    // on-demand so the INSERT does not fail with "relation does not exist".
    // (v0.29.0 bug fix: pre-existing infer() path used compile_single_rule
    // which always targeted the delta table, even for new predicates.)
    if !has_dedicated_vp(head_pred) {
        crate::storage::merge::ensure_htap_tables(head_pred);
    }
    let target = format!("{}_delta", vp_table(head_pred));

    compile_single_rule_inner(rule, &target, true)
}

/// Shared body of [`compile_single_rule`] / [`compile_single_rule_to`].
///
/// `mark_inferred` appends the `source` column with value `1` to the INSERT —
/// only valid for HTAP delta targets (temp fixpoint tables have no `source`
/// column).
fn compile_single_rule_inner(
    rule: &Rule,
    target: &str,
    mark_inferred: bool,
) -> Result<String, String> {
    let head = rule
        .head
        .as_ref()
        .ok_or_else(|| "cannot compile constraint rule as INSERT".to_owned())?;

    let head_pred = match &head.p {
        Term::Const(id) => *id,
        Term::Var(_) => return Err("variable predicate in rule head is not supported".to_owned()),
        _ => return Err("invalid predicate term in rule head".to_owned()),
    };

    let head_g_expr = match &head.g {
        Term::Const(id) => const_sql(*id),
        Term::Var(v) => format!("g_var_{v}"),
        Term::DefaultGraph => "0".to_owned(),
        Term::Wildcard => "0".to_owned(),
    };

    let is_recursive = is_recursive_rule(rule, head_pred);
    if is_recursive {
        super::sql::compile_recursive_rule(rule, head_pred, &head_g_expr, target, mark_inferred)
    } else {
        compile_nonrecursive_rule(rule, head_pred, &head_g_expr, target, mark_inferred)
    }
}

/// Compile a non-recursive rule to `INSERT … SELECT … ON CONFLICT DO NOTHING`.
///
/// # v0.29.0 optimizations applied here
///
/// 1. **Cost-based body atom reordering**: positive atoms sorted by estimated
///    VP-table cardinality when `pg_ripple.datalog_cost_reorder = true`.
/// 2. **Anti-join negation**: negated atoms compile to `LEFT JOIN … IS NULL`
///    when the table has ≥ `pg_ripple.datalog_antijoin_threshold` rows.
/// 3. **Predicate-filter pushdown**: Compare/StringBuiltin guards that have all
///    their variables bound after the most recent positive atom are inlined into
///    that atom's `JOIN … ON` clause instead of the outer `WHERE` clause.
fn compile_nonrecursive_rule(
    rule: &Rule,
    _head_pred: i64,
    _head_g_expr: &str,
    target: &str,
    mark_inferred: bool,
) -> Result<String, String> {
    let head = rule
        .head
        .as_ref()
        .ok_or_else(|| "compile_nonrecursive_rule: rule has no head".to_string())?;

    // ── Step 1: Sort positive body atoms by cost (v0.29.0) ────────────────────
    let sorted_positive: Vec<&Atom> = if crate::DATALOG_COST_REORDER.get() {
        cost_order_atoms(rule)
    } else {
        rule.body
            .iter()
            .filter_map(|lit| {
                if let BodyLiteral::Positive(a) = lit {
                    Some(a)
                } else {
                    None
                }
            })
            .collect()
    };

    // ── Step 1b: Collect temporal filters (v0.106.0 + v0.107.0) ─────────────
    // Build a combined SQL WHERE fragment from all temporal filter literals.
    // This fragment is appended to the table expression for any atom whose
    // predicate is registered as temporal.
    //
    // v0.107.0 sequential operators (WITHIN, SEQUENCE, CONSECUTIVE) compile to
    // standalone WHERE EXISTS / NOT EXISTS subqueries that are appended to
    // `where_clauses` after the FROM clause is assembled.
    let temporal_filter_sql: Option<String> = {
        use crate::datalog::TemporalFilter;
        let mut parts: Vec<String> = Vec::new();
        for lit in &rule.body {
            if let BodyLiteral::TemporalFilter(tf) = lit {
                let s = match tf {
                    TemporalFilter::After(ts) => {
                        format!("valid_from > {ts}::timestamptz")
                    }
                    TemporalFilter::Before(ts) => {
                        format!("valid_from < {ts}::timestamptz")
                    }
                    TemporalFilter::During(from_ts, to_ts) => {
                        format!(
                            "tstzrange(valid_from, valid_to, '[)') && \
                             tstzrange({from_ts}::timestamptz, {to_ts}::timestamptz, '[)')"
                        )
                    }
                    // v0.107.0 operators are collected as whole-query WHERE subqueries;
                    // skip them here (handled after step 3 below).
                    TemporalFilter::Within(..)
                    | TemporalFilter::Sequence(..)
                    | TemporalFilter::Consecutive(..) => continue,
                    // v0.118.0 Allen's interval relations — compile to timestamptz comparisons.
                    // These do not filter the temporal_facts table directly; they compare
                    // timestamp arguments provided as rule body constants or variables.
                    TemporalFilter::AllenBefore(a_start, a_end, b_start, _b_end) => {
                        format!(
                            "({a_start}::timestamptz < {b_start}::timestamptz \
                             AND {a_end}::timestamptz <= {b_start}::timestamptz)"
                        )
                    }
                    TemporalFilter::AllenMeets(_a_start, a_end, b_start, _b_end) => {
                        format!("{a_end}::timestamptz = {b_start}::timestamptz")
                    }
                    TemporalFilter::AllenOverlaps(a_start, a_end, b_start, b_end) => {
                        format!(
                            "({a_start}::timestamptz < {b_start}::timestamptz \
                             AND {a_end}::timestamptz > {b_start}::timestamptz \
                             AND {a_end}::timestamptz < {b_end}::timestamptz)"
                        )
                    }
                    TemporalFilter::AllenDuring(a_start, a_end, b_start, b_end) => {
                        format!(
                            "({a_start}::timestamptz > {b_start}::timestamptz \
                             AND {a_end}::timestamptz < {b_end}::timestamptz)"
                        )
                    }
                    TemporalFilter::AllenFinishes(a_start, a_end, b_start, b_end) => {
                        format!(
                            "({a_start}::timestamptz > {b_start}::timestamptz \
                             AND {a_end}::timestamptz = {b_end}::timestamptz)"
                        )
                    }
                    TemporalFilter::AllenStarts(a_start, a_end, b_start, b_end) => {
                        format!(
                            "({a_start}::timestamptz = {b_start}::timestamptz \
                             AND {a_end}::timestamptz < {b_end}::timestamptz)"
                        )
                    }
                    TemporalFilter::AllenEquals(a_start, a_end, b_start, b_end) => {
                        format!(
                            "({a_start}::timestamptz = {b_start}::timestamptz \
                             AND {a_end}::timestamptz = {b_end}::timestamptz)"
                        )
                    }
                };
                parts.push(s);
            }
        }
        if parts.is_empty() {
            None
        } else {
            Some(parts.join(" AND "))
        }
    };

    // ── Step 2: Collect guards for filter-pushdown (v0.29.0) ──────────────────
    // Guards that are not yet pushed will remain as WHERE conditions.
    let all_guards: Vec<&BodyLiteral> = rule
        .body
        .iter()
        .filter(|lit| {
            matches!(
                lit,
                BodyLiteral::Compare(..) | BodyLiteral::StringBuiltin(_)
            )
        })
        .collect();
    let mut pushed_guards: std::collections::HashSet<usize> = std::collections::HashSet::new();

    let mut from_clauses: Vec<String> = Vec::new();
    let mut where_clauses: Vec<String> = Vec::new();
    let mut var_map = VarMap::default();
    let mut antijoin_idx = 0usize;

    // ── Step 3: Process positive atoms with pushdown ───────────────────────────
    for (atom_idx, atom) in sorted_positive.iter().enumerate() {
        let alias = format!("t{atom_idx}");
        let pred_id = match &atom.p {
            Term::Const(id) => *id,
            Term::Var(_) => {
                return Err("variable predicate in rule body not supported".to_owned());
            }
            _ => return Err("invalid predicate term in rule body".to_owned()),
        };

        // Bind variables.
        if let Term::Var(v) = &atom.s {
            var_map.bind(v, &alias, "s");
        } else if let Term::Const(c) = &atom.s {
            where_clauses.push(format!("{alias}.s = {}", const_sql(*c)));
        }
        if let Term::Var(v) = &atom.o {
            var_map.bind(v, &alias, "o");
        } else if let Term::Const(c) = &atom.o {
            where_clauses.push(format!("{alias}.o = {}", const_sql(*c)));
        }
        if let Term::Var(v) = &atom.g {
            var_map.bind(v, &alias, "g");
        } else if let Term::Const(c) = &atom.g {
            where_clauses.push(format!("{alias}.g = {}", const_sql(*c)));
        } else {
            // DefaultGraph: g = 0 only when rule_graph_scope = 'default' (not the default)
            let scope = crate::RULE_GRAPH_SCOPE
                .get()
                .as_ref()
                .and_then(|c| c.to_str().ok())
                .unwrap_or("all")
                .to_owned();
            if scope == "default" {
                where_clauses.push(format!("{alias}.g = 0"));
            }
        }

        // After binding, check which guards can be pushed into this JOIN ON.
        let mut pushdown_conds: Vec<String> = Vec::new();
        for (gi, guard) in all_guards.iter().enumerate() {
            if !pushed_guards.contains(&gi)
                && guard_fully_bound(guard, &var_map)
                && let Some(cond_sql) = compile_guard_sql(guard, &var_map)
            {
                pushdown_conds.push(cond_sql);
                pushed_guards.insert(gi);
            }
        }

        if atom_idx == 0 {
            // Choose between VP table and temporal_facts routing.
            let table_expr = if let Some(ref tf_sql) = temporal_filter_sql
                && crate::temporal::is_temporal_predicate(pred_id)
            {
                crate::temporal::temporal_read_expr_filtered(pred_id, tf_sql)
            } else {
                vp_read_expr(pred_id)
            };
            from_clauses.push(format!("{table_expr} {alias}"));
            // Pushdown conditions on the first atom go to WHERE (no JOIN ON).
            where_clauses.extend(pushdown_conds);
        } else {
            let mut join_cond = build_join_cond(&alias, atom, &var_map);
            if !pushdown_conds.is_empty() {
                if join_cond.is_empty() {
                    join_cond = pushdown_conds.join(" AND ");
                } else {
                    join_cond = format!("{join_cond} AND {}", pushdown_conds.join(" AND "));
                }
            }
            // Choose between VP table and temporal_facts routing.
            let table_expr = if let Some(ref tf_sql) = temporal_filter_sql
                && crate::temporal::is_temporal_predicate(pred_id)
            {
                crate::temporal::temporal_read_expr_filtered(pred_id, tf_sql)
            } else {
                vp_read_expr(pred_id)
            };
            if join_cond.is_empty() {
                from_clauses.push(format!("{table_expr} {alias}"));
            } else {
                from_clauses.push(format!("JOIN {table_expr} {alias} ON {join_cond}"));
            }
        }
    }

    // ── Step 4: Process negated atoms (anti-join or NOT EXISTS) ───────────────
    for lit in &rule.body {
        if let BodyLiteral::Negated(atom) = lit {
            let pred_id = match &atom.p {
                Term::Const(id) => *id,
                _ => return Err("variable predicate in NOT atom not supported".to_owned()),
            };

            let threshold = crate::DATALOG_ANTIJOIN_THRESHOLD.get() as i64;
            let row_count = if threshold > 0 {
                estimate_pred_cardinality(pred_id)
            } else {
                0
            };

            if threshold > 0 && row_count >= threshold {
                // ── Anti-join form (v0.29.0): LEFT JOIN … IS NULL ────────────
                let aj_alias = format!("aj{antijoin_idx}");
                antijoin_idx += 1;
                let on_cond = build_antijoin_on_cond(&aj_alias, atom, &var_map);
                from_clauses.push(format!(
                    "LEFT JOIN {} {aj_alias} ON {on_cond}",
                    vp_read_expr(pred_id)
                ));
                where_clauses.push(format!("{aj_alias}.s IS NULL"));
            } else {
                // ── NOT EXISTS form (original behavior) ──────────────────────
                let inner_conds = build_not_exists_conds(atom, &var_map);
                let cond_str = if inner_conds.is_empty() {
                    "TRUE".to_owned()
                } else {
                    inner_conds.join(" AND ")
                };
                where_clauses.push(format!(
                    "NOT EXISTS (SELECT 1 FROM {} WHERE {cond_str})",
                    vp_read_expr(pred_id)
                ));
            }
        }
    }

    // ── Step 5: Process remaining guards (not pushed down) ────────────────────
    for (gi, lit) in all_guards.iter().enumerate() {
        if pushed_guards.contains(&gi) {
            continue; // already handled in pushdown
        }
        if let Some(cond_sql) = compile_guard_sql(lit, &var_map) {
            where_clauses.push(cond_sql);
        }
    }

    // ── Step 5b: Process v0.107.0 sequential temporal operators ──────────────
    // WITHIN, SEQUENCE, and CONSECUTIVE compile to standalone EXISTS subqueries.
    {
        use crate::datalog::TemporalFilter;
        for lit in &rule.body {
            if let BodyLiteral::TemporalFilter(tf) = lit {
                match tf {
                    TemporalFilter::Within(duration) => {
                        // EXISTS (SELECT 1 FROM temporal_facts WHERE s = <s_ref>
                        //         AND p = <p_id> AND valid_from >= now() - interval)
                        // We use the first bound subject variable from var_map as the
                        // subject reference (best-effort; the rule body is responsible
                        // for binding a subject via a positive atom).
                        let interval_expr = format!("INTERVAL '{duration}'");
                        let subq = format!(
                            "EXISTS (SELECT 1 FROM _pg_ripple.temporal_facts \
                             WHERE valid_from >= (now() - {interval_expr}))"
                        );
                        where_clauses.push(subq);
                    }
                    TemporalFilter::Sequence(s1, p1, o1, s2, p2, o2, window) => {
                        // Correlated subquery: event1 strictly before event2 within window.
                        // Resolve predicate IRIs to dictionary IDs.
                        let p1_id = resolve_predicate_id(p1);
                        let p2_id = resolve_predicate_id(p2);
                        let s1_ref = resolve_term_ref(s1, &var_map);
                        let o1_ref = resolve_term_ref(o1, &var_map);
                        let s2_ref = resolve_term_ref(s2, &var_map);
                        let o2_ref = resolve_term_ref(o2, &var_map);
                        let interval_expr = format!("INTERVAL '{window}'");
                        let subq = format!(
                            "EXISTS (\
                               SELECT 1 \
                               FROM _pg_ripple.temporal_facts e1 \
                               JOIN _pg_ripple.temporal_facts e2 \
                                 ON e1.s = e2.s \
                               WHERE e1.p = {p1_id} AND e2.p = {p2_id} \
                                 AND {s1_ref} AND {s2_ref} \
                                 AND {o1_ref} AND {o2_ref} \
                                 AND e1.valid_from < e2.valid_from \
                                 AND e2.valid_from - e1.valid_from <= {interval_expr}\
                             )"
                        );
                        where_clauses.push(subq);
                    }
                    TemporalFilter::Consecutive(n, pred, window) => {
                        // ROW_NUMBER() OVER (PARTITION BY s, p ORDER BY valid_from)
                        // with HAVING COUNT(*) >= n within the time window.
                        let p_id = resolve_predicate_id(pred);
                        let interval_expr = format!("INTERVAL '{window}'");
                        let subq = format!(
                            "EXISTS (\
                               SELECT 1 \
                               FROM (\
                                 SELECT s, \
                                        ROW_NUMBER() OVER (PARTITION BY s ORDER BY valid_from) AS rn, \
                                        valid_from, \
                                        MIN(valid_from) OVER (PARTITION BY s) AS first_vf \
                                 FROM _pg_ripple.temporal_facts \
                                 WHERE p = {p_id} \
                               ) ranked \
                               WHERE rn = {n} \
                                 AND valid_from - first_vf <= {interval_expr}\
                             )"
                        );
                        where_clauses.push(subq);
                    }
                    _ => {} // AFTER/BEFORE/DURING already handled above
                }
            }
        }
    }

    // ── Step 6: Process Assign literals (always in WHERE — they mutate var_map) ─
    for lit in &rule.body {
        if let BodyLiteral::Assign(var, lhs, op, rhs) = lit {
            // M-1: wrap divisor with NULLIF to prevent division-by-zero.
            let l = render_comparison_term(lhs, &var_map);
            let r_raw = render_comparison_term(rhs, &var_map);
            let r = if matches!(op, crate::datalog::ArithOp::Div) {
                format!("NULLIF({r_raw}, 0)")
            } else {
                r_raw
            };
            let op_str = arith_op_sql(op);
            let col_expr = format!("({l} {op_str} {r})");
            var_map.bind(var, &col_expr, "");
        }
    }

    // M-2: Compile-time check for unbound variables in comparisons and assigns.
    let head_text = rule
        .rule_text
        .lines()
        .next()
        .unwrap_or(&rule.rule_text)
        .trim();
    for lit in &rule.body {
        match lit {
            BodyLiteral::Compare(lhs, _, rhs) => {
                for term in [lhs, rhs] {
                    if let crate::datalog::Term::Var(v) = term
                        && var_map.col_ref(v).is_none()
                    {
                        return Err(format!(
                            "unbound variable ?{v} in comparison in rule '{head_text}': \
                             every variable in a comparison must be bound by a positive body literal"
                        ));
                    }
                }
            }
            BodyLiteral::Assign(var, lhs, _, rhs) => {
                for term in [lhs, rhs] {
                    if let crate::datalog::Term::Var(v) = term
                        && var_map.col_ref(v).is_none()
                        && v != var
                    {
                        return Err(format!(
                            "unbound variable ?{v} in assignment in rule '{head_text}': \
                             every variable in an arithmetic expression must be bound by a positive body literal"
                        ));
                    }
                }
            }
            _ => {}
        }
    }

    // Build SELECT columns: head s and o.
    let select_s = match &head.s {
        Term::Var(v) => var_map
            .col_ref(v)
            .ok_or_else(|| format!("unbound variable ?{v} in head"))?,
        Term::Const(id) => const_sql(*id),
        Term::Wildcard => return Err("wildcard in head not allowed".to_owned()),
        Term::DefaultGraph => "0".to_owned(),
    };
    let select_o = match &head.o {
        Term::Var(v) => var_map
            .col_ref(v)
            .ok_or_else(|| format!("unbound variable ?{v} in head"))?,
        Term::Const(id) => const_sql(*id),
        Term::Wildcard => return Err("wildcard in head not allowed".to_owned()),
        Term::DefaultGraph => "0".to_owned(),
    };
    let select_g = super::helpers::head_g_select_expr(head, &var_map)?;

    let from_str = from_clauses.join("\n");
    let where_str = if where_clauses.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", where_clauses.join("\n  AND "))
    };

    // VAL-207: plain-path INSERTs materialise rows as inferred (source = 1),
    // matching the semi-naive fixpoint (CONFLICT-03) so conflict detection,
    // DRed retraction and justification read one provenance class.
    let (head_cols, head_source) = if mark_inferred {
        ("s, o, g, source", ", 1")
    } else {
        ("s, o, g", "")
    };

    Ok(format!(
        "INSERT INTO {target} ({head_cols})\n\
         SELECT {select_s}, {select_o}, {select_g}{head_source}\n\
         FROM {from_str}\n\
         {where_str}\n\
         ON CONFLICT DO NOTHING"
    ))
}
