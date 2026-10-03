//! Tests for the Datalog parser (extracted from parser.rs v0.122.0 H17-02).

use super::*;
use pgrx::prelude::*;

#[test]
fn test_tokenize_simple() {
    let text = "?x <p> ?y :- ?x <q> ?z . ?a <b> ?c :- ?d <e> ?f .";
    let rules = tokenize_rules(text);
    assert_eq!(rules.len(), 2);
}

#[test]
fn test_tokenize_with_literal() {
    let text = r#"?x <p> "hello.world" :- ?x <q> ?z ."#;
    let rules = tokenize_rules(text);
    assert_eq!(rules.len(), 1, "dot inside literal should not split");
}

#[test]
fn test_tokenize_comment() {
    let text = "# this is a comment\n?x <p> ?y :- ?x <q> ?z .";
    let rules = tokenize_rules(text);
    assert_eq!(rules.len(), 1);
}

#[test]
fn test_find_neck() {
    let rule = "?x <p> ?y :- ?x <q> ?z";
    let pos = find_neck(rule).unwrap();
    assert_eq!(&rule[pos..pos + 2], ":-");
}

#[test]
fn test_parse_comparison() {
    let lit = "?a > 18";
    let result = try_parse_comparison(lit);
    assert!(result.is_some());
    if let Some(BodyLiteral::Compare(_, op, _)) = result {
        assert_eq!(op, CompareOp::Gt);
    }
}

#[test]
fn test_split_body_simple() {
    let body = "?x <p> ?y, ?y <q> ?z";
    let parts = split_body(body);
    assert_eq!(parts.len(), 2);
}

// ─── v0.131.0 (VAL-208): @name annotation and stable rule identity ──────────
//
// These tests parse real rules (IRIs resolve through _pg_ripple.dictionary),
// so they run as #[pg_test] — plain unit tests cannot reach the dictionary.

#[pg_test]
fn test_parse_name_annotation() {
    let rs = parse_rules("?x <p> ?y :- ?x <q> ?z @name(\"dep\")", "t").unwrap();
    assert_eq!(rs.rules.len(), 1);
    assert_eq!(rs.rules[0].name.as_deref(), Some("dep"));
    // The annotation is stripped from the stored rule text.
    assert!(!rs.rules[0].rule_text.contains("@name"));
}

#[pg_test]
fn test_parse_name_and_weight_annotations() {
    let rs = parse_rules("?x <p> ?y :- ?x <q> ?z @weight(0.5) @name(\"w\")", "t").unwrap();
    assert_eq!(rs.rules[0].name.as_deref(), Some("w"));
    assert_eq!(rs.rules[0].weight, Some(0.5));
}

#[pg_test]
fn test_parse_duplicate_name_rejected() {
    let err = parse_rules(
        "?x <p> ?y :- ?x <q> ?z @name(\"dup\") . ?a <b> ?c :- ?a <d> ?e @name(\"dup\") .",
        "t",
    );
    let err = err.err().expect("duplicate @name must be rejected");
    assert!(err.contains("PT0302"), "unexpected error: {err}");
}

#[pg_test]
fn test_rule_display_name_auto_fingerprint() {
    let rs = parse_rules("?x <p> ?y :- ?x <q> ?z", "t").unwrap();
    let name = crate::datalog::rule_display_name(&rs.rules[0]);
    assert!(name.starts_with("auto:"), "unexpected name: {name}");
    assert_eq!(name.len(), 17, "'auto:' + 12 hex chars, got: {name}");
    // Deterministic across reloads of the same text.
    let again = parse_rules("?x <p> ?y :- ?x <q> ?z", "t").unwrap();
    assert_eq!(name, crate::datalog::rule_display_name(&again.rules[0]));
}

#[pg_test]
fn test_rule_display_name_explicit_wins() {
    let rs = parse_rules("?x <p> ?y :- ?x <q> ?z @name(\"label\")", "t").unwrap();
    assert_eq!(crate::datalog::rule_display_name(&rs.rules[0]), "label");
}
