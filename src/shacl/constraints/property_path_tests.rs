//! pg_tests for SHACL `sh:oneOrMorePath` / `sh:zeroOrMorePath` SQL.
//!
//! `traverse_sh_path` has no SQL entry point yet, so the recursive paths are
//! exercised here.

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
// A16-CQ: test helper — unwrap/expect are acceptable in test-only code.
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use pgrx::prelude::*;

    use super::super::{ShPath, traverse_sh_path};

    /// On a ring of 40 nodes with edges i->i+1 and i->i+2 every node reaches
    /// every node, through more simple paths than any timeout allows to list:
    /// the recursion must return each reachable node once and stop.
    #[pg_test]
    fn test_recursive_sh_paths_are_sets_on_cyclic_graph() {
        let mut nt = String::new();
        for i in 0..40 {
            for step in [1, 2] {
                nt.push_str(&format!(
                    "<https://shp.test/n{i}> <https://shp.test/dep> <https://shp.test/n{}> .\n",
                    (i + step) % 40
                ));
            }
        }
        Spi::get_one_with_args::<i64>(
            "SELECT pg_ripple.load_ntriples($1)",
            &[pgrx::datum::DatumWithOid::from(nt.as_str())],
        )
        .unwrap();
        let n0 = crate::dictionary::lookup_iri("https://shp.test/n0").unwrap();
        let dep = || Box::new(ShPath::Predicate("https://shp.test/dep".to_owned()));

        // The pg_test already runs inside one statement: a statement_timeout set
        // here would not apply to it, a temp_file_limit does (path enumeration
        // spills to temp files).
        Spi::run("SET LOCAL temp_file_limit = '256MB'").unwrap();
        for path in [ShPath::OneOrMore(dep()), ShPath::ZeroOrMore(dep())] {
            let reached = traverse_sh_path(&path, n0, 0);
            let mut distinct = reached.clone();
            distinct.sort_unstable();
            distinct.dedup();
            assert_eq!(distinct.len(), 40, "{path:?}");
            assert_eq!(reached.len(), 40, "duplicates for {path:?}");
        }
    }
}
