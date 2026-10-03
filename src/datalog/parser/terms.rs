//! Term- and atom-level parsing for the Datalog rule parser.
//!
//! Split out of `parser.rs` (VAL-350, file-size gate Q13-04): variables,
//! triple atoms, term tokenization, and RDF term encoding.

use crate::datalog::{Atom, Term};

/// Parse a variable name from `?var` or `?_` (wildcard).
pub(super) fn parse_variable(text: &str) -> Option<String> {
    text.strip_prefix('?').map(|s| s.to_owned())
}

/// Parse a triple atom with optional GRAPH clause.
///
/// Forms:
/// - `<s> <p> <o>`
/// - `?s <p> ?o`
/// - `GRAPH <g> { <s> <p> <o> }`
/// - `GRAPH ?g { <s> <p> <o> }`
pub(super) fn parse_atom(text: &str) -> Result<Atom, String> {
    let text = text.trim();

    let upper = text.to_uppercase();
    if upper.starts_with("GRAPH") {
        let rest = text[5..].trim();
        // Find the graph term (up to the '{')
        let brace = rest
            .find('{')
            .ok_or_else(|| format!("missing '{{' in GRAPH pattern: {text}"))?;
        let graph_term_str = rest[..brace].trim();
        let inner = rest[brace + 1..].trim();
        let inner = inner
            .strip_suffix('}')
            .ok_or_else(|| format!("missing '}}' in GRAPH pattern: {text}"))?
            .trim();

        let g = parse_term(graph_term_str)?;
        let (s, p, o) = parse_triple_terms(inner)?;
        return Ok(Atom { s, p, o, g });
    }

    let (s, p, o) = parse_triple_terms(text)?;
    Ok(Atom {
        s,
        p,
        o,
        g: Term::DefaultGraph,
    })
}

/// Parse three whitespace-separated terms for a triple pattern.
fn parse_triple_terms(text: &str) -> Result<(Term, Term, Term), String> {
    let tokens = tokenize_terms(text);
    if tokens.len() < 3 {
        return Err(format!(
            "expected 3 terms in triple pattern, got {}: {text}",
            tokens.len()
        ));
    }
    let s = parse_term(&tokens[0])?;
    let p = parse_term(&tokens[1])?;
    // Object may be a multi-token literal; join remaining tokens.
    let o_text = if tokens.len() == 3 {
        tokens[2].clone()
    } else {
        tokens[2..].join(" ")
    };
    let o = parse_term(&o_text)?;
    Ok((s, p, o))
}

/// Tokenize a term list, respecting IRIs and literals.
fn tokenize_terms(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_literal = false;
    let mut in_iri = false;
    let mut in_quoted = false; // << >> quoted triple

    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];
        match c {
            '"' => {
                in_literal = !in_literal;
                current.push(c);
            }
            '<' if !in_literal => {
                // Check for <<
                if i + 1 < chars.len() && chars[i + 1] == '<' {
                    in_quoted = true;
                    current.push(c);
                    current.push(chars[i + 1]);
                    i += 2;
                    continue;
                }
                in_iri = true;
                current.push(c);
            }
            '>' if !in_literal && in_quoted => {
                // Check for >>
                if i + 1 < chars.len() && chars[i + 1] == '>' {
                    in_quoted = false;
                    current.push(c);
                    current.push(chars[i + 1]);
                    i += 2;
                    continue;
                }
                current.push(c);
            }
            '>' if !in_literal && in_iri => {
                in_iri = false;
                current.push(c);
            }
            ' ' | '\t' | '\n' if !in_literal && !in_iri && !in_quoted => {
                if !current.is_empty() {
                    tokens.push(current.trim().to_owned());
                    current.clear();
                }
            }
            _ => current.push(c),
        }
        i += 1;
    }
    if !current.trim().is_empty() {
        tokens.push(current.trim().to_owned());
    }
    tokens
}

/// Parse a single term (simple form without GRAPH context).
pub(super) fn parse_term_simple(text: &str) -> Result<Term, String> {
    parse_term(text)
}

