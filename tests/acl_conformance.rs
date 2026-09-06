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
    /// contract_version 1.0/1.1 form: a single fragment.
    #[serde(default)]
    expected_error_substring: Option<String>,
    /// contract_version 1.2 form: every fragment must appear. It exists
    /// because a §6.2.1 shape case has to pin BOTH the bridge's
    /// `mcp.acl.rules[i]` prefix and the axis named by the reason.
    #[serde(default)]
    expected_error_substrings: Vec<String>,
    /// The offending field. Pinned INSTEAD of a reason phrase: the reason is
    /// apcore's, and apcore-rust, apcore-js and apcore-python word the same
    /// fault entirely differently.
    #[serde(default)]
    expected_error_names_field: Option<String>,
    /// Asserts a fragment is ABSENT — how the ordering case separates "named
    /// the right axis" from "rejected something".
    #[serde(default)]
    must_not_contain: Option<String>,
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
            Ok(_) => panic!("case {}: expected a rejection but build succeeded", case.id),
        };

        let mut expected: Vec<&str> = Vec::new();
        if let Some(single) = case.expected_error_substring.as_deref() {
            expected.push(single);
        }
        expected.extend(case.expected_error_substrings.iter().map(String::as_str));
        assert!(
            !expected.is_empty(),
            "case {}: fixture case carries no expectation",
            case.id
        );
        for fragment in expected {
            assert!(
                err.contains(fragment),
                "case {}: error message {:?} missing substring {:?}",
                case.id,
                err,
                fragment,
            );
        }

        if let Some(field) = case.expected_error_names_field.as_deref() {
            // The BARE name, not the quoted form: apcore-python and apcore-js
            // write `'callers'` while apcore-rust writes `'callers[1]'`,
            // naming the offending element. The bare token is the only
            // spelling all three share.
            assert!(
                err.contains(field),
                "case {}: error message {:?} does not name the field {:?}",
                case.id,
                err,
                field,
            );
        }

        if let Some(forbidden) = case.must_not_contain.as_deref() {
            assert!(
                !err.contains(forbidden),
                "case {}: error message {:?} contains {:?}, which means the wrong validation \
                 axis was reported (PROTOCOL_SPEC §6.2.1 order)",
                case.id,
                err,
                forbidden,
            );
        }
    }
}
