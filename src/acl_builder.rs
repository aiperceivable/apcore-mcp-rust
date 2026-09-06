// Build an apcore `ACL` instance from a Config Bus `mcp.acl` section.
//
// Config Bus schema (YAML, shared across Python/TS/Rust bridges). The
// `system.*` modules are apcore's built-in management surface (health,
// usage, manifest, control — see `apcore::sys_modules`); this is the
// reference rule set for gating it (aiperceivable/apcore-mcp#14):
//
// ```yaml
// acl:
//   default_effect: deny
//   rules:
//     # Rule 1 — read-only management surface.
//     # MUST precede the catch-all deny: evaluation is first-match-wins.
//     - callers: ["@external"]
//       targets: ["system.health.*", "system.usage.*", "system.manifest.*"]
//       effect: allow
//       conditions:
//         identity_types: ["human"]
//         roles: ["apcore.admin"]
//       description: "Console read access to the management surface"
//
//     # Rule 2 — administration. ACL allow is not execution:
//     # system.control.* declares requiresApproval=true and still passes the approval gate.
//     - callers: ["@external"]
//       targets: ["system.control.*"]
//       effect: allow
//       conditions:
//         identity_types: ["human"]
//         roles: ["apcore.admin"]
//       description: "Administration; requires_approval still applies"
//
//     # Rule 3 — catch-all deny. MUST be last.
//     # Agent identities, anonymous callers and insufficient roles land here.
//     - callers: ["@external"]
//       targets: ["system.*"]
//       effect: deny
//       description: "Block all other access to system modules"
// ```
//
// Two mechanics worth spelling out, since they are easy to get backwards:
//
// - Every MCP call's `caller_id` is `null`; the ACL normalizes that to the
//   synthetic `@external` caller. So `callers` can never distinguish "the
//   console" from "an agent" — that distinction has to come from
//   `conditions` reading `identity_types` / `roles` off the caller's JWT, as
//   the rules above do.
// - An `allow` rule is authorization, not an execution guarantee: apcore's
//   `approval` gate (declared via the `approval: required` rule key, or a
//   module's own `requires_approval` annotation) still runs afterwards. Rule
//   2 above allows the *call*; whether it also requires human sign-off is a
//   separate, orthogonal decision.
//
// Mirrors the Python `acl_builder.build_acl_from_config` contract. Invalid
// entries return an error so misconfiguration fails loudly at startup.

use apcore::{ACLRule, ApprovalRequirement, ACL};
use serde_json::Value;

use crate::apcore_mcp::APCoreMCPError;

const ALLOWED_EFFECTS: &[&str] = &["allow", "deny"];
const ALLOWED_RULE_KEYS: &[&str] = &[
    "callers",
    "targets",
    "effect",
    "approval",
    "description",
    "conditions",
];
/// The accepted string values for the `approval` rule key — the same closed
/// set `apcore::ApprovalRequirement` deserializes (PROTOCOL_SPEC §6.1.6),
/// where `not_required` is both a spec-sanctioned value and the default.
///
/// This bridge deliberately does **not** narrow the set. An earlier version
/// accepted only `"required"`, on the reasoning that `mcp.acl` should have
/// exactly one spelling for "no approval requirement here". That made the
/// bridge stricter than the schema it bridges: a rule that loads fine from
/// apcore's own `acl/` directory failed at startup when the identical rule
/// was carried through the Config Bus instead. Rejecting `not_required`
/// prevents no misconfiguration — `ACLRule::approval_required()` treats
/// `Some(NotRequired)` and `None` identically — while breaking a valid
/// configuration, so the redundant spelling is accepted and passed through.
const ALLOWED_APPROVALS: &[&str] = &["required", "not_required"];

