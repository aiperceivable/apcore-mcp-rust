//! Cross-language conformance: middleware Config Bus loading.
//!
//! Drives the Rust builder from the shared fixture at
//! `apcore-mcp/conformance/fixtures/middleware_config.json`. The Python and
//! TypeScript bridges run the same fixture through their own builders; all
//! three implementations must agree on the resulting middleware names and on
//! which inputs are rejected.

mod common;

use apcore_mcp::middleware_builder::build_middleware_from_config;
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
    input_entries: Value,
    expected_middleware_names: Vec<String>,
}

#[derive(Deserialize)]
struct ErrorCase {
    id: String,
    input_entries: Value,
    expected_error_substring: String,
}

#[test]
fn conformance_success_cases() {
    let Some(fixture) = common::load_fixture::<Fixture>("middleware_config.json") else {
        return;
    };

    for case in &fixture.test_cases {
        let result = build_middleware_from_config(Some(&case.input_entries));
        let mws = match result {
            Ok(mws) => mws,
            Err(e) => panic!("case {}: unexpected error: {e}", case.id),
        };
        let names: Vec<&str> = mws.iter().map(|mw| mw.name()).collect();
        assert_eq!(
            names, case.expected_middleware_names,
            "case {}: names mismatch",
            case.id
        );
    }
}

#[test]
fn conformance_error_cases() {
    let Some(fixture) = common::load_fixture::<Fixture>("middleware_config.json") else {
        return;
    };

    for case in &fixture.error_cases {
        let result = build_middleware_from_config(Some(&case.input_entries));
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
