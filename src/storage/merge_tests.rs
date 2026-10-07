//! pg_tests for the HTAP merge (extracted from merge.rs, MERGE-RACE-01).
//!
//! Inline `#[pg_schema] mod tests` (same shape as `src/datalog/parser_tests.rs`)
//! so the `#[pg_test]` functions are emitted into the `tests` SQL schema.

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
// A16-CQ: test helper — unwrap/expect are acceptable in test-only code.
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use pgrx::prelude::*;

    // MERGE-RACE-01: the hook of merge_predicate_with_hook runs after main_new
    // is built and before the swap phase — exactly where concurrent writers
    // used to commit rows that the cleanup then destroyed.  Writing from the
    // hook in the same transaction is visible to every later merge statement
    // (READ COMMITTED: fresh snapshot per statement), which is what a
    // concurrent commit looks like to the old `TRUNCATE {delta}`.

    const S: &str = "<https://merge-race.test/s>";

    /// A predicate promoted to its HTAP split, holding one triple (s, p, o0).
    fn promoted_predicate(p: &str) -> i64 {
        crate::storage::insert_triple(S, p, "<https://merge-race.test/o0>", 0);
        let p_id = crate::dictionary::encode(
            crate::storage::strip_angle_brackets_pub(p),
            crate::dictionary::KIND_IRI,
        );
        crate::storage::promote::promote_predicate_pub(p_id);
        assert!(
            super::super::is_htap(p_id),
            "predicate must be HTAP after promotion"
        );
        p_id
    }

    fn visible(p_id: i64, o: &str) -> i64 {
        let o_id = crate::storage::encode_rdf_term(o);
        Spi::get_one_with_args::<i64>(
            &format!("SELECT count(*)::bigint FROM _pg_ripple.vp_{p_id} WHERE o = $1"),
            &[pgrx::datum::DatumWithOid::from(o_id)],
        )
        .unwrap()
        .unwrap_or(0)
    }

    #[pg_test]
    fn test_merge_keeps_delta_row_inserted_mid_merge() {
        let p = "<https://merge-race.test/p_insert>";
        let p_id = promoted_predicate(p);
        let late = "<https://merge-race.test/late>";

        super::super::merge_predicate_with_hook(p_id, || {
            crate::storage::insert_triple(S, p, late, 0);
        });

        assert_eq!(visible(p_id, late), 1, "row inserted mid-merge was lost");
        assert_eq!(visible(p_id, "<https://merge-race.test/o0>"), 1);
        let count = Spi::get_one_with_args::<i64>(
            "SELECT triple_count FROM _pg_ripple.predicates WHERE id = $1",
            &[pgrx::datum::DatumWithOid::from(p_id)],
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            count, 2,
            "triple_count must include the surviving delta row"
        );

        // The next cycle folds the survivor into main.
        super::super::merge_predicate(p_id);
        assert_eq!(visible(p_id, late), 1);
        let delta_rows = Spi::get_one::<i64>(&format!(
            "SELECT count(*)::bigint FROM _pg_ripple.vp_{p_id}_delta"
        ))
        .unwrap()
        .unwrap();
        assert_eq!(delta_rows, 0);
    }

    #[pg_test]
    fn test_merge_keeps_tombstone_written_mid_merge() {
        let p = "<https://merge-race.test/p_tomb>";
        let p_id = promoted_predicate(p);
        let o0 = "<https://merge-race.test/o0>";
        super::super::merge_predicate(p_id); // o0 now lives in main

        // A delete of a main-resident triple writes a tombstone.  With the
        // default tombstone_retention_seconds = 0 the old merge TRUNCATEd it.
        super::super::merge_predicate_with_hook(p_id, || {
            crate::storage::delete_triple(S, p, o0, 0);
        });
        assert_eq!(visible(p_id, o0), 0, "delete during merge was undone");

        super::super::merge_predicate(p_id);
        assert_eq!(visible(p_id, o0), 0, "delete undone by the following merge");
    }

    #[pg_test]
    fn test_merge_drops_delta_row_deleted_mid_merge() {
        let p = "<https://merge-race.test/p_delete>";
        let p_id = promoted_predicate(p);
        let o0 = "<https://merge-race.test/o0>";

        // o0 is still in delta: delete_triple removes the delta row and writes
        // no tombstone, after main_new already copied it.
        super::super::merge_predicate_with_hook(p_id, || {
            crate::storage::delete_triple(S, p, o0, 0);
        });
        assert_eq!(
            visible(p_id, o0),
            0,
            "deleted delta row resurrected in main"
        );
    }

    #[pg_test]
    fn test_merge_holds_predicate_fence_during_build() {
        // Overlapping merges of one predicate (worker with merge_workers = 1,
        // compact()) must serialise on 0x5052_5000 + pred_id for the WHOLE
        // merge: the build phase already holds it.  True cross-session overlap
        // cannot run inside one pg_test backend (advisory locks are re-entrant
        // per session), so this pins the lock's scope.
        let p = "<https://merge-race.test/p_fence>";
        let p_id = promoted_predicate(p);
        let key = 0x5052_5000_i64 + p_id;
        let mut held = false;
        super::super::merge_predicate_with_hook(p_id, || {
            held = Spi::get_one_with_args::<bool>(
                "SELECT EXISTS (SELECT 1 FROM pg_locks \
                 WHERE locktype = 'advisory' AND pid = pg_backend_pid() \
                   AND mode = 'ExclusiveLock' AND granted \
                   AND ((classid::bigint << 32) | objid::bigint) = $1)",
                &[pgrx::datum::DatumWithOid::from(key)],
            )
            .unwrap()
            .unwrap();
        });
        assert!(held, "merge fence must be held while main_new is built");
    }

    #[pg_test(error = "merge: main table was replaced while main_new was built; aborting merge")]
    fn test_merge_aborts_when_main_replaced_mid_merge() {
        let p = "<https://merge-race.test/p_oid>";
        let p_id = promoted_predicate(p);
        super::super::merge_predicate_with_hook(p_id, || {
            Spi::run(&format!(
                "ALTER TABLE _pg_ripple.vp_{p_id}_main RENAME TO vp_{p_id}_main_prev; \
                 CREATE TABLE _pg_ripple.vp_{p_id}_main \
                     (LIKE _pg_ripple.vp_{p_id}_main_prev INCLUDING ALL)"
            ))
            .unwrap();
        });
    }
}