/// Construct an `apcore::ACL` from a Config Bus `mcp.acl` value.
///
/// Returns `Ok(None)` when `acl_config` is `None`, `Value::Null`, or an empty
/// object (no rules / no default_effect). Returns `Err` on malformed entries.
pub fn build_acl_from_config(acl_config: Option<&Value>) -> Result<Option<ACL>, APCoreMCPError> {
    let Some(cfg) = acl_config else {
        return Ok(None);
    };
    if cfg.is_null() {
        return Ok(None);
    }

    let obj = cfg.as_object().ok_or_else(|| {
        APCoreMCPError::Config(format!(
            "mcp.acl must be a mapping with 'rules' and optional 'default_effect', \
             got {}",
            value_type_name(cfg)
        ))
    })?;

    // Validate `rules` type up-front before early-returning on empty config.
    let rules_val = obj.get("rules");
    if let Some(v) = rules_val {
        if !v.is_array() {
            return Err(APCoreMCPError::Config(format!(
                "mcp.acl.rules must be a list, got {}",
                value_type_name(v)
            )));
        }
    }

    let has_rules = rules_val.is_some_and(|v| v.as_array().is_some_and(|a| !a.is_empty()));
    let has_default = obj.contains_key("default_effect");
    if !has_rules && !has_default {
        return Ok(None);
    }

    let default_effect = obj
        .get("default_effect")
        .and_then(Value::as_str)
        .unwrap_or("deny")
        .to_string();
    if !ALLOWED_EFFECTS.contains(&default_effect.as_str()) {
        return Err(APCoreMCPError::Config(format!(
            "mcp.acl.default_effect must be 'allow' or 'deny', got {default_effect:?}"
        )));
    }

    let raw_rules = rules_val
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut rules: Vec<ACLRule> = Vec::with_capacity(raw_rules.len());
    for (idx, entry) in raw_rules.into_iter().enumerate() {
        let entry_obj = entry.as_object().ok_or_else(|| {
            APCoreMCPError::Config(format!(
                "mcp.acl.rules[{idx}] must be an object, got {}",
                value_type_name(&entry)
            ))
        })?;

        // Unknown keys → hard error.
        let extra: Vec<&str> = entry_obj
            .keys()
            .filter(|k: &&String| !ALLOWED_RULE_KEYS.contains(&k.as_str()))
            .map(String::as_str)
            .collect();
        if !extra.is_empty() {
            let mut sorted = extra.clone();
            sorted.sort_unstable();
            return Err(APCoreMCPError::Config(format!(
                "mcp.acl.rules[{idx}] got unexpected keys: {}",
                sorted.join(", ")
            )));
        }

        // PROTOCOL_SPEC §6.2.1 (apcore 0.29.0, spec v1.31.0) fixes the order in
        // which a rule bad on more than one axis is refused: `effect` ->
        // `approval` -> `callers` -> `targets`, with the rule index dominating.
        // This builder used to run it in reverse, so a rule wrong in both
        // `effect` and `callers` was refused for `callers` here and for
        // `effect` by apcore's own doors — the same file, two answers,
        // depending on which door it reached first. The unknown-key check above
        // stays ahead of all four: a Config-Bus shape fault with no apcore
        // counterpart. `default_effect` is judged ahead of the rule loop.
        let effect = entry_obj
            .get("effect")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                APCoreMCPError::Config(format!(
                    "mcp.acl.rules[{idx}] 'effect' must be 'allow' or 'deny'"
                ))
            })?;
        if !ALLOWED_EFFECTS.contains(&effect) {
            return Err(APCoreMCPError::Config(format!(
                "mcp.acl.rules[{idx}] 'effect' must be 'allow' or 'deny', got {effect:?}"
            )));
        }

        // §6.2.1 puts `approval` second, ahead of the pattern fields.
        if let Some(raw) = entry_obj.get("approval") {
            let ok = raw.is_null()
                || raw
                    .as_str()
                    .is_some_and(|s| ALLOWED_APPROVALS.contains(&s));
            if !ok {
                return Err(APCoreMCPError::Config(format!(
                    "mcp.acl.rules[{idx}] 'approval' must be one of {ALLOWED_APPROVALS:?} \
                     (or omitted), got {raw}"
                )));
            }
        }

        let callers = entry_obj
            .get("callers")
            .and_then(Value::as_array)
            .filter(|a| !a.is_empty())
            .ok_or_else(|| {
                APCoreMCPError::Config(format!(
                    "mcp.acl.rules[{idx}] 'callers' must be a non-empty list"
                ))
            })?;
        let targets = entry_obj
            .get("targets")
            .and_then(Value::as_array)
            .filter(|a| !a.is_empty())
            .ok_or_else(|| {
                APCoreMCPError::Config(format!(
                    "mcp.acl.rules[{idx}] 'targets' must be a non-empty list"
                ))
            })?;

        if let Some(conds) = entry_obj.get("conditions") {
            if !conds.is_null() && !conds.is_object() {
                return Err(APCoreMCPError::Config(format!(
                    "mcp.acl.rules[{idx}] 'conditions' must be an object or null"
                )));
            }
        }

        // apcore 0.28.0 argument-scoped approval (spec §6.1.6, apcore#108):
        // an `allow` rule can additionally require a human decision. Both
        // spellings apcore itself accepts are accepted here — see
        // `ALLOWED_APPROVALS` for why this bridge must not narrow the set.
        // `deny` + `approval: required` has no meaning and is rejected below
        // at `ACL::try_new`, which is also where the rule index and offending
        // value get named in the error.
        let approval = match entry_obj.get("approval") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) if s == "required" => Some(ApprovalRequirement::Required),
            Some(Value::String(s)) if s == "not_required" => Some(ApprovalRequirement::NotRequired),
            Some(other) => {
                return Err(APCoreMCPError::Config(format!(
                    "mcp.acl.rules[{idx}] 'approval' must be one of {ALLOWED_APPROVALS:?} \
                     (or omitted), got {other}"
                )));
            }
        };

        // Reconstruct rule via serde — snake_case field names already match.
        let callers_vec: Vec<String> = callers
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect();
        let targets_vec: Vec<String> = targets
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect();
        let description = entry_obj
            .get("description")
            .and_then(Value::as_str)
            .map(String::from);
        let conditions = entry_obj
            .get("conditions")
            .filter(|v| !v.is_null())
            .cloned();

        // `ACLRule` is `#[non_exhaustive]` as of apcore 0.29.0, so a struct
        // literal no longer compiles from outside that crate. `ACLRule::new`
        // takes the three required fields and the rest are assigned on the
        // returned value — the form apcore's own api-surface-conventions §9.3
        // names as the one that compiles downstream.
        let mut rule = ACLRule::new(callers_vec, targets_vec, effect);
        rule.approval = approval;
        rule.description = description;
        rule.conditions = conditions;

        // Attribute apcore's own refusal to THIS rule. `ACL::try_new` validates
        // the whole list and names an index within it; apcore exports no
        // per-rule validator, so a throwaway single-rule construction is the
        // only way to learn which rule was at fault while we still know its
        // real position.
        if let Err(e) = ACL::try_new(vec![rule.clone()], default_effect.clone(), None) {
            return Err(APCoreMCPError::Config(format!(
                "mcp.acl.rules[{idx}] {e}"
            )));
        }
        rules.push(rule);
    }

    // `try_new` (not `new`): `new` panics on an invalid rule (e.g.
    // `approval: required` on a `deny` effect, apcore#108 §6.1.6 rule 2), and
    // this function's contract is to fail loudly via `Err`, not via a panic
    // that unwinds through a config-loading call site.
    let rule_count = rules.len();
    let acl = ACL::try_new(rules, default_effect.clone(), None)
        .map_err(|e| APCoreMCPError::Config(format!("mcp.acl: {e}")))?;
    // Cross-language parity: the spec's `## Contract: build_acl_from_config`
    // Properties row states "logs at INFO on success" as a cross-language
    // guarantee. Only the Python bridge did this before now — TypeScript and
    // Rust were both silent, an operator debugging via logs saw confirmation
    // on Python only.
    tracing::info!("Built ACL with {rule_count} rule(s), default_effect={default_effect}");
    Ok(Some(acl))
}

