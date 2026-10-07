//! HTAP merge logic for pg_ripple v0.6.0.
//!
//! Each VP table is split into:
//! - `_pg_ripple.vp_{id}_delta`      — write inbox (B-tree indexed, small)
//! - `_pg_ripple.vp_{id}_main`       — read-optimised archive (BRIN indexed)
//! - `_pg_ripple.vp_{id}_tombstones` — pending deletes from main
//!
//! A VIEW `_pg_ripple.vp_{id}` exposes the union of main + delta minus
//! tombstones, maintaining backward compatibility with the SPARQL query engine.
//!
//! The merge cycle ("fresh-table generation merge"):
//! 0. Snapshot delta and tombstones into private temp tables
//! 1. Create `vp_{id}_main_new` from `(main − tombstone snapshot) UNION ALL delta snapshot ORDER BY s`
//! 2. Add BRIN index on `vp_{id}_main_new` (on i column — monotonic SID)
//! 3. Lock delta/tombstones against writers, reconcile, atomically rename `_main_new` to `_main`
//! 4. Delete exactly the snapshotted delta and tombstone rows (never TRUNCATE)
//! 5. ANALYZE the new main table

use pgrx::datum::DatumWithOid;
use pgrx::prelude::*;

// ─── Schema setup ─────────────────────────────────────────────────────────────

/// Create the `subject_patterns` and `object_patterns` tables if they are absent.
// Q15-01: internal API field; kept for public API surface or future extension consumers.
#[allow(dead_code)]
pub fn initialize_pattern_tables() {
    Spi::run_with_args(
        "CREATE TABLE IF NOT EXISTS _pg_ripple.subject_patterns ( \
             s       BIGINT   NOT NULL PRIMARY KEY, \
             pattern BIGINT[] NOT NULL \
         )",
        &[],
    )
    .unwrap_or_else(|e| pgrx::error!("subject_patterns table creation error: {e}"));

    Spi::run_with_args(
        "CREATE INDEX IF NOT EXISTS idx_subject_patterns_gin \
         ON _pg_ripple.subject_patterns USING GIN (pattern)",
        &[],
    )
    .unwrap_or_else(|e| pgrx::error!("subject_patterns GIN index creation error: {e}"));

    Spi::run_with_args(
        "CREATE TABLE IF NOT EXISTS _pg_ripple.object_patterns ( \
             o       BIGINT   NOT NULL PRIMARY KEY, \
             pattern BIGINT[] NOT NULL \
         )",
        &[],
    )
    .unwrap_or_else(|e| pgrx::error!("object_patterns table creation error: {e}"));

    Spi::run_with_args(
        "CREATE INDEX IF NOT EXISTS idx_object_patterns_gin \
         ON _pg_ripple.object_patterns USING GIN (pattern)",
        &[],
    )
    .unwrap_or_else(|e| pgrx::error!("object_patterns GIN index creation error: {e}"));

    // v0.6.0: add `htap` flag to predicates catalog (idempotent).
    Spi::run_with_args(
        "ALTER TABLE _pg_ripple.predicates \
         ADD COLUMN IF NOT EXISTS htap BOOLEAN NOT NULL DEFAULT false",
        &[],
    )
    .unwrap_or_else(|e| pgrx::error!("predicates.htap column migration error: {e}"));

    // v0.61.0: add `brin_summarize_failures` counter to predicates catalog (idempotent).
    Spi::run_with_args(
        "ALTER TABLE _pg_ripple.predicates \
         ADD COLUMN IF NOT EXISTS brin_summarize_failures INT NOT NULL DEFAULT 0",
        &[],
    )
    .unwrap_or_else(|e| {
        pgrx::warning!("predicates.brin_summarize_failures column migration (non-fatal): {e}")
    });
}

// ─── HTAP table creation ──────────────────────────────────────────────────────