/// Parse a single RDF term.
fn parse_term(text: &str) -> Result<Term, String> {
    let text = text.trim();

    // Variable
    if let Some(name) = text.strip_prefix('?') {
        if name == "_" {
            return Ok(Term::Wildcard);
        }
        return Ok(Term::Var(name.to_owned()));
    }

    // Full IRI <…>
    if text.starts_with('<') && text.ends_with('>') {
        let iri = &text[1..text.len() - 1];
        return Ok(Term::Const(crate::datalog::encode_iri(iri)));
    }

    // Quoted triple << s p o >>
    if text.starts_with("<<") && text.ends_with(">>") {
        let inner = &text[2..text.len() - 2].trim();
        let (s, p, o) = parse_triple_terms(inner)?;
        let s_id = term_to_const(&s)?;
        let p_id = term_to_const(&p)?;
        let o_id = term_to_const(&o)?;
        let id = crate::dictionary::encode_quoted_triple(s_id, p_id, o_id);
        return Ok(Term::Const(id));
    }

    // Typed literal "value"^^<datatype>
    if text.starts_with('"')
        && let Some((val, rest)) = split_literal(text)
    {
        if let Some(dt_str) = rest.strip_prefix("^^") {
            let dt = dt_str.trim().trim_start_matches('<').trim_end_matches('>');
            let dt_resolved = crate::datalog::resolve_prefix(dt);
            let id = crate::dictionary::encode_typed_literal(&val, &dt_resolved);
            return Ok(Term::Const(id));
        }
        if let Some(lang) = rest.strip_prefix('@') {
            let id = crate::dictionary::encode_lang_literal(&val, lang);
            return Ok(Term::Const(id));
        }
        // Plain literal
        let id = crate::dictionary::encode(&val, crate::dictionary::KIND_LITERAL);
        return Ok(Term::Const(id));
    }

    // Blank node _:name
    if let Some(rest) = text.strip_prefix("_:") {
        let id = crate::dictionary::encode(rest, crate::dictionary::KIND_BLANK);
        return Ok(Term::Const(id));
    }

    // Bare numeric literal (integer or decimal): 18, -3, 3.14
    if text
        .chars()
        .next()
        .map(|c| c.is_ascii_digit() || c == '-' || c == '+')
        .unwrap_or(false)
    {
        let looks_numeric = text
            .trim_start_matches(['+', '-'])
            .chars()
            .all(|c| c.is_ascii_digit() || c == '.');
        if looks_numeric {
            let dt = if text.contains('.') {
                "http://www.w3.org/2001/XMLSchema#decimal"
            } else {
                "http://www.w3.org/2001/XMLSchema#integer"
            };
            let id = crate::dictionary::encode_typed_literal(text, dt);
            return Ok(Term::Const(id));
        }
    }

    // Prefixed IRI: prefix:local — resolve via prefix registry
    if text.contains(':') && !text.contains(' ') {
        let iri = crate::datalog::resolve_prefix(text);
        if iri != text {
            return Ok(Term::Const(crate::datalog::encode_iri(&iri)));
        }
        // Try to encode as-is (may be a full IRI without angle brackets)
        return Ok(Term::Const(crate::datalog::encode_iri(&iri)));
    }

    Err(format!("unrecognized term: {text}"))
}

/// Split a quoted literal string from its type annotation.
/// Returns `(unescaped_value, rest_after_closing_quote)`.
fn split_literal(text: &str) -> Option<(String, &str)> {
    let bytes = text.as_bytes();
    if bytes[0] != b'"' {
        return None;
    }
    let mut i = 1usize;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            i += 2;
        } else if bytes[i] == b'"' {
            let raw = &text[1..i];
            let rest = &text[i + 1..];
            let unescaped = raw
                .replace("\\\"", "\"")
                .replace("\\\\", "\\")
                .replace("\\n", "\n")
                .replace("\\r", "\r")
                .replace("\\t", "\t");
            return Some((unescaped, rest));
        } else {
            i += 1;
        }
    }
    None
}

/// Convert a `Term::Const` to its i64, erroring on non-const terms.
fn term_to_const(term: &Term) -> Result<i64, String> {
    match term {
        Term::Const(id) => Ok(*id),
        Term::Var(name) => Err(format!("variable ?{name} not allowed in quoted triple")),
        Term::Wildcard => Err("wildcard not allowed in quoted triple".to_owned()),
        Term::DefaultGraph => Err("default graph not allowed in quoted triple".to_owned()),
    }
}