fn value_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn none_or_null_returns_none() {
        assert!(build_acl_from_config(None).unwrap().is_none());
        assert!(build_acl_from_config(Some(&Value::Null)).unwrap().is_none());
        assert!(build_acl_from_config(Some(&json!({}))).unwrap().is_none());
    }

    #[test]
    fn builds_acl_with_default_effect_deny() {
        let cfg = json!({
            "default_effect": "deny",
            "rules": [
                {"callers": ["role:admin"], "targets": ["sys.*"], "effect": "allow"}
            ]
        });
        let acl = build_acl_from_config(Some(&cfg)).unwrap().unwrap();
        assert_eq!(acl.rules().len(), 1);
    }

    /// A `MakeWriter` that appends every write into a shared buffer, so a
    /// test can assert on `tracing::info!` output without a global
    /// subscriber leaking into other tests.
    #[derive(Clone, Default)]
    struct CaptureWriter(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for CaptureWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("capture buffer lock").extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CaptureWriter {
        type Writer = CaptureWriter;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    #[test]
    fn logs_at_info_on_success_cross_language_parity_with_python_a_004() {
        // The spec's `## Contract: build_acl_from_config` Properties row
        // states "logs at INFO on success" as a cross-language guarantee.
        // Before this fix, only the Python bridge did — TypeScript and Rust
        // were both silent, so an operator debugging via logs saw
        // confirmation on Python only despite an identical config on all
        // three bridges.
        let buffer = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_writer(CaptureWriter(std::sync::Arc::clone(&buffer)))
            .with_ansi(false)
            .finish();

        let cfg = json!({
            "default_effect": "deny",
            "rules": [{"callers": ["*"], "targets": ["public.*"], "effect": "allow"}]
        });
        tracing::subscriber::with_default(subscriber, || {
            build_acl_from_config(Some(&cfg)).unwrap();
        });

        let output = String::from_utf8(buffer.lock().expect("capture buffer lock").clone())
            .expect("captured tracing output must be valid UTF-8");
        assert!(output.contains("Built ACL with 1 rule(s)"), "got: {output}");
        assert!(output.contains("default_effect=deny"), "got: {output}");
    }

    #[test]
    fn does_not_log_when_the_section_is_absent() {
        let buffer = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_writer(CaptureWriter(std::sync::Arc::clone(&buffer)))
            .with_ansi(false)
            .finish();

        tracing::subscriber::with_default(subscriber, || {
            build_acl_from_config(None).unwrap();
        });

        let output = String::from_utf8(buffer.lock().expect("capture buffer lock").clone())
            .expect("captured tracing output must be valid UTF-8");
        assert!(output.is_empty(), "expected no log output, got: {output}");
    }

    #[test]
    fn default_effect_defaults_to_deny_when_omitted() {
        let cfg = json!({
            "rules": [
                {"callers": ["*"], "targets": ["public.*"], "effect": "allow"}
            ]
        });
        let acl = build_acl_from_config(Some(&cfg)).unwrap().unwrap();
        assert_eq!(acl.rules().len(), 1);
    }

    #[test]
    fn rule_with_description_and_conditions() {
        let cfg = json!({
            "rules": [
                {
                    "callers": ["role:admin"],
                    "targets": ["sys.*"],
                    "effect": "allow",
                    "description": "admin access",
                    "conditions": {"identity_types": ["human"]}
                }
            ]
        });
        let acl = build_acl_from_config(Some(&cfg)).unwrap().unwrap();
        let rule = &acl.rules()[0];
        assert_eq!(rule.description.as_deref(), Some("admin access"));
        assert!(rule.conditions.is_some());
    }

    #[test]
    fn invalid_default_effect_errors() {
        let cfg = json!({"default_effect": "maybe", "rules": []});
        let err = build_acl_from_config(Some(&cfg)).unwrap_err();
        assert!(
            format!("{err}").contains("default_effect must be"),
            "got: {err}"
        );
    }

    #[test]
    fn missing_callers_errors() {
        let cfg = json!({
            "rules": [{"targets": ["x.*"], "effect": "allow"}]
        });
        let err = build_acl_from_config(Some(&cfg)).unwrap_err();
        assert!(
            format!("{err}").contains("'callers' must be a non-empty list"),
            "got: {err}"
        );
    }

    #[test]
    fn missing_targets_errors() {
        let cfg = json!({
            "rules": [{"callers": ["*"], "effect": "allow"}]
        });
        let err = build_acl_from_config(Some(&cfg)).unwrap_err();
        assert!(
            format!("{err}").contains("'targets' must be a non-empty list"),
            "got: {err}"
        );
    }

    #[test]
    fn invalid_effect_errors() {
        let cfg = json!({
            "rules": [{"callers": ["*"], "targets": ["*"], "effect": "maybe"}]
        });
        let err = build_acl_from_config(Some(&cfg)).unwrap_err();
        assert!(
            format!("{err}").contains("'effect' must be 'allow' or 'deny'"),
            "got: {err}"
        );
    }

    #[test]
    fn unknown_rule_keys_error() {
        let cfg = json!({
            "rules": [
                {"callers": ["*"], "targets": ["*"], "effect": "allow", "bogus": true}
            ]
        });
        let err = build_acl_from_config(Some(&cfg)).unwrap_err();
        assert!(format!("{err}").contains("unexpected keys"), "got: {err}");
    }

    #[test]
    fn non_object_top_level_errors() {
        let cfg = json!("deny");
        let err = build_acl_from_config(Some(&cfg)).unwrap_err();
        assert!(format!("{err}").contains("must be a mapping"), "got: {err}");
    }

    #[test]
    fn rules_non_array_errors() {
        let cfg = json!({"rules": "oops"});
        let err = build_acl_from_config(Some(&cfg)).unwrap_err();
        assert!(
            format!("{err}").contains("rules must be a list"),
            "got: {err}"
        );
    }

    // ---- `approval` rule key (apcore 0.28.0, apcore#108, aiperceivable/apcore-mcp#14) ----

    #[test]
    fn approval_required_on_allow_rule_parses() {
        let cfg = json!({
            "rules": [
                {
                    "callers": ["@external"],
                    "targets": ["system.control.*"],
                    "effect": "allow",
                    "approval": "required"
                }
            ]
        });
        let acl = build_acl_from_config(Some(&cfg)).unwrap().unwrap();
        let rule = &acl.rules()[0];
        assert_eq!(rule.approval, Some(apcore::ApprovalRequirement::Required));
    }

    #[test]
    fn approval_absent_means_not_required() {
        let cfg = json!({
            "rules": [
                {"callers": ["*"], "targets": ["public.*"], "effect": "allow"}
            ]
        });
        let acl = build_acl_from_config(Some(&cfg)).unwrap().unwrap();
        assert_eq!(acl.rules()[0].approval, None);
    }

    #[test]
    fn approval_unknown_string_errors() {
        let cfg = json!({
            "rules": [
                {
                    "callers": ["*"],
                    "targets": ["*"],
                    "effect": "allow",
                    "approval": "maybe"
                }
            ]
        });
        let err = build_acl_from_config(Some(&cfg)).unwrap_err();
        assert!(
            format!("{err}").contains("'approval' must be one of"),
            "got: {err}"
        );
    }

    #[test]
    fn approval_not_required_spelling_is_accepted() {
        // `not_required` is spec-sanctioned (PROTOCOL_SPEC §6.1.6) and is
        // apcore's own `ApprovalRequirement::default()`, so a rule carrying it
        // loads fine from apcore's `acl/` directory. This bridge previously
        // rejected it as a redundant spelling, which meant the identical rule
        // failed at startup when carried through the Config Bus instead — a
        // bridge must not narrow the schema it bridges.
        let cfg = json!({
            "rules": [
                {
                    "callers": ["*"],
                    "targets": ["*"],
                    "effect": "allow",
                    "approval": "not_required"
                }
            ]
        });
        let acl = build_acl_from_config(Some(&cfg)).unwrap().unwrap();
        assert_eq!(
            acl.rules()[0].approval,
            Some(apcore::ApprovalRequirement::NotRequired)
        );
        // ...and it composes to the same verdict as omitting the key.
        assert!(!acl.rules()[0].approval_required());
    }

    #[test]
    fn approval_required_on_deny_rule_errors() {
        // apcore#108 §6.1.6 rule 2: `approval: required` on a `deny` effect
        // is meaningless and rejected by `ACL::try_new`. This asserts the
        // rejection surfaces as a `Config` error, not a panic — `ACL::new`
        // panics on this combination, and `build_acl_from_config` must go
        // through `try_new` instead so misconfiguration fails loudly via
        // `Err`, matching the module's documented contract.
        let cfg = json!({
            "rules": [
                {
                    "callers": ["*"],
                    "targets": ["system.control.*"],
                    "effect": "deny",
                    "approval": "required"
                }
            ]
        });
        let err = build_acl_from_config(Some(&cfg)).unwrap_err();
        assert!(format!("{err}").contains("mcp.acl"), "got: {err}");
    }

    // ---- ACL rule template <-> real sys_modules parity (aiperceivable/apcore-mcp#14) ----

    /// Every `targets` pattern in this module's doc-comment ACL template
    /// must match at least one module id that `apcore::sys_modules::
    /// register_sys_modules` actually registers. A prefix drifting out of
    /// sync with the real module ids would silently leave the documented
    /// template gating nothing.
    #[test]
    fn acl_template_targets_match_real_sys_module_ids() {
        use std::sync::Arc;

        use apcore::config::Config;
        use apcore::executor::Executor;
        use apcore::registry::Registry;
        use apcore::sys_modules::register_sys_modules;

        let registry = Arc::new(Registry::new());
        let mut config = Config::default();
        config.set("sys_modules.enabled", json!(true));
        // Control modules (system.control.*) only register with events
        // enabled — needed so the "system.control.*" pattern below has a
        // real module id to match.
        config.set("sys_modules.events.enabled", json!(true));
        let executor = Executor::new(Arc::clone(&registry), Config::default());

        register_sys_modules(Arc::clone(&registry), &executor, &config, None)
            .expect("register_sys_modules should succeed with sys_modules.enabled=true");

        let registered_ids = registry.list(None, None, None);
        assert!(
            !registered_ids.is_empty(),
            "sanity: register_sys_modules should have registered at least one module"
        );

        // Patterns copied verbatim from the module doc comment's ACL
        // template `targets` lists.
        let patterns = [
            "system.health.*",
            "system.usage.*",
            "system.manifest.*",
            "system.control.*",
        ];
        for pattern in patterns {
            let prefix = pattern
                .strip_suffix('*')
                .expect("template patterns are prefix globs ending in '*'");
            assert!(
                registered_ids.iter().any(|id| id.starts_with(prefix)),
                "template pattern {pattern:?} matched no module id registered by \
                 register_sys_modules; registered ids were: {registered_ids:?}"
            );
        }
    }
}
