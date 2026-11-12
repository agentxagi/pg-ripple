//! pg_tests for the recursive-rule SQL in `sql.rs`.
//!
//! `get_on_demand_cte` has no SQL entry point, so the on-demand CTE of a
//! recursive rule is exercised here; the materialising paths are covered by
//! `tests/pg_regress/sql/v0147d_datalog_closure_cyclic.sql`.

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
// A16-CQ: test helper — unwrap/expect are acceptable in test-only code.
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use pgrx::prelude::*;

    use crate::datalog::{Term, parse_rules};

    /// The on-demand CTE of a recursive rule is the closure as a set: no CYCLE
    /// clause, one row per (s, o, g), and it terminates on a cyclic graph whose
    /// simple paths number in the millions.
    #[pg_test]
    fn test_on_demand_cte_is_set_closure_on_cyclic_graph() {
        // Ring of 30 nodes with edges i->i+1 and i->i+2: strongly connected,
        // so the closure is all 30 x 30 pairs.
        let mut nquads = String::new();
        for i in 0..30 {
            for step in [1, 2] {
                nquads.push_str(&format!(
                    "<https://odcte.test/n{i}> <https://odcte.test/dep> \
                     <https://odcte.test/n{}> <https://odcte.test/g> .\n",
                    (i + step) % 30
                ));
            }
        }
        let loaded = Spi::get_one_with_args::<i64>(
            "SELECT pg_ripple.load_nquads($1)",
            &[pgrx::datum::DatumWithOid::from(nquads.as_str())],
        )
        .unwrap();
        assert_eq!(loaded, Some(60));

        let rules = parse_rules(
            "GRAPH ?g { ?x <https://odcte.test/dep> ?z } :- \
               GRAPH ?g { ?x <https://odcte.test/dep> ?y }, \
               GRAPH ?g { ?y <https://odcte.test/dep> ?z } .",
            "odcte",
        )
        .unwrap()
        .rules;
        let pred_id = match rules[0].head.as_ref().map(|h| &h.p) {
            Some(Term::Const(id)) => *id,
            other => panic!("rule head without a constant predicate: {other:?}"),
        };

        let cte = crate::datalog::compiler::compile_on_demand_cte(&rules, pred_id).unwrap();
        assert!(
            !cte.contains("CYCLE"),
            "on-demand CTE still has a CYCLE clause: {cte}"
        );

        // The pg_test already runs inside one statement: a statement_timeout set
        // here would not apply to it, a temp_file_limit does (path enumeration
        // spills to temp files).
        Spi::run("SET LOCAL temp_file_limit = '256MB'").unwrap();
        let (rows, pairs) = Spi::get_two::<i64, i64>(&format!(
            "{cte} SELECT count(*), count(DISTINCT (s, o, g)) FROM derived_{pred_id}"
        ))
        .unwrap();
        assert_eq!((rows, pairs), (Some(900), Some(900)));
    }
}