/// Create the HTAP triple partition for `pred_id`:
/// - `_pg_ripple.vp_{id}_delta`      (B-tree on s,o and o,s)
/// - `_pg_ripple.vp_{id}_main`       (BRIN on i — monotonic SID column)
/// - `_pg_ripple.vp_{id}_tombstones` (index on s,o,g)
/// - VIEW `_pg_ripple.vp_{id}`       = (main − tombstones) UNION ALL delta
///
/// Marks `predicates.htap = true` and updates `table_oid` to the view OID.
pub fn ensure_htap_tables(pred_id: i64) -> String {
    let view = format!("_pg_ripple.vp_{pred_id}");
    let delta = format!("_pg_ripple.vp_{pred_id}_delta");
    let main = format!("_pg_ripple.vp_{pred_id}_main");
    let tombs = format!("_pg_ripple.vp_{pred_id}_tombstones");

    // Delta table — write inbox.
    Spi::run_with_args(
        &format!(
            "CREATE TABLE IF NOT EXISTS {delta} ( \
                 s      BIGINT   NOT NULL, \
                 o      BIGINT   NOT NULL, \
                 g      BIGINT   NOT NULL DEFAULT 0, \
                 i      BIGINT   NOT NULL DEFAULT nextval('_pg_ripple.statement_id_seq'), \
                 source SMALLINT NOT NULL DEFAULT 0, \
                 UNIQUE (s, o, g) \
             )"
        ),
        &[],
    )
    .unwrap_or_else(|e| pgrx::error!("delta table creation error: {e}"));

    Spi::run_with_args(
        &format!("CREATE INDEX IF NOT EXISTS idx_vp_{pred_id}_delta_s_o ON {delta} (s, o)"),
        &[],
    )
    .unwrap_or_else(|e| pgrx::error!("delta index(s,o) error: {e}"));

    Spi::run_with_args(
        &format!("CREATE INDEX IF NOT EXISTS idx_vp_{pred_id}_delta_o_s ON {delta} (o, s)"),
        &[],
    )
    .unwrap_or_else(|e| pgrx::error!("delta index(o,s) error: {e}"));

    // Main table — read-optimised.
    Spi::run_with_args(
        &format!(
            "CREATE TABLE IF NOT EXISTS {main} ( \
                 s      BIGINT   NOT NULL, \
                 o      BIGINT   NOT NULL, \
                 g      BIGINT   NOT NULL DEFAULT 0, \
                 i      BIGINT   NOT NULL DEFAULT nextval('_pg_ripple.statement_id_seq'), \
                 source SMALLINT NOT NULL DEFAULT 0 \
             )"
        ),
        &[],
    )
    .unwrap_or_else(|e| pgrx::error!("main table creation error: {e}"));

    Spi::run_with_args(
        &format!(
            "CREATE INDEX IF NOT EXISTS idx_vp_{pred_id}_main_i_brin ON {main} USING BRIN (i)"
        ),
        &[],
    )
    .unwrap_or_else(|e| pgrx::error!("main BRIN index error: {e}"));

    // Tombstones table — pending deletes from main.
    // Column `i` records the SID at insert time; merge_predicate matches it
    // (with s, o, g) to delete exactly the tombstones it consumed (MERGE-RACE-01).
    Spi::run_with_args(
        &format!(
            "CREATE TABLE IF NOT EXISTS {tombs} ( \
                 s BIGINT NOT NULL, \
                 o BIGINT NOT NULL, \
                 g BIGINT NOT NULL DEFAULT 0, \
                 i BIGINT NOT NULL DEFAULT nextval('_pg_ripple.statement_id_seq') \
             )"
        ),
        &[],
    )
    .unwrap_or_else(|e| pgrx::error!("tombstones table creation error: {e}"));

    Spi::run_with_args(
        &format!(
            "CREATE INDEX IF NOT EXISTS idx_vp_{pred_id}_tombs \
             ON {tombs} (s, o, g)"
        ),
        &[],
    )
    .unwrap_or_else(|e| pgrx::error!("tombstones index error: {e}"));

    // View — UNION ALL of (main − tombstones) + delta, with dedup safety net (v0.22.0 H-6).
    // The DISTINCT ON (s, o, g) prevents a triple from appearing twice when it exists
    // in both main and delta (e.g., if an insert was already in main before the
    // delta UNIQUE constraint was added, or if a triple crossed a merge boundary
    // before the constraint existed). The UNIQUE (s, o, g) constraint on delta
    // ensures no duplicates within delta itself, and future merges will prevent
    // main+delta duplicates via the merging process. This view definition covers
    // historical data that may not have had the constraint when inserted.
    //
    // Always start with tombstone-aware form (LEFT JOIN). The tombstone-skip
    // optimisation (no LEFT JOIN) is enabled after a merge cycle confirms
    // tombstone_count == 0 (see rebuild_htap_view in merge_predicate).
    let view_sql = htap_view_sql(&view, &main, &delta, &tombs, true);
    Spi::run_with_args(&view_sql, &[])
        .unwrap_or_else(|e| pgrx::error!("vp view creation error: {e}"));

    // Update predicates catalog: set htap=true and table_oid = view OID.
    Spi::run_with_args(
        "INSERT INTO _pg_ripple.predicates (id, table_oid, triple_count, htap) \
         VALUES ($1, $2::regclass::oid, 0, true) \
         ON CONFLICT (id) DO UPDATE \
             SET table_oid = EXCLUDED.table_oid, htap = true",
        &[
            DatumWithOid::from(pred_id),
            DatumWithOid::from(view.as_str()),
        ],
    )
    .unwrap_or_else(|e| pgrx::error!("predicates htap upsert error: {e}"));

    view
}

/// Build the HTAP view SQL for a predicate.
///
/// When `has_tombstones` is `false` (tombstone_count = 0), the view omits the
/// `LEFT JOIN` on the tombstones table, eliminating that join overhead on the
/// hot read path.  When `has_tombstones` is `true`, the full form with the
/// `LEFT JOIN` is used to filter out pending deletes.  (M15-05, v0.96.0)
fn htap_view_sql(view: &str, main: &str, delta: &str, tombs: &str, has_tombstones: bool) -> String {
    if has_tombstones {
        format!(
            "CREATE OR REPLACE VIEW {view} AS \
             SELECT DISTINCT ON (s, o, g) s, o, g, i, source \
             FROM ( \
                 SELECT m.s, m.o, m.g, m.i, m.source \
                 FROM {main} m \
                 LEFT JOIN {tombs} t ON m.s = t.s AND m.o = t.o AND m.g = t.g \
                 WHERE t.s IS NULL \
                 UNION ALL \
                 SELECT d.s, d.o, d.g, d.i, d.source \
                 FROM {delta} d \
             ) merged \
             ORDER BY s, o, g, i ASC"
        )
    } else {
        // Tombstone-skip form: no LEFT JOIN when tombstone_count = 0.
        format!(
            "CREATE OR REPLACE VIEW {view} AS \
             SELECT DISTINCT ON (s, o, g) s, o, g, i, source \
             FROM ( \
                 SELECT s, o, g, i, source FROM {main} \
                 UNION ALL \
                 SELECT s, o, g, i, source FROM {delta} \
             ) merged \
             ORDER BY s, o, g, i ASC"
        )
    }
}

