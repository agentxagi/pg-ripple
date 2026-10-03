//! Variable-predicate rule instantiation (v0.44.0).
//!
//! Split out of `seminaive.rs` (VAL-350, file-size gate Q13-04): rules whose
//! predicate position is a variable are instantiated at runtime against the
//! predicate catalog and compiled per binding.

use pgrx::prelude::*;

use crate::datalog::{BodyLiteral, Rule, Term, compile_rule_set, vp_read_expr_pub};

// ─── Variable-predicate rule instantiation (v0.44.0) ─────────────────────────

fn collect_pred_vars(rule: &Rule) -> Vec<String> {
    let mut vars: Vec<String> = Vec::new();
    if let Some(Term::Var(v)) = rule.head.as_ref().map(|h| &h.p)
        && !vars.contains(v)
    {
        vars.push(v.clone());
    }
    for lit in &rule.body {
        let atom = match lit {
            BodyLiteral::Positive(a) | BodyLiteral::Negated(a) => a,
            _ => continue,
        };
        if let Term::Var(v) = &atom.p
            && !vars.contains(v)
        {
            vars.push(v.clone());
        }
    }
    vars
}

fn substitute_pred_var(rule: &Rule, var_name: &str, pred_id: i64) -> Rule {
    let sub = |t: &Term| -> Term {
        match t {
            Term::Var(v) if v == var_name => Term::Const(pred_id),
            other => other.clone(),
        }
    };
    let sub_atom = |a: &crate::datalog::Atom| -> crate::datalog::Atom {
        crate::datalog::Atom {
            s: sub(&a.s),
            p: sub(&a.p),
            o: sub(&a.o),
            g: sub(&a.g),
        }
    };
    let new_head = rule.head.as_ref().map(sub_atom);
    let new_body = rule
        .body
        .iter()
        .map(|lit| match lit {
            BodyLiteral::Positive(a) => BodyLiteral::Positive(sub_atom(a)),
            BodyLiteral::Negated(a) => BodyLiteral::Negated(sub_atom(a)),
            other => other.clone(),
        })
        .collect();
    Rule {
        head: new_head,
        body: new_body,
        rule_text: format!("/* {var_name}={pred_id} */ {}", rule.rule_text),
        name: rule.name.clone(),
        weight: rule.weight,
    }
}

fn enumerate_pred_var_values(rule: &Rule, var_name: &str) -> Vec<i64> {
    let mut values: std::collections::HashSet<i64> = std::collections::HashSet::new();
    for lit in &rule.body {
        let atom = match lit {
            BodyLiteral::Positive(a) => a,
            _ => continue,
        };
        let atom_pred_id = match &atom.p {
            Term::Const(id) => *id,
            _ => continue,
        };
        let is_subj = matches!(&atom.s, Term::Var(v) if v == var_name);
        let is_obj = matches!(&atom.o, Term::Var(v) if v == var_name);
        if is_subj {
            let sql = match &atom.o {
                Term::Const(o_id) => format!(
                    "SELECT DISTINCT s FROM {} WHERE o = {o_id}",
                    vp_read_expr_pub(atom_pred_id)
                ),
                _ => format!("SELECT DISTINCT s FROM {}", vp_read_expr_pub(atom_pred_id)),
            };
            let ids: Vec<i64> = Spi::connect(|c| {
                c.select(&sql, None, &[])
                    .ok()
                    .map(|rows| {
                        rows.filter_map(|row| row.get::<i64>(1).ok().flatten())
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default()
            });
            values.extend(ids);
        } else if is_obj {
            let sql = match &atom.s {
                Term::Const(s_id) => format!(
                    "SELECT DISTINCT o FROM {} WHERE s = {s_id}",
                    vp_read_expr_pub(atom_pred_id)
                ),
                _ => format!("SELECT DISTINCT o FROM {}", vp_read_expr_pub(atom_pred_id)),
            };
            let ids: Vec<i64> = Spi::connect(|c| {
                c.select(&sql, None, &[])
                    .ok()
                    .map(|rows| {
                        rows.filter_map(|row| row.get::<i64>(1).ok().flatten())
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default()
            });
            values.extend(ids);
        }
    }
    values.into_iter().collect()
}

fn compute_pred_var_bindings(rule: &Rule, pred_vars: &[String]) -> Vec<Vec<(String, i64)>> {
    if pred_vars.is_empty() {
        return vec![vec![]];
    }

    for lit in &rule.body {
        let atom = match lit {
            BodyLiteral::Positive(a) => a,
            _ => continue,
        };
        let atom_pred_id = match &atom.p {
            Term::Const(id) => *id,
            _ => continue,
        };
        let subj_var = match &atom.s {
            Term::Var(v) if pred_vars.contains(v) => Some(v.clone()),
            _ => None,
        };
        let obj_var = match &atom.o {
            Term::Var(v) if pred_vars.contains(v) => Some(v.clone()),
            _ => None,
        };
        if let (Some(sv), Some(ov)) = (subj_var, obj_var) {
            let sql = format!(
                "SELECT DISTINCT s, o FROM {}",
                vp_read_expr_pub(atom_pred_id)
            );
            let pairs: Vec<(i64, i64)> = Spi::connect(|c| {
                c.select(&sql, None, &[])
                    .ok()
                    .map(|rows| {
                        rows.filter_map(|row| {
                            let s = row.get::<i64>(1).ok().flatten()?;
                            let o = row.get::<i64>(2).ok().flatten()?;
                            Some((s, o))
                        })
                        .collect::<Vec<_>>()
                    })
                    .unwrap_or_default()
            });
            return pairs
                .into_iter()
                .map(|(s, o)| vec![(sv.clone(), s), (ov.clone(), o)])
                .collect();
        }
    }

    let mut per_var: Vec<(String, Vec<i64>)> = Vec::new();
    for var_name in pred_vars {
        let vals = enumerate_pred_var_values(rule, var_name);
        if vals.is_empty() {
            return vec![];
        }
        per_var.push((var_name.clone(), vals));
    }

    let mut result: Vec<Vec<(String, i64)>> = vec![vec![]];
    for (var_name, values) in &per_var {
        let mut new_result = Vec::new();
        for partial in &result {
            for &val in values {
                let mut extended = partial.clone();
                extended.push((var_name.clone(), val));
                new_result.push(extended);
            }
        }
        result = new_result;
    }
    result
}

/// Handle a rule with variable predicates by instantiating at runtime.
pub(super) fn run_var_pred_rule(rule: &Rule) -> i64 {
    let pred_vars = collect_pred_vars(rule);
    if pred_vars.is_empty() {
        return 0;
    }
    let bindings = compute_pred_var_bindings(rule, &pred_vars);
    if bindings.is_empty() {
        return 0;
    }
    let mut total = 0i64;
    for binding in bindings {
        let mut specialized = rule.clone();
        for (var_name, pred_id) in &binding {
            specialized = substitute_pred_var(&specialized, var_name, *pred_id);
        }
        match compile_rule_set(std::slice::from_ref(&specialized)) {
            Ok(sqls) => {
                for sql in &sqls {
                    match Spi::run_with_args(sql, &[]) {
                        Ok(()) => total += 1,
                        Err(e) => pgrx::warning!("var_pred_rule SQL error: {e}"),
                    }
                }
            }
            Err(e) => pgrx::warning!("var_pred_rule compile error after instantiation: {e}"),
        }
    }
    total
}
