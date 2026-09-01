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
/// The only accepted string value for the `approval` rule key. Anything
/// else (including the technically-valid `"not_required"` — the same
/// meaning as omitting the key) is rejected: `mcp.acl` should have exactly
/// one spelling for "no approval requirement here", which is not writing
/// the key at all.
const APPROVAL_REQUIRED: &str = "required";

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

        // Validate callers/targets/effect shape before handing to serde.
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

        if let Some(conds) = entry_obj.get("conditions") {
            if !conds.is_null() && !conds.is_object() {
                return Err(APCoreMCPError::Config(format!(
                    "mcp.acl.rules[{idx}] 'conditions' must be an object or null"
                )));
            }
        }

        // apcore 0.28.0 argument-scoped approval (spec §6.1.6, apcore#108):
        // an `allow` rule can additionally require a human decision. The only
        // accepted spelling is the string "required"; the field is otherwise
        // absent — there is no separate "not_required" the operator should
        // ever need to write. `deny` + `approval: required` has no meaning
        // and is rejected below at `ACL::try_new`, which is also where the
        // rule index and offending value get named in the error.
        let approval = match entry_obj.get("approval") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) if s == APPROVAL_REQUIRED => Some(ApprovalRequirement::Required),
            Some(other) => {
                return Err(APCoreMCPError::Config(format!(
                    "mcp.acl.rules[{idx}] 'approval' must be the string \"required\" \
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

        rules.push(ACLRule {
            callers: callers_vec,
            targets: targets_vec,
            effect: effect.to_string(),
            approval,
            description,
            conditions,
        });
    }

    // `try_new` (not `new`): `new` panics on an invalid rule (e.g.
    // `approval: required` on a `deny` effect, apcore#108 §6.1.6 rule 2), and
    // this function's contract is to fail loudly via `Err`, not via a panic
    // that unwinds through a config-loading call site.
    let acl = ACL::try_new(rules, default_effect, None)
        .map_err(|e| APCoreMCPError::Config(format!("mcp.acl: {e}")))?;
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
            format!("{err}").contains("'approval' must be the string"),
            "got: {err}"
        );
    }

    #[test]
    fn approval_not_required_spelling_is_rejected() {
        // Only "required" is accepted; the explicit "not_required" spelling
        // (apcore's own serde form for the absent key) is refused so there is
        // exactly one way to write "no approval requirement here": omit the
        // key.
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
        let err = build_acl_from_config(Some(&cfg)).unwrap_err();
        assert!(
            format!("{err}").contains("'approval' must be the string"),
            "got: {err}"
        );
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
