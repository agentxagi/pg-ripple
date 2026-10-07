//! Flat VP table → HTAP split migration (extracted from merge.rs, MERGE-RACE-01).

use pgrx::datum::DatumWithOid;
use pgrx::prelude::*;

use super::is_htap;

/// Migrate an existing flat VP table `_pg_ripple.vp_{id}` to the HTAP split.
///
/// Called from the `ALTER EXTENSION pg_ripple UPDATE` migration script
/// via the `pg_ripple.htap_migrate_predicate(bigint)` function.
pub fn migrate_flat_to_htap(pred_id: i64) {
    let flat = format!("_pg_ripple.vp_{pred_id}");
    let backup = format!("_pg_ripple.vp_{pred_id}_pre_htap");
    let delta = format!("_pg_ripple.vp_{pred_id}_delta");
    let main = format!("_pg_ripple.vp_{pred_id}_main");
    let tombs = format!("_pg_ripple.vp_{pred_id}_tombstones");
    let view = format!("_pg_ripple.vp_{pred_id}");

    // Check if already migrated.
    if is_htap(pred_id) {
        return;
    }

    // Rename flat table → backup.
    Spi::run_with_args(
        &format!("ALTER TABLE IF EXISTS {flat} RENAME TO vp_{pred_id}_pre_htap"),
        &[],
    )
    .unwrap_or_else(|e| pgrx::error!("htap_migrate: rename flat error: {e}"));

    // Create delta table (copy existing rows into it as the write inbox).
    Spi::run_with_args(
        &format!("CREATE TABLE {delta} AS SELECT * FROM {backup}"),
        &[],
    )
    .unwrap_or_else(|e| pgrx::error!("htap_migrate: create delta error: {e}"));

    Spi::run_with_args(
        &format!("CREATE INDEX idx_vp_{pred_id}_delta_s_o ON {delta} (s, o)"),
        &[],
    )
    .unwrap_or_else(|e| pgrx::error!("htap_migrate: delta index(s,o) error: {e}"));

    Spi::run_with_args(
        &format!("CREATE INDEX idx_vp_{pred_id}_delta_o_s ON {delta} (o, s)"),
        &[],
    )
    .unwrap_or_else(|e| pgrx::error!("htap_migrate: delta index(o,s) error: {e}"));

    // Create empty main table.
    Spi::run_with_args(
        &format!(
            "CREATE TABLE {main} ( \
                 s      BIGINT   NOT NULL, \
                 o      BIGINT   NOT NULL, \
                 g      BIGINT   NOT NULL DEFAULT 0, \
                 i      BIGINT   NOT NULL DEFAULT nextval('_pg_ripple.statement_id_seq'), \
                 source SMALLINT NOT NULL DEFAULT 0 \
             )"
        ),
        &[],
    )
    .unwrap_or_else(|e| pgrx::error!("htap_migrate: create main error: {e}"));

    Spi::run_with_args(
        &format!("CREATE INDEX idx_vp_{pred_id}_main_brin ON {main} USING BRIN (s)"),
        &[],
    )
    .unwrap_or_else(|e| pgrx::error!("htap_migrate: main BRIN index error: {e}"));

    // Create empty tombstones table.
    Spi::run_with_args(
        &format!(
            "CREATE TABLE {tombs} ( \
                 s BIGINT NOT NULL, \
                 o BIGINT NOT NULL, \
                 g BIGINT NOT NULL DEFAULT 0, \
                 i BIGINT NOT NULL DEFAULT nextval('_pg_ripple.statement_id_seq') \
             )"
        ),
        &[],
    )
    .unwrap_or_else(|e| pgrx::error!("htap_migrate: create tombstones error: {e}"));

    Spi::run_with_args(
        &format!("CREATE INDEX idx_vp_{pred_id}_tombs ON {tombs} (s, o, g)"),
        &[],
    )
    .unwrap_or_else(|e| pgrx::error!("htap_migrate: tombstones index error: {e}"));

    // Create the view.
    Spi::run_with_args(
        &format!(
            "CREATE VIEW {view} AS \
             SELECT m.s, m.o, m.g, m.i, m.source \
             FROM {main} m \
             LEFT JOIN {tombs} t ON m.s = t.s AND m.o = t.o AND m.g = t.g \
             WHERE t.s IS NULL \
             UNION ALL \
             SELECT d.s, d.o, d.g, d.i, d.source \
             FROM {delta} d"
        ),
        &[],
    )
    .unwrap_or_else(|e| pgrx::error!("htap_migrate: create view error: {e}"));

    // Update predicates catalog.
    Spi::run_with_args(
        "UPDATE _pg_ripple.predicates \
         SET table_oid = $2::regclass::oid, htap = true \
         WHERE id = $1",
        &[
            DatumWithOid::from(pred_id),
            DatumWithOid::from(view.as_str()),
        ],
    )
    .unwrap_or_else(|e| pgrx::error!("htap_migrate: predicates update error: {e}"));

    // Drop the backup table.
    Spi::run_with_args("SET LOCAL pg_ripple.maintenance_mode = 'on'", &[])
        .unwrap_or_else(|e| pgrx::error!("htap_migrate: set maintenance_mode error: {e}"));
    Spi::run_with_args(&format!("DROP TABLE IF EXISTS {backup}"), &[])
        .unwrap_or_else(|e| pgrx::error!("htap_migrate: drop backup error: {e}"));
}