/// Rebuild the HTAP view for `pred_id` to the tombstone-aware or tombstone-free form.
///
/// Called when tombstone_count transitions 0 → 1 (switch to LEFT JOIN form) or
/// when tombstones are fully cleared after a merge cycle (switch to simple form).
pub fn rebuild_htap_view(pred_id: i64, has_tombstones: bool) {
    let view = format!("_pg_ripple.vp_{pred_id}");
    let main = format!("_pg_ripple.vp_{pred_id}_main");
    let delta = format!("_pg_ripple.vp_{pred_id}_delta");
    let tombs = format!("_pg_ripple.vp_{pred_id}_tombstones");
    let sql = htap_view_sql(&view, &main, &delta, &tombs, has_tombstones);
    Spi::run_with_args(&sql, &[])
        .unwrap_or_else(|e| pgrx::error!("rebuild_htap_view: view rebuild error: {e}"));
}

/// Check whether a predicate has been split into HTAP partitions.
pub fn is_htap(pred_id: i64) -> bool {
    Spi::get_one_with_args::<bool>(
        "SELECT htap FROM _pg_ripple.predicates WHERE id = $1",
        &[DatumWithOid::from(pred_id)],
    )
    .unwrap_or(None)
    .unwrap_or(false)
}

/// Return the delta table name for a predicate, or `None` if not HTAP.
#[allow(dead_code)] // used by the ExecutorEnd hook introduced in v0.6.0
pub fn delta_table(pred_id: i64) -> Option<String> {
    if is_htap(pred_id) {
        Some(format!("_pg_ripple.vp_{pred_id}_delta"))
    } else {
        None
    }
}

// ─── Fresh-table generation merge ─────────────────────────────────────────────

/// Merge delta into main for a single predicate.
///
/// Uses the "fresh-table generation merge" to maintain BRIN effectiveness:
/// 0. Snapshots delta and tombstones into private temp tables
/// 1. Creates `vp_{id}_main_new` from the snapshots, rows ordered by `s`
/// 2. Adds BRIN index
/// 3. Locks delta/tombstones against writers and atomically renames
///    `main_new` to `vp_{id}_main`
/// 4. Deletes exactly the snapshotted delta and tombstone rows
/// 5. ANALYZEs the new main table
///
/// Returns the number of rows in the new main table.
pub fn merge_predicate(pred_id: i64) -> i64 {
    merge_predicate_with_hook(pred_id, || {})
}

