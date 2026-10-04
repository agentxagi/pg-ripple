//! Symmetric-CBD helper split out of `scan.rs` (v0.140.3, VAL-376 review).
//!
//! `scan.rs` hit the 1,000-line lint gate (Q13-04); this function is
//! describe-side (its only caller lives in `sparql::execute::describe`),
//! so it moved to its own `describe_object` module file as the least
//! disruptive home.

use pgrx::datum::DatumWithOid;
use pgrx::prelude::*;

/// Return all `(subject_id, predicate_id)` pairs where the given `object_id`
/// appears as the object.  Used by the symmetric CBD DESCRIBE algorithm.
pub fn triples_for_object(object_id: i64) -> Vec<(i64, i64)> {
    let mut result = Vec::new();

    let pred_ids: Vec<i64> = Spi::connect(|c| {
        c.select(
            "SELECT id FROM _pg_ripple.predicates WHERE table_oid IS NOT NULL",
            None,
            &[],
        )
        .unwrap_or_else(|e| pgrx::error!("describe_incoming predicates SPI error: {e}"))
        .filter_map(|row| row.get::<i64>(1).ok().flatten())
        .collect()
    });

    for p_id in pred_ids {
        let table = format!("_pg_ripple.vp_{p_id}");
        let pairs: Vec<(i64, i64)> = Spi::connect(|c| {
            c.select(
                &format!("SELECT s, $1 FROM {table} WHERE o = $2"),
                None,
                &[DatumWithOid::from(p_id), DatumWithOid::from(object_id)],
            )
            .unwrap_or_else(|e| pgrx::error!("describe_incoming vp SPI error: {e}"))
            .filter_map(|row| {
                Some((
                    row.get::<i64>(1).ok().flatten()?,
                    row.get::<i64>(2).ok().flatten()?,
                ))
            })
            .collect()
        });
        result.extend(pairs);
    }

    let rare_pairs: Vec<(i64, i64)> = Spi::connect(|c| {
        c.select(
            "SELECT s, p FROM _pg_ripple.vp_rare WHERE o = $1",
            None,
            &[DatumWithOid::from(object_id)],
        )
        .unwrap_or_else(|e| pgrx::error!("describe_incoming vp_rare SPI error: {e}"))
        .filter_map(|row| {
            Some((
                row.get::<i64>(1).ok().flatten()?,
                row.get::<i64>(2).ok().flatten()?,
            ))
        })
        .collect()
    });
    result.extend(rare_pairs);

    result
}
