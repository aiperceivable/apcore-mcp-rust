//! Cross-language conformance: ACL Config Bus loading.
//!
//! Drives the Rust builder from the shared fixture at
//! `apcore-mcp/conformance/fixtures/acl_config.json`. The Python and
//! TypeScript bridges run the same fixture through their own builders; all
//! three implementations must agree on (rule_count, default_effect) and on
//! which inputs are rejected.

mod common;

use apcore_mcp::acl_builder::build_acl_from_config;
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize)]
struct Fixture {
    test_cases: Vec<SuccessCase>,
    error_cases: Vec<ErrorCase>,
}

#[derive(Deserialize)]
struct SuccessCase {
    id: String,
    input: Value,
    expected_acl: Option<SuccessExpected>,
}

#[derive(Deserialize)]
struct SuccessExpected {
    rule_count: usize,
    default_effect: String,
}

#[derive(Deserialize)]
struct ErrorCase {
    id: String,
    input: Value,
    expected_error_substring: String,
}

#[test]
fn conformance_success_cases() {
    let Some(fixture) = common::load_fixture::<Fixture>("acl_config.json") else {
        return;
    };

    for case in &fixture.test_cases {
        let result = build_acl_from_config(Some(&case.input));
        let acl_opt = match result {
            Ok(opt) => opt,
            Err(e) => panic!("case {}: unexpected error: {e}", case.id),
        };
        match (&case.expected_acl, acl_opt) {
            (None, None) => {}
            (None, Some(_)) => panic!("case {}: expected no ACL", case.id),
            (Some(_), None) => panic!("case {}: expected ACL, got None", case.id),
            (Some(expected), Some(acl)) => {
                assert_eq!(
                    acl.rules().len(),
                    expected.rule_count,
                    "case {}: rule_count mismatch",
                    case.id
                );
                // ACL does not expose `default_effect` publicly; check via the
                // behavior path — evaluate a caller/target with no matching
                // rule and observe the decision. We use a randomised pattern
                // unlikely to match any rule.
                let decision = acl.check(Some("@conformance_probe"), "no_such_module", None);
                let observed = if decision { "allow" } else { "deny" };
                assert_eq!(
                    observed, expected.default_effect,
                    "case {}: default_effect mismatch (probed via check())",
                    case.id
                );
            }
        }
    }
}

#[test]
fn conformance_error_cases() {
    let Some(fixture) = common::load_fixture::<Fixture>("acl_config.json") else {
        return;
    };

    for case in &fixture.error_cases {
        let result = build_acl_from_config(Some(&case.input));
        let err = match result {
            Err(e) => format!("{e}"),
            Ok(_) => panic!(
                "case {}: expected error containing {:?} but build succeeded",
                case.id, case.expected_error_substring
            ),
        };
        assert!(
            err.contains(&case.expected_error_substring),
            "case {}: error message {:?} missing substring {:?}",
            case.id,
            err,
            case.expected_error_substring,
        );
    }
}
