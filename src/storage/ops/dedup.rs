//! Deduplication helpers for VP tables (v0.7.0, split from scan.rs v0.122.0).

use pgrx::datum::DatumWithOid;
use pgrx::prelude::*;

use super::super::super::vp_rare_io::get_dedicated_vp_table;
use crate::dictionary;

/// Remove duplicate `(s, o, g)` rows for the predicate identified by `p_iri`.
///
/// Strategy:
/// - **delta table**: DELETE all duplicate (s,o,g) rows keeping the minimum-i row.
/// - **main table**: tombstone each duplicate (s,o,g) group once (existence-guarded)
///   and re-assert its minimum-SID row into delta, so duplicates are masked at
///   query time and main collapses to one row on the next merge; the catalog
///   counters are kept in step and the HTAP view is flipped to the tombstone-
///   aware form when tombstone_count leaves 0 (VAL-380).
/// - **vp_rare** (if predicate has no dedicated table): DELETE duplicate rows by
///   (p, s, o, g) keeping the minimum ctid.
///
/// Runs ANALYZE on all modified tables afterward.
/// Returns the total count of rows removed.
pub fn deduplicate_predicate(p_iri: &str) -> i64 {
    let p_clean = if p_iri.starts_with('<') && p_iri.ends_with('>') {
        &p_iri[1..p_iri.len() - 1]
    } else {
        p_iri
    };

    let p_id = match dictionary::lookup_iri(p_clean) {
        Some(id) => id,
        None => {
            // Predicate not in dictionary — nothing to deduplicate.
            return 0;
        }
    };

    let mut total_removed: i64 = 0;

    if get_dedicated_vp_table(p_id).is_some() {
        // Dedicated HTAP VP table: handle delta and main separately.
        let delta = format!("_pg_ripple.vp_{p_id}_delta");
        let main = format!("_pg_ripple.vp_{p_id}_main");
        let tombs = format!("_pg_ripple.vp_{p_id}_tombstones");

        // Deduplicate delta: delete all rows keeping the minimum-i (SID) row per (s,o,g).
        let delta_removed = Spi::get_one_with_args::<i64>(
            &format!(
                "WITH keep AS ( \
                     SELECT s, o, g, MIN(i) AS min_i \
                     FROM {delta} \
                     GROUP BY s, o, g \
                     HAVING COUNT(*) > 1 \
                 ), \
                 del AS ( \
                     DELETE FROM {delta} d \
                     USING keep k \
                     WHERE d.s = k.s AND d.o = k.o AND d.g = k.g AND d.i <> k.min_i \
                     RETURNING 1 \
                 ) \
                 SELECT COUNT(*)::BIGINT FROM del"
            ),
            &[],
        )
        .unwrap_or(None)
        .unwrap_or(0);

        // VAL-376: physically removed duplicate rows must leave the catalog
        // counter too, or dedup repairs the table but re-drifts the catalog.
        if delta_removed > 0 {
            Spi::run_with_args(
                "UPDATE _pg_ripple.predicates \
                 SET triple_count = GREATEST(0, triple_count - $2) WHERE id = $1",
                &[DatumWithOid::from(p_id), DatumWithOid::from(delta_removed)],
            )
            .unwrap_or_else(|e| pgrx::error!("dedup delta count update SPI error: {e}"));
        }

        total_removed += delta_removed;

        // Deduplicate main: tombstone duplicate (s,o,g) groups so they are
        // masked at query time and physically collapse on the next merge.
        //
        // The tombstone join is on (s, o, g), so one tombstone masks EVERY
        // main row of the group — including the minimum-SID row that must
        // survive. This statement therefore also re-asserts that survivor
        // into delta (explicit i, ON CONFLICT DO NOTHING): the tombstone-
        // aware view keeps returning exactly one row per group, and the next
        // merge folds (main − tombstones) ∪ delta into a single physical
        // row. VAL-380: without the re-assert, flipping the view to the
        // tombstone-aware form (below) hides the whole group and the next
        // merge drops it permanently.
        //
        // Existence-guarded INSERT: a pre-existing tombstone wins (a real
        // delete must never be undone by re-asserting a survivor; a prior
        // dedup run already arranged it), so repeated runs are no-ops.
        let (main_removed, survivors_kept) = Spi::get_two_with_args::<i64, i64>(
            &format!(
                "WITH ranked AS ( \
                     SELECT s, o, g, i, \
                            ROW_NUMBER() OVER (PARTITION BY s, o, g ORDER BY i ASC) AS rn \
                     FROM {main} \
                 ), \
                 dup_groups AS ( \
                     SELECT s, o, g, COUNT(*)::BIGINT AS n \
                     FROM ranked \
                     GROUP BY s, o, g \
                     HAVING COUNT(*) > 1 \
                 ), \
                 new_ts AS ( \
                     INSERT INTO {tombs} (s, o, g) \
                     SELECT d.s, d.o, d.g FROM dup_groups d \
                     WHERE NOT EXISTS ( \
                         SELECT 1 FROM {tombs} t \
                         WHERE t.s = d.s AND t.o = d.o AND t.g = d.g \
                     ) \
                     RETURNING s, o, g \
                 ), \
                 survivors AS ( \
                     INSERT INTO {delta} (s, o, g, i) \
                     SELECT r.s, r.o, r.g, r.i \
                     FROM ranked r \
                     JOIN new_ts nt ON r.s = nt.s AND r.o = nt.o AND r.g = nt.g \
                     WHERE r.rn = 1 \
                     ON CONFLICT (s, o, g) DO NOTHING \
                     RETURNING 1 \
                 ) \
                 SELECT \
                     COALESCE((SELECT SUM(d.n - 1)::BIGINT FROM dup_groups d \
                               JOIN new_ts nt \
                                 ON d.s = nt.s AND d.o = nt.o AND d.g = nt.g), 0::BIGINT), \
                     (SELECT COUNT(*)::BIGINT FROM survivors)"
            ),
            &[],
        )
        .unwrap_or((None, None));
        let main_removed = main_removed.unwrap_or(0);
        let survivors_kept = survivors_kept.unwrap_or(0);

        // VAL-376 (same contract as the delta/vp_rare branches): the
        // tombstoned duplicate rows leave main at the next merge, so they
        // leave the catalog counter now — catalog, query visibility and
        // physical stay in lockstep at every point (one logical triple per
        // deduplicated group from here on; delta holds the survivor).
        if main_removed > 0 {
            Spi::run_with_args(
                "UPDATE _pg_ripple.predicates \
                 SET triple_count = GREATEST(0, triple_count - $2) WHERE id = $1",
                &[DatumWithOid::from(p_id), DatumWithOid::from(main_removed)],
            )
            .unwrap_or_else(|e| pgrx::error!("dedup main count update SPI error: {e}"));
        }

        total_removed += main_removed;

        // Survivors ride the delta inbox — same bookkeeping as the insert
        // paths, so reads and the next merge see them (VAL-380).
        if survivors_kept > 0 {
            crate::shmem::record_delta_inserts(survivors_kept);
            crate::shmem::set_predicate_delta_bit(p_id);
        }

        // VAL-380: tombstones created here must be honoured by the catalog
        // and the HTAP view immediately — same contract as the clear_graph /
        // drop_graph / delete main branches (M15-05). The recompute is
        // absolute (the tombstone table is the source of truth, existence
        // guard included). When the count leaves 0 the view was in the
        // tombstone-skip form (no LEFT JOIN) and must be flipped back, or
        // the new tombstones stay invisible until some other path rebuilds
        // the view.
        if main_removed > 0 {
            let prev_count: i64 = Spi::get_one_with_args::<i64>(
                "SELECT tombstone_count FROM _pg_ripple.predicates WHERE id = $1",
                &[DatumWithOid::from(p_id)],
            )
            .unwrap_or(None)
            .unwrap_or(1);

            Spi::run_with_args(
                &format!(
                    "UPDATE _pg_ripple.predicates \
                     SET tombstone_count = (SELECT COUNT(*)::BIGINT FROM {tombs}) \
                     WHERE id = $1"
                ),
                &[DatumWithOid::from(p_id)],
            )
            .unwrap_or_else(|e| pgrx::error!("dedup main tombstone_count update SPI error: {e}"));

            if prev_count == 0 {
                crate::storage::merge::rebuild_htap_view(p_id, true);
            }
        }

        // ANALYZE both tables.
        Spi::run_with_args(&format!("ANALYZE {delta}"), &[])
            .unwrap_or_else(|e| pgrx::error!("ANALYZE delta error: {e}"));
        Spi::run_with_args(&format!("ANALYZE {main}"), &[])
            .unwrap_or_else(|e| pgrx::error!("ANALYZE main error: {e}"));
    } else {
        // vp_rare: DELETE duplicate (p, s, o, g) keeping the minimum-SID row.
        let rare_removed = Spi::get_one_with_args::<i64>(
            "WITH del AS ( \
                 DELETE FROM _pg_ripple.vp_rare r \
                 WHERE r.p = $1 \
                   AND r.i NOT IN ( \
                       SELECT MIN(i) FROM _pg_ripple.vp_rare \
                       WHERE p = $1 \
                       GROUP BY p, s, o, g \
                   ) \
                 RETURNING 1 \
             ) \
             SELECT COUNT(*)::BIGINT FROM del",
            &[DatumWithOid::from(p_id)],
        )
        .unwrap_or(None)
        .unwrap_or(0);

        // VAL-376: same as the delta branch — removed physical rows must
        // leave the counter.
        if rare_removed > 0 {
            Spi::run_with_args(
                "UPDATE _pg_ripple.predicates \
                 SET triple_count = GREATEST(0, triple_count - $2) WHERE id = $1",
                &[DatumWithOid::from(p_id), DatumWithOid::from(rare_removed)],
            )
            .unwrap_or_else(|e| pgrx::error!("dedup vp_rare count update SPI error: {e}"));
        }

        total_removed += rare_removed;

        if rare_removed > 0 {
            Spi::run_with_args("ANALYZE _pg_ripple.vp_rare", &[])
                .unwrap_or_else(|e| pgrx::error!("ANALYZE vp_rare error: {e}"));
        }
    }

    total_removed
}