/// [`merge_predicate`] with a hook that runs after `main_new` has been built
/// and before the swap phase takes its locks — the window in which concurrent
/// writers keep committing into delta/tombstones.  Production passes a no-op;
/// the pg_tests use it to inject those writes deterministically (MERGE-RACE-01).
pub(crate) fn merge_predicate_with_hook(pred_id: i64, after_build: impl FnOnce()) -> i64 {
    if !is_htap(pred_id) {
        return 0;
    }

    // MERGE-FENCE-01 (v0.81.0) / MERGE-RACE-01: the build phase (Steps 0–2)
    // takes no table lock that blocks the query path; the swap (Step 3) locks
    // the relations briefly, without waiting while holding them.
    //
    // CC13-02 (v0.85.0): the per-predicate merge key is namespaced with the
    // pg_ripple prefix 0x5052_5000 (the bare prefix is reserved for Citus
    // rebalance events); only merges take `0x5052_5000 + pred_id`.
    // MERGE-RACE-01: it is taken HERE, for the whole merge, not just the swap.
    // Two merges of one predicate must not overlap: the second would snapshot
    // delta rows the first already moved into the new main and copy them
    // twice (worker with merge_workers = 1 and compact() take no other lock).
    const MERGE_FENCE_NAMESPACE: i64 = 0x5052_5000;
    Spi::run_with_args(
        "SELECT pg_advisory_xact_lock($1)",
        &[DatumWithOid::from(
            MERGE_FENCE_NAMESPACE.wrapping_add(pred_id),
        )],
    )
    .unwrap_or_else(|e| pgrx::error!("merge: advisory lock error: {e}"));

    let main = format!("_pg_ripple.vp_{pred_id}_main");
    let main_new = format!("_pg_ripple.vp_{pred_id}_main_new");
    let delta = format!("_pg_ripple.vp_{pred_id}_delta");
    let tombs = format!("_pg_ripple.vp_{pred_id}_tombstones");
    let view = format!("_pg_ripple.vp_{pred_id}");
    // Guard for the swap: main must still be the relation main_new was built from.
    let main_oid_at_build = Spi::get_one::<i64>(&format!("SELECT '{main}'::regclass::oid::bigint"))
        .unwrap_or_else(|e| pgrx::error!("merge: main oid lookup error: {e}"));
    // MERGE-RACE-01: private snapshots of the delta / tombstone rows this cycle
    // consumes.  They replace v0.22.0 C-4's `max_sid_at_snapshot` (a
    // `last_value` read of statement_id_seq): sequences are not
    // transactional, so a writer that drew its SID before that read but
    // committed after main_new's snapshot was cleaned up without ever having
    // been merged — and delta rows can carry an old explicit SID anyway
    // (dedup re-asserts survivors with their original `i`).
    let delta_snap = "pg_temp.pg_ripple_merge_delta_snap";
    let tombs_snap = "pg_temp.pg_ripple_merge_tombs_snap";

    // Drop any leftover _main_new from a previous failed merge.
    Spi::run_with_args("SET LOCAL pg_ripple.maintenance_mode = 'on'", &[])
        .unwrap_or_else(|e| pgrx::error!("merge: set maintenance_mode (cleanup) error: {e}"));
    Spi::run_with_args(&format!("DROP TABLE IF EXISTS {main_new}"), &[])
        .unwrap_or_else(|e| pgrx::error!("merge: drop leftover main_new error: {e}"));

    // Step 0 (MERGE-RACE-01): snapshot exactly what this cycle consumes.  Step 1
    // builds main_new from these snapshots, and Step 4 deletes exactly these
    // rows from delta / tombstones — never a TRUNCATE.  Under READ COMMITTED
    // each statement takes a fresh snapshot, so rows committed after these
    // copies were taken are not in main_new and must survive the cleanup.
    // merge_all() runs several predicates in one transaction, hence the
    // DROP IF EXISTS in addition to ON COMMIT DROP.
    for sql in [
        format!("DROP TABLE IF EXISTS {delta_snap}"),
        format!("DROP TABLE IF EXISTS {tombs_snap}"),
        format!(
            "CREATE TEMP TABLE pg_ripple_merge_delta_snap ON COMMIT DROP AS \
             SELECT s, o, g, i, source FROM {delta}"
        ),
        format!(
            "CREATE TEMP TABLE pg_ripple_merge_tombs_snap ON COMMIT DROP AS \
             SELECT s, o, g, i FROM {tombs}"
        ),
    ] {
        Spi::run_with_args(&sql, &[])
            .unwrap_or_else(|e| pgrx::error!("merge: snapshot delta/tombstones error: {e}"));
    }

    // Step 1: create fresh main_new from (main − tombstones UNION ALL delta) ORDER BY s,
    // reading delta and tombstones through the Step 0 snapshots.
    // When dedup_on_merge is enabled, use DISTINCT ON (s,o,g) to deduplicate,
    // keeping the row with the lowest SID (oldest assertion) per logical triple.
    let dedup_on_merge = crate::DEDUP_ON_MERGE.get();
    let create_sql = if dedup_on_merge {
        format!(
            "CREATE TABLE {main_new} AS \
             SELECT DISTINCT ON (merged.s, merged.o, merged.g) \
                    merged.s, merged.o, merged.g, merged.i, merged.source \
             FROM ( \
                 SELECT m.s, m.o, m.g, m.i, m.source \
                 FROM {main} m \
                 LEFT JOIN {tombs_snap} t ON m.s = t.s AND m.o = t.o AND m.g = t.g \
                 WHERE t.s IS NULL \
                 UNION ALL \
                 SELECT d.s, d.o, d.g, d.i, d.source \
                 FROM {delta_snap} d \
             ) merged \
             ORDER BY merged.s, merged.o, merged.g, merged.i ASC"
        )
    } else {
        format!(
            "CREATE TABLE {main_new} AS \
             SELECT merged.s, merged.o, merged.g, merged.i, merged.source \
             FROM ( \
                 SELECT m.s, m.o, m.g, m.i, m.source \
                 FROM {main} m \
                 LEFT JOIN {tombs_snap} t ON m.s = t.s AND m.o = t.o AND m.g = t.g \
                 WHERE t.s IS NULL \
                 UNION ALL \
                 SELECT d.s, d.o, d.g, d.i, d.source \
                 FROM {delta_snap} d \
             ) merged \
             ORDER BY merged.s"
        )
    };
    Spi::run_with_args(&create_sql, &[])
        .unwrap_or_else(|e| pgrx::error!("merge: create main_new error: {e}"));

    // Step 2: BRIN index on new main (effective because rows arrive in SID (i) order —
    // monotonically increasing, giving BRIN strong correlation on the i column).
    // Drop any stale index from a previous merge cycle.
    Spi::run_with_args(
        &format!("DROP INDEX IF EXISTS _pg_ripple.idx_vp_{pred_id}_main_new_i_brin"),
        &[],
    )
    .unwrap_or_else(|e| pgrx::error!("merge: drop stale BRIN index error: {e}"));
    Spi::run_with_args(
        &format!("CREATE INDEX idx_vp_{pred_id}_main_new_i_brin ON {main_new} USING BRIN (i)"),
        &[],
    )
    .unwrap_or_else(|e| pgrx::error!("merge: BRIN index on main_new error: {e}"));

    after_build();

    // Step 3: F7-1 (v0.60.0) — atomic rename-swap that never leaves the backing
    // relation non-existent: a. main → main_old, b. main_new → main,
    // c. CREATE OR REPLACE VIEW, d. DROP main_old, e. rename the BRIN index.
    //
    // MERGE-RACE-01: lock everything the swap and cleanup touch in ONE step,
    // never waiting while holding part of it.  Locking the view ACCESS
    // EXCLUSIVE recursively locks main, delta and tombstones (and covers the
    // view rebuilds below): in-flight writers drain first, new ones block
    // until commit.  Each attempt waits at most 25 ms (well under
    // deadlock_timeout), so a transaction that read the view and then writes
    // delta never deadlocks with us; failed attempts roll back their
    // subtransaction (releasing partial locks) and retry with backoff until
    // pg_ripple.merge_lock_timeout_ms, then the merge aborts with no loss.
    let lock_timeout_ms = crate::MERGE_LOCK_TIMEOUT_MS.get();
    Spi::run(&format!(
        "DO $merge_lock$ \
         DECLARE \
             deadline timestamptz := clock_timestamp() + {lock_timeout_ms} * interval '1 ms'; \
             backoff float8 := 0.005; \
         BEGIN \
             PERFORM set_config('lock_timeout', '25ms', true); \
             LOOP \
                 BEGIN \
                     LOCK TABLE {view} IN ACCESS EXCLUSIVE MODE; \
                     RETURN; \
                 EXCEPTION WHEN lock_not_available THEN \
                     IF clock_timestamp() >= deadline THEN \
                         RAISE EXCEPTION 'merge: could not lock {view} within {lock_timeout_ms} ms' \
                             USING ERRCODE = 'lock_not_available'; \
                     END IF; \
                     PERFORM pg_sleep(backoff); \
                     backoff := least(backoff * 2, 0.1); \
                 END; \
             END LOOP; \
         END $merge_lock$"
    ))
    .unwrap_or_else(|e| pgrx::error!("merge: swap lock error: {e}"));
    // MERGE-LOCK-GUC-01 (v0.82.0): remaining statements use the GUC timeout.
    Spi::run_with_args(
        &format!("SET LOCAL lock_timeout = '{lock_timeout_ms}ms'"),
        &[],
    )
    .unwrap_or_else(|e| pgrx::error!("merge: set lock_timeout error: {e}"));

    let main_oid_now = Spi::get_one::<i64>(&format!("SELECT '{main}'::regclass::oid::bigint"))
        .unwrap_or_else(|e| pgrx::error!("merge: main oid lookup error: {e}"));
    if main_oid_now != main_oid_at_build {
        pgrx::warning!("merge: {main} changed OID during the merge");
        pgrx::error!("merge: main table was replaced while main_new was built; aborting merge");
    }

    // MERGE-RACE-01: a snapshotted delta row that a concurrent delete_triple removed
    // while main_new was being built (deletes of delta-resident triples drop
    // the delta row and write no tombstone) must not be resurrected in main.
    // Under the lock above this anti-join is exact.
    let vanished: i64 = Spi::get_one_with_args::<i64>(
        &format!(
            "SELECT count(*)::bigint FROM {delta_snap} x \
             WHERE NOT EXISTS (SELECT 1 FROM {delta} d \
                 WHERE d.s = x.s AND d.o = x.o AND d.g = x.g AND d.i = x.i)"
        ),
        &[],
    )
    .unwrap_or_else(|e| pgrx::error!("merge: vanished delta rows check error: {e}"))
    .unwrap_or(0);
    if vanished > 0 {
        Spi::run_with_args(
            &format!(
                "DELETE FROM {main_new} n USING {delta_snap} x \
                 WHERE n.s = x.s AND n.o = x.o AND n.g = x.g AND n.i = x.i \
                   AND NOT EXISTS (SELECT 1 FROM {delta} d \
                       WHERE d.s = x.s AND d.o = x.o AND d.g = x.g AND d.i = x.i)"
            ),
            &[],
        )
        .unwrap_or_else(|e| pgrx::error!("merge: drop vanished delta rows error: {e}"));
    }

    // Count rows before rename (for return value).
    let row_count: i64 =
        Spi::get_one_with_args::<i64>(&format!("SELECT count(*)::bigint FROM {main_new}"), &[])
            .unwrap_or_else(|e| pgrx::error!("merge: count main_new error: {e}"))
            .unwrap_or(0);

    let main_old = format!("_pg_ripple.vp_{pred_id}_main_old");

    // a. Rename current main → main_old (keeps the old OID in the view working).
    // If main_old already exists from a previously aborted merge, drop it first.
    Spi::run_with_args(&format!("DROP TABLE IF EXISTS {main_old}"), &[])
        .unwrap_or_else(|e| pgrx::error!("merge: drop stale main_old error: {e}"));
    Spi::run_with_args(
        &format!("ALTER TABLE {main} RENAME TO vp_{pred_id}_main_old"),
        &[],
    )
    .unwrap_or_else(|e| pgrx::error!("merge: rename main → main_old error: {e}"));

    // b. Rename main_new → main (backing table for the refreshed view).
    Spi::run_with_args(
        &format!("ALTER TABLE {main_new} RENAME TO vp_{pred_id}_main"),
        &[],
    )
    .unwrap_or_else(|e| pgrx::error!("merge: rename main_new → main error: {e}"));

    // c. CREATE OR REPLACE VIEW — atomically repoints to new main OID.
    // The view must exist for find_triples / SPARQL queries to work correctly.
    // M15-05 (v0.96.0): after renaming main_new → main, we have clean merged data.
    // Tombstones will be cleared in step 4; use the full LEFT JOIN form here
    // (tombstones may still exist until the DELETE below, and tombstones
    // written during the merge survive it).
    let view_sql = htap_view_sql(&view, &main, &delta, &tombs, true);
    Spi::run_with_args(&view_sql, &[])
        .unwrap_or_else(|e| pgrx::error!("merge: recreate view error: {e}"));

    // d. Drop the old main table now that the view points to the new one.
    Spi::run_with_args(&format!("DROP TABLE IF EXISTS {main_old}"), &[])
        .unwrap_or_else(|e| pgrx::error!("merge: drop main_old error: {e}"));

    // e. Rename the BRIN index on the new main to the canonical name.
    //    The old index was dropped with main_old; the new one was created as
    //    idx_vp_{pred_id}_main_new_i_brin and needs to be renamed.
    Spi::run_with_args(
        &format!(
            "ALTER INDEX IF EXISTS _pg_ripple.idx_vp_{pred_id}_main_new_i_brin \
             RENAME TO idx_vp_{pred_id}_main_i_brin"
        ),
        &[],
    )
    .unwrap_or_else(|e| pgrx::warning!("merge: rename BRIN index error (non-fatal): {e}"));

    // Re-summarize BRIN index so page-range summaries are valid immediately
    // without waiting for the autovacuum BRIN worker.
    let brin_sql = format!(
        "SELECT brin_summarize_new_values(c.oid) \
         FROM pg_class c \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = '_pg_ripple' \
           AND c.relname = 'idx_vp_{pred_id}_main_i_brin' \
           AND c.relkind = 'i'"
    );
    // Best-effort: failure to re-summarize is non-fatal (BRIN self-heals on next vacuum).
    if let Err(e) = Spi::run_with_args(&brin_sql, &[]) {
        // v0.61.0 F7-3: increment failure counter and promote to NOTICE after 2nd failure.
        let failure_count: i64 = Spi::get_one_with_args::<i64>(
            "UPDATE _pg_ripple.predicates \
             SET brin_summarize_failures = COALESCE(brin_summarize_failures, 0) + 1 \
             WHERE id = $1 \
             RETURNING brin_summarize_failures",
            &[DatumWithOid::from(pred_id)],
        )
        .unwrap_or(None)
        .unwrap_or(1);

        if failure_count >= 2 {
            pgrx::notice!(
                "merge: brin_summarize_new_values failed for vp_{pred_id}_main (consecutive failure #{failure_count}): {e}"
            );
        } else {
            pgrx::debug1!(
                "merge: brin_summarize_new_values failed for vp_{pred_id}_main (non-fatal): {e}"
            );
        }
    } else {
        // Reset failure counter on success.
        let _ = Spi::run_with_args(
            "UPDATE _pg_ripple.predicates SET brin_summarize_failures = 0 WHERE id = $1",
            &[DatumWithOid::from(pred_id)],
        );
    }

    // v0.37.0: Atomically update _pg_ripple.statements SID-range catalog in the
    // same transaction as the VP table swap. This prevents a race where the merge
    // worker is killed mid-update and leaves a stale SID→OID mapping for RDF-star
    // queries. DELETE then INSERT guarantees an atomic replacement.
    let new_sid_min: i64 =
        Spi::get_one_with_args::<i64>(&format!("SELECT COALESCE(MIN(i), 0) FROM {main}"), &[])
            .unwrap_or(None)
            .unwrap_or(0);
    let new_sid_max: i64 =
        Spi::get_one_with_args::<i64>(&format!("SELECT COALESCE(MAX(i), 0) FROM {main}"), &[])
            .unwrap_or(None)
            .unwrap_or(0);
    if new_sid_min > 0 && new_sid_max >= new_sid_min {
        Spi::run_with_args(
            "DELETE FROM _pg_ripple.statements WHERE predicate_id = $1",
            &[DatumWithOid::from(pred_id)],
        )
        .unwrap_or_else(|e| pgrx::warning!("merge: statements delete error: {e}"));
        Spi::run_with_args(
            "INSERT INTO _pg_ripple.statements (sid_min, sid_max, predicate_id, table_oid) \
             VALUES ($1, $2, $3, \
                 (SELECT c.oid FROM pg_class c \
                  JOIN pg_namespace n ON n.oid = c.relnamespace \
                  WHERE n.nspname = '_pg_ripple' AND c.relname = $4)) \
             ON CONFLICT (sid_min) DO UPDATE \
             SET sid_max = EXCLUDED.sid_max, \
                 predicate_id = EXCLUDED.predicate_id, \
                 table_oid = EXCLUDED.table_oid",
            &[
                DatumWithOid::from(new_sid_min),
                DatumWithOid::from(new_sid_max),
                DatumWithOid::from(pred_id),
                DatumWithOid::from(format!("vp_{pred_id}_main").as_str()),
            ],
        )
        .unwrap_or_else(|e| pgrx::warning!("merge: statements insert error: {e}"));
    }

    // Step 4 (MERGE-RACE-01): delete exactly the delta / tombstone rows main_new
    // consumed (the Step 0 snapshots).  The old `TRUNCATE {delta}` also wiped
    // rows committed after main_new's snapshot — silent data loss — and the
    // old tombstone cleanup (TRUNCATE, or `i <= max_sid_at_snapshot`) could
    // drop a delete that main_new never applied, resurrecting the triple.
    // Rows written after the snapshot stay for the next cycle.  Both joins use
    // existing indexes: delta UNIQUE (s, o, g), tombstones (s, o, g).
    Spi::run_with_args(
        &format!(
            "DELETE FROM {delta} d USING {delta_snap} x \
             WHERE d.s = x.s AND d.o = x.o AND d.g = x.g AND d.i = x.i"
        ),
        &[],
    )
    .unwrap_or_else(|e| pgrx::error!("merge: delete merged delta rows error: {e}"));
    Spi::run_with_args(
        &format!(
            "DELETE FROM {tombs} t USING {tombs_snap} x \
             WHERE t.s = x.s AND t.o = x.o AND t.g = x.g AND t.i = x.i"
        ),
        &[],
    )
    .unwrap_or_else(|e| pgrx::error!("merge: delete merged tombstones error: {e}"));
    for sql in [
        format!("DROP TABLE IF EXISTS {delta_snap}"),
        format!("DROP TABLE IF EXISTS {tombs_snap}"),
    ] {
        Spi::run_with_args(&sql, &[])
            .unwrap_or_else(|e| pgrx::error!("merge: drop snapshots error: {e}"));
    }

    if crate::TOMBSTONE_RETENTION_SECONDS.get() == 0 {
        // Record the GC timestamp in the predicates catalog (v0.55.0 migration col).
        Spi::run_with_args(
            "UPDATE _pg_ripple.predicates SET tombstones_cleared_at = now() WHERE id = $1",
            &[DatumWithOid::from(pred_id)],
        )
        .unwrap_or_else(|e| pgrx::warning!("merge: tombstones_cleared_at update error: {e}"));
    }

    // M15-05 (v0.96.0): tombstone_count tracks the tombstones that survive the
    // cleanup (MERGE-RACE-01: written mid-merge); at 0 the view is rebuilt to
    // the tombstone-skip form.
    let remaining_tombs: i64 =
        Spi::get_one_with_args::<i64>(&format!("SELECT count(*)::bigint FROM {tombs}"), &[])
            .unwrap_or(None)
            .unwrap_or(1); // default 1 = assume tombstones remain, safer
    Spi::run_with_args(
        "UPDATE _pg_ripple.predicates SET tombstone_count = $2 WHERE id = $1",
        &[
            DatumWithOid::from(pred_id),
            DatumWithOid::from(remaining_tombs),
        ],
    )
    .unwrap_or_else(|e| pgrx::warning!("merge: update tombstone_count error: {e}"));
    if remaining_tombs == 0 {
        rebuild_htap_view(pred_id, false);
    }

    // MERGE-RACE-01: rows committed into delta during the merge are still there.
    let remaining_delta: i64 =
        Spi::get_one_with_args::<i64>(&format!("SELECT count(*)::bigint FROM {delta}"), &[])
            .unwrap_or_else(|e| pgrx::error!("merge: count remaining delta error: {e}"))
            .unwrap_or(0);

    // Step 5: ANALYZE so planner has fresh stats.
    // AUTO_ANALYZE GUC (v0.24.0): skip ANALYZE if the user has disabled it.
    if crate::AUTO_ANALYZE.get() {
        Spi::run_with_args(&format!("ANALYZE {main}"), &[])
            .unwrap_or_else(|e| pgrx::error!("merge: ANALYZE error: {e}"));
    }

    // Clear the bloom filter bit — only when delta is really empty.
    if remaining_delta == 0 {
        crate::shmem::clear_predicate_delta_bit(pred_id);
    }

    // Update triple_count in predicates catalog: merged main plus the delta
    // rows that arrived during the merge (each already counted on insert).
    Spi::run_with_args(
        "UPDATE _pg_ripple.predicates SET triple_count = $1 WHERE id = $2",
        &[
            DatumWithOid::from(row_count + remaining_delta),
            DatumWithOid::from(pred_id),
        ],
    )
    .unwrap_or_else(|e| pgrx::error!("merge: update triple_count error: {e}"));

    // v0.37.0: Tombstone GC — schedule VACUUM on the tombstones table when the
    // residual tombstone count exceeds tombstone_gc_threshold × main row count.
    if crate::TOMBSTONE_GC_ENABLED.get() && row_count > 0 {
        let threshold_str = crate::TOMBSTONE_GC_THRESHOLD_STR
            .get()
            .and_then(|c| c.to_str().ok().map(|s| s.to_owned()))
            .unwrap_or_else(|| "0.05".to_string());
        let threshold: f64 = threshold_str.parse().unwrap_or(0.05);
        let tombs_remaining: i64 =
            Spi::get_one_with_args::<i64>(&format!("SELECT count(*)::bigint FROM {tombs}"), &[])
                .unwrap_or(None)
                .unwrap_or(0);
        if (tombs_remaining as f64) / (row_count as f64) > threshold {
            // MERGE-RACE-01: VACUUM cannot run inside the merge transaction
            // (PostgreSQL raises an ERROR that aborts the whole merge — the old
            // `if let Err` never saw it).  The path was unreachable while the
            // default retention TRUNCATEd every tombstone; now tombstones
            // written mid-merge survive, so refresh statistics only and leave
            // dead-tuple reclaim to autovacuum.
            if let Err(e) = Spi::run_with_args(&format!("ANALYZE {tombs}"), &[]) {
                pgrx::warning!("merge: tombstone GC ANALYZE on {tombs}: {e}");
            }
        }
    }

    // v0.53.0: Emit CDC lifecycle NOTIFY (best-effort, non-blocking).
    // Count remaining tombstones after GC for the payload.
    let tombs_remaining: i64 =
        Spi::get_one_with_args::<i64>(&format!("SELECT count(*)::bigint FROM {tombs}"), &[])
            .unwrap_or(None)
            .unwrap_or(0);
    notify_merge_lifecycle(pred_id, row_count, tombs_remaining);

    row_count
}

