//! Unit tests for the Datalog stratifier and subsumption checks
//! (extracted from stratify.rs, VAL-350 file-size gate Q13-04).

use super::*;
use crate::datalog::{Atom, BodyLiteral, Rule, Term};

fn make_rule(head_p: i64, body_p: i64, negated: bool) -> Rule {
    Rule {
        head: Some(Atom {
            s: Term::Var("x".to_owned()),
            p: Term::Const(head_p),
            o: Term::Var("y".to_owned()),
            g: Term::DefaultGraph,
        }),
        body: vec![if negated {
            BodyLiteral::Negated(Atom {
                s: Term::Var("x".to_owned()),
                p: Term::Const(body_p),
                o: Term::Var("y".to_owned()),
                g: Term::DefaultGraph,
            })
        } else {
            BodyLiteral::Positive(Atom {
                s: Term::Var("x".to_owned()),
                p: Term::Const(body_p),
                o: Term::Var("y".to_owned()),
                g: Term::DefaultGraph,
            })
        }],
        rule_text: String::new(),
        name: None,
        weight: None,
    }
}

#[test]
fn test_stratify_simple() {
    let rules = vec![make_rule(10, 20, false), make_rule(30, 10, false)];
    let result = stratify(&rules).unwrap();
    assert!(!result.strata.is_empty());
}

#[test]
fn test_stratify_negation_ok() {
    // 10 depends negatively on 20 — OK as long as 20 is base data.
    let rules = vec![make_rule(10, 20, true)];
    let result = stratify(&rules).unwrap();
    assert!(!result.strata.is_empty());
}

#[test]
fn test_stratify_negation_cycle_error() {
    // 10 ← ¬10 is unstratifiable.
    let rules = vec![make_rule(10, 10, true)];
    let result = stratify(&rules);
    assert!(result.is_err(), "expected unstratifiable error");
}

#[test]
fn test_stratify_recursive() {
    // 10 ← 10 (positive self-loop — recursive)
    let rules = vec![make_rule(10, 10, false)];
    let result = stratify(&rules).unwrap();
    let has_recursive = result.strata.iter().any(|s| s.is_recursive);
    assert!(has_recursive);
}

// ─── check_subsumption (constants-aware, v0.129.0) ──────────────────────

use crate::datalog::Term::{Const, Var};

/// Head atom `?s <head_p> ?o` in the default graph.
fn head_atom(p: i64, s: Term, o: Term) -> Atom {
    Atom {
        s,
        p: Term::Const(p),
        o,
        g: Term::DefaultGraph,
    }
}

/// Positive body atom `?s <p> ?o` in the default graph.
fn pos_atom(s: Term, p: i64, o: Term) -> BodyLiteral {
    BodyLiteral::Positive(Atom {
        s,
        p: Term::Const(p),
        o,
        g: Term::DefaultGraph,
    })
}

fn lit_rule(head: Atom, body: Vec<BodyLiteral>, text: &str, weight: Option<f64>) -> Rule {
    Rule {
        head: Some(head),
        body,
        rule_text: text.to_owned(),
        name: None,
        weight,
    }
}

#[test]
fn test_subsumption_keeps_rules_differing_only_in_body_constants() {
    // Same head, same body predicate, different constants — neither rule
    // may be eliminated.  v0.128.0 deduplicated them, silently dropping
    // one side of a head conflict before rule_conflicts could see it.
    let rules = vec![
        lit_rule(
            head_atom(10, Var("x".to_owned()), Var("y".to_owned())),
            vec![pos_atom(Var("x".to_owned()), 20, Const(300))],
            "r1",
            None,
        ),
        lit_rule(
            head_atom(10, Var("x".to_owned()), Var("y".to_owned())),
            vec![pos_atom(Var("x".to_owned()), 20, Const(301))],
            "r2",
            None,
        ),
    ];
    assert!(check_subsumption(&rules).is_empty());
}

#[test]
fn test_subsumption_keeps_rules_differing_only_in_head_constants() {
    let rules = vec![
        lit_rule(
            head_atom(10, Var("x".to_owned()), Const(100)),
            vec![pos_atom(Var("x".to_owned()), 20, Var("y".to_owned()))],
            "r1",
            None,
        ),
        lit_rule(
            head_atom(10, Var("x".to_owned()), Const(101)),
            vec![pos_atom(Var("x".to_owned()), 20, Var("y".to_owned()))],
            "r2",
            None,
        ),
    ];
    assert!(check_subsumption(&rules).is_empty());
}

#[test]
fn test_subsumption_dedupes_rules_identical_up_to_renaming() {
    let rules = vec![
        lit_rule(
            head_atom(10, Var("x".to_owned()), Var("y".to_owned())),
            vec![pos_atom(Var("x".to_owned()), 20, Var("y".to_owned()))],
            "r1",
            None,
        ),
        lit_rule(
            head_atom(10, Var("a".to_owned()), Var("b".to_owned())),
            vec![pos_atom(Var("a".to_owned()), 20, Var("b".to_owned()))],
            "r2",
            None,
        ),
    ];
    assert_eq!(check_subsumption(&rules), vec!["r2".to_owned()]);
}

