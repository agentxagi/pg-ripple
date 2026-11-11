//! Read-only dictionary lookups for the query path (FB-0278).
//!
//! A SPARQL query must not write the dictionary: the ValorBrain app role only
//! reads `_pg_ripple`, and the encode path's `INSERT ... ON CONFLICT` needs the
//! INSERT privilege even for a term already stored. These are the read sides
//! of `encode`, `encode_lang_literal` and `encode_typed_literal`, plus SQL that
//! resolves a term not stored yet when the statement runs.

use super::{
    KIND_LANG_LITERAL, KIND_LITERAL, KIND_TYPED_LITERAL, inline_typed_literal, lang_literal_term,
    lookup, term_hash, typed_literal_term,
};

/// Look up a plain literal (`xsd:string`) without inserting: the read side of
/// `encode(value, KIND_LITERAL)`.
///
/// A SPARQL query must not write the dictionary. The ValorBrain app role only
/// reads `_pg_ripple`, and an ASK with a literal object failed with
/// "permission denied for table dictionary" whenever the literal was not in
/// the shared cache, even when it was already stored (FB-0278): the encode
/// path runs an INSERT ... ON CONFLICT, which needs the INSERT privilege
/// either way. A literal that was never stored matches no triple.
pub fn lookup_plain_literal(value: &str) -> Option<i64> {
    lookup(value, KIND_LITERAL)
}

/// Look up a language-tagged literal without inserting (same canonical form
/// as `encode_lang_literal`).
pub fn lookup_lang_literal(value: &str, lang: &str) -> Option<i64> {
    lookup(&lang_literal_term(value, lang), KIND_LANG_LITERAL)
}

/// Look up a typed literal without inserting: the inline encoding first, as
/// `encode_typed_literal` does, then the same canonical form.
pub fn lookup_typed_literal(value: &str, datatype: &str) -> Option<i64> {
    if let Some(id) = inline_typed_literal(value, datatype) {
        return Some(id);
    }
    lookup(&typed_literal_term(value, datatype), KIND_TYPED_LITERAL)
}

/// SQL that finds a term's dictionary id when the statement runs, by the hash
/// `lookup` uses: the id once the term is stored, `NULL` before (so `col = …`
/// matches nothing). For a term a query names that the dictionary does not
/// hold yet (FB-0278): the query must not write it, and SQL kept for later —
/// by the plan cache or by a view — must still match the term once it is
/// stored, which a `FALSE` frozen at translation would not (review of #29).
/// It needs only SELECT on `_pg_ripple.dictionary`.
pub fn lookup_sql(term: &str, kind: i16) -> String {
    let hash128 = term_hash(term, kind);
    let hash_hi = (hash128 >> 64) as i64;
    let hash_lo = hash128 as i64;
    format!(
        "(SELECT id FROM _pg_ripple.dictionary WHERE hash_hi = {hash_hi} AND hash_lo = {hash_lo})"
    )
}

/// `lookup_sql` for a language-tagged literal.
pub fn lang_literal_lookup_sql(value: &str, lang: &str) -> String {
    lookup_sql(&lang_literal_term(value, lang), KIND_LANG_LITERAL)
}

/// `lookup_sql` for a typed literal that has no inline encoding.
pub fn typed_literal_lookup_sql(value: &str, datatype: &str) -> String {
    lookup_sql(&typed_literal_term(value, datatype), KIND_TYPED_LITERAL)
}