/// Emit a CDC lifecycle NOTIFY for a completed merge cycle.
///
/// Channel: `pg_ripple_cdc_lifecycle` (global lifecycle channel).
/// Payload: `{"op":"merge","predicate_id":N,"merged":M,"tombstones":T}`
///
/// This is best-effort: errors are logged as warnings and do not fail the merge.
pub(crate) fn notify_merge_lifecycle(pred_id: i64, merged: i64, tombstones: i64) {
    let channel = "pg_ripple_cdc_lifecycle";
    let payload = format!(
        r#"{{"op":"merge","predicate_id":{pred_id},"merged":{merged},"tombstones":{tombstones}}}"#
    );
    let _ = Spi::run_with_args(
        "SELECT pg_notify($1, $2)",
        &[
            pgrx::datum::DatumWithOid::from(channel),
            pgrx::datum::DatumWithOid::from(payload.as_str()),
        ],
    );
}

/// Merge all HTAP predicates.  Returns total rows across all merged main tables.
pub fn merge_all() -> i64 {
    let pred_ids: Vec<i64> = Spi::connect(|c| {
        c.select(
            "SELECT id FROM _pg_ripple.predicates WHERE htap = true ORDER BY id",
            None,
            &[],
        )
        .unwrap_or_else(|e| pgrx::error!("merge_all predicates SPI error: {e}"))
        .filter_map(|row| row.get::<i64>(1).ok().flatten())
        .collect()
    });

    let mut total = 0i64;
    for p_id in pred_ids {
        // Only merge predicates that have rows in delta.
        let delta_rows: i64 = Spi::get_one_with_args::<i64>(
            &format!("SELECT count(*)::bigint FROM _pg_ripple.vp_{p_id}_delta"),
            &[],
        )
        .unwrap_or(None)
        .unwrap_or(0);

        if delta_rows > 0 {
            total += merge_predicate(p_id);
        }
    }

    // CONF-GC-01c: after every merge cycle, purge confidence rows whose
    // statement_id no longer appears in any VP table (orphans from tombstoned
    // or deleted triples that were merged into the main partition and discarded).
    // This is a best-effort sweep; vacuum_confidence() provides on-demand cleanup.
    // The DO block handles the case where the confidence table does not yet exist
    // (fresh installs that have not run the v0.87.0 migration).
    Spi::run(
        "DO $conf_gc$ BEGIN \
           DELETE FROM _pg_ripple.confidence c \
           WHERE NOT EXISTS ( \
             SELECT 1 FROM _pg_ripple.vp_rare WHERE i = c.statement_id \
           ) AND NOT EXISTS ( \
             SELECT 1 FROM _pg_ripple.predicates p2 \
             WHERE p2.table_oid IS NOT NULL \
               AND EXISTS ( \
                 SELECT 1 FROM pg_catalog.pg_class pc \
                 WHERE pc.oid = p2.table_oid \
                   AND pc.relname LIKE 'vp_%_delta' \
               ) \
           ); \
         EXCEPTION WHEN undefined_table THEN NULL; \
         END $conf_gc$",
    )
    .unwrap_or(());

    total
}