/// Remove duplicate `(s, o, g)` rows across all predicates and `vp_rare`.
///
/// Iterates over all predicate IRIs in `_pg_ripple.predicates` and calls
/// `deduplicate_predicate` for each. Then deduplicates `vp_rare` for any
/// predicates that remain in the rare table.
///
/// Returns the total count of rows removed.
pub fn deduplicate_all() -> i64 {
    // Collect all predicate IRIs from the catalog.
    let pred_iris: Vec<String> = Spi::connect(|c| {
        c.select(
            "SELECT d.value FROM _pg_ripple.predicates p \
             JOIN _pg_ripple.dictionary d ON d.id = p.id",
            None,
            &[],
        )
        .unwrap_or_else(|e| pgrx::error!("deduplicate_all SPI error: {e}"))
        .filter_map(|row| row.get::<&str>(1).ok().flatten().map(|s| s.to_owned()))
        .collect()
    });

    let mut total: i64 = 0;
    for iri in pred_iris {
        total += deduplicate_predicate(&iri);
    }

    // Deduplicate all remaining rare triples in vp_rare.
    let rare_removed = Spi::get_one_with_args::<i64>(
        "WITH del AS ( \
             DELETE FROM _pg_ripple.vp_rare r \
             WHERE r.i NOT IN ( \
                 SELECT MIN(i) FROM _pg_ripple.vp_rare \
                 GROUP BY p, s, o, g \
             ) \
             RETURNING 1 \
         ) \
         SELECT COUNT(*)::BIGINT FROM del",
        &[],
    )
    .unwrap_or(None)
    .unwrap_or(0);

    total += rare_removed;

    if rare_removed > 0 {
        Spi::run_with_args("ANALYZE _pg_ripple.vp_rare", &[])
            .unwrap_or_else(|e| pgrx::error!("ANALYZE vp_rare error: {e}"));
    }

    total
}

/// Look up the statement ID (`i` column) for a given `(s, p, o)` triple.
///
/// Returns `None` if the triple does not exist.
pub fn statement_id_for_triple(s: i64, p: i64, o: i64) -> Option<i64> {
    // Check dedicated VP table first.
    let table_oid = Spi::get_one_with_args::<i64>(
        "SELECT table_oid::bigint FROM _pg_ripple.predicates WHERE id = $1",
        &[DatumWithOid::from(p)],
    )
    .unwrap_or(None);

    if table_oid.is_some() {
        let sql = format!("SELECT i FROM _pg_ripple.vp_{p} WHERE s = {s} AND o = {o} LIMIT 1");
        if let Ok(Some(sid)) = Spi::get_one::<i64>(&sql) {
            return Some(sid);
        }
    }

    // Fall back to vp_rare.
    Spi::get_one_with_args::<i64>(
        &format!("SELECT i FROM _pg_ripple.vp_rare WHERE p = {p} AND s = {s} AND o = {o} LIMIT 1"),
        &[],
    )
    .unwrap_or(None)
}