#[test]
fn test_subsumption_detects_genuine_subset() {
    // R1: p(?x,?y) :- q(?x,?y).  R2: same with an extra body atom.
    let rules = vec![
        lit_rule(
            head_atom(10, Var("x".to_owned()), Var("y".to_owned())),
            vec![pos_atom(Var("x".to_owned()), 20, Var("y".to_owned()))],
            "r1",
            None,
        ),
        lit_rule(
            head_atom(10, Var("x".to_owned()), Var("y".to_owned())),
            vec![
                pos_atom(Var("x".to_owned()), 20, Var("y".to_owned())),
                pos_atom(Var("x".to_owned()), 30, Var("y".to_owned())),
            ],
            "r2",
            None,
        ),
    ];
    assert_eq!(check_subsumption(&rules), vec!["r2".to_owned()]);
}

#[test]
fn test_subsumption_binds_constants_through_substitution() {
    // R1 derives p(?x,?y) for every q pair; R2 derives the single p(a,b),
    // so R1 subsumes R2 via σ(?x)=a, σ(?y)=b.
    let rules = vec![
        lit_rule(
            head_atom(10, Var("x".to_owned()), Var("y".to_owned())),
            vec![pos_atom(Var("x".to_owned()), 20, Var("y".to_owned()))],
            "r1",
            None,
        ),
        lit_rule(
            head_atom(10, Const(100), Const(200)),
            vec![
                pos_atom(Const(100), 20, Const(200)),
                pos_atom(Const(100), 30, Var("z".to_owned())),
            ],
            "r2",
            None,
        ),
    ];
    assert_eq!(check_subsumption(&rules), vec!["r2".to_owned()]);
}

#[test]
fn test_subsumption_respects_variable_joins() {
    // R1 requires the two q arguments to unify (?x ?x); R2's q(?x,?y)
    // admits distinct values, so R1 does not subsume R2.
    let rules = vec![
        lit_rule(
            head_atom(10, Var("x".to_owned()), Var("y".to_owned())),
            vec![pos_atom(Var("x".to_owned()), 20, Var("x".to_owned()))],
            "r1",
            None,
        ),
        lit_rule(
            head_atom(10, Var("x".to_owned()), Var("y".to_owned())),
            vec![
                pos_atom(Var("x".to_owned()), 20, Var("y".to_owned())),
                pos_atom(Var("x".to_owned()), 30, Var("y".to_owned())),
            ],
            "r2",
            None,
        ),
    ];
    assert!(check_subsumption(&rules).is_empty());
}

#[test]
fn test_subsumption_skips_negated_bodies() {
    // R2's negation makes it strictly weaker than R1 — not a duplicate.
    let rules = vec![
        lit_rule(
            head_atom(10, Var("x".to_owned()), Var("y".to_owned())),
            vec![pos_atom(Var("x".to_owned()), 20, Var("y".to_owned()))],
            "r1",
            None,
        ),
        Rule {
            head: Some(head_atom(10, Var("x".to_owned()), Var("y".to_owned()))),
            body: vec![
                pos_atom(Var("x".to_owned()), 20, Var("y".to_owned())),
                BodyLiteral::Negated(Atom {
                    s: Var("x".to_owned()),
                    p: Term::Const(30),
                    o: Var("y".to_owned()),
                    g: Term::DefaultGraph,
                }),
            ],
            rule_text: "r2".to_owned(),
            name: None,
            weight: None,
        },
    ];
    assert!(check_subsumption(&rules).is_empty());
}

#[test]
fn test_subsumption_weighted_rules() {
    let mk = |text: &str, weight: Option<f64>| {
        lit_rule(
            head_atom(10, Var("x".to_owned()), Var("y".to_owned())),
            vec![pos_atom(Var("x".to_owned()), 20, Var("y".to_owned()))],
            text,
            weight,
        )
    };
    // Identical form, different weights: kept (probabilities differ).
    assert!(check_subsumption(&[mk("r1", None), mk("r2", Some(0.5))]).is_empty());
    // Identical form, same weight: deduplicated.
    assert_eq!(
        check_subsumption(&[mk("r1", Some(0.5)), mk("r2", Some(0.5))]),
        vec!["r2".to_owned()]
    );
    // Weighted rule is never subsumption-eliminated.
    let general = mk("r1", Some(0.5));
    let specific = lit_rule(
        head_atom(10, Var("x".to_owned()), Var("y".to_owned())),
        vec![
            pos_atom(Var("x".to_owned()), 20, Var("y".to_owned())),
            pos_atom(Var("x".to_owned()), 30, Var("y".to_owned())),
        ],
        "r2",
        None,
    );
    assert_eq!(
        check_subsumption(&[general, specific]),
        Vec::<String>::new()
    );
}