// ─── Pattern tables ────────────────────────────────────────────────────────────

/// Rebuild `_pg_ripple.subject_patterns` from all VP tables.
///
/// For each subject, records the sorted array of all predicates it appears in.
/// Called by the merge worker after each generation merge.
pub fn rebuild_subject_patterns() {
    // Collect all HTAP predicate IDs (predicates with dedicated VP tables).
    // Predicate with table_oid IS NOT NULL are those promoted from vp_rare to have
    // their own dedicated vp_{id} table. We enumerate ONLY these, excluding vp_rare.
    // This prevents the "vp_rare double-count" bug (v0.22.0 H-7) where entries in
    // vp_rare would be counted twice: once via vp_rare itself and once via their
    // respective dedicated vp_{id} tables (if promoted).
    let pred_ids: Vec<i64> = Spi::connect(|c| {
        c.select(
            "SELECT id FROM _pg_ripple.predicates WHERE table_oid IS NOT NULL",
            None,
            &[],
        )
        .unwrap_or_else(|e| pgrx::error!("rebuild_subject_patterns: predicates scan error: {e}"))
        .filter_map(|row| row.get::<i64>(1).ok().flatten())
        .collect()
    });

    if pred_ids.is_empty() {
        return;
    }

    // Build a union query across all dedicated VP tables (view name = _pg_ripple.vp_{id}).
    // Each dedicated VP table's view already incorporates merged main/delta/tombstones.
    // vp_rare is never scanned directly as a table in this aggregation.
    let union_parts: Vec<String> = pred_ids
        .iter()
        .map(|&p| format!("SELECT {p}::bigint AS p, s FROM _pg_ripple.vp_{p}"))
        .collect();

    let union_sql = union_parts.join(" UNION ALL ");

    // Rebuild subject_patterns as an aggregation: s → array_agg(DISTINCT p ORDER BY p).
    Spi::run_with_args(
        &format!(
            "INSERT INTO _pg_ripple.subject_patterns (s, pattern) \
             SELECT s, array_agg(DISTINCT p ORDER BY p) \
             FROM ({union_sql}) AS all_triples \
             GROUP BY s \
             ON CONFLICT (s) DO UPDATE \
                 SET pattern = EXCLUDED.pattern"
        ),
        &[],
    )
    .unwrap_or_else(|e| pgrx::error!("rebuild_subject_patterns: upsert error: {e}"));
}

/// Rebuild `_pg_ripple.object_patterns` from all VP tables.
/// Rebuild `_pg_ripple.object_patterns` from all dedicated VP tables (v0.22.0 H-7).
///
/// For each object, records the sorted array of all predicates it appears in.
/// Only enumerates dedicated VP tables (table_oid IS NOT NULL), never scans vp_rare
/// directly to prevent double-counting. Entries in vp_rare are already reachable via
/// their associated dedicated vp_{id} tables after promotion.
pub fn rebuild_object_patterns() {
    let pred_ids: Vec<i64> = Spi::connect(|c| {
        c.select(
            "SELECT id FROM _pg_ripple.predicates WHERE table_oid IS NOT NULL",
            None,
            &[],
        )
        .unwrap_or_else(|e| pgrx::error!("rebuild_object_patterns: predicates scan error: {e}"))
        .filter_map(|row| row.get::<i64>(1).ok().flatten())
        .collect()
    });

    if pred_ids.is_empty() {
        return;
    }

    let union_parts: Vec<String> = pred_ids
        .iter()
        .map(|&p| format!("SELECT {p}::bigint AS p, o FROM _pg_ripple.vp_{p}"))
        .collect();

    let union_sql = union_parts.join(" UNION ALL ");

    Spi::run_with_args(
        &format!(
            "INSERT INTO _pg_ripple.object_patterns (o, pattern) \
             SELECT o, array_agg(DISTINCT p ORDER BY p) \
             FROM ({union_sql}) AS all_triples \
             GROUP BY o \
             ON CONFLICT (o) DO UPDATE \
                 SET pattern = EXCLUDED.pattern"
        ),
        &[],
    )
    .unwrap_or_else(|e| pgrx::error!("rebuild_object_patterns: upsert error: {e}"));
}

// ─── Full compact ─────────────────────────────────────────────────────────────

/// Trigger an immediate full merge of all HTAP VP tables.
///
/// After the merge, rebuild subject_patterns and object_patterns.
/// Called by `pg_ripple.compact()` SQL function.
pub fn compact() -> i64 {
    let merged = merge_all();
    rebuild_subject_patterns();
    rebuild_object_patterns();
    // Signal the shmem counter to zero.
    crate::shmem::reset_delta_count();
    // All deltas are now empty — reset the bloom filter entirely.
    crate::shmem::reset_bloom_filter();
    merged
}

// ─── Migrate flat table to HTAP ───────────────────────────────────────────────

// Extracted to merge_migrate.rs (MERGE-RACE-01: keeps merge.rs under the
// 1,000-line Q13-04 gate).
#[path = "merge_migrate.rs"]
mod migrate;
pub use migrate::migrate_flat_to_htap;

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(any(test, feature = "pg_test"))]
#[path = "merge_tests.rs"]
mod merge_tests;
