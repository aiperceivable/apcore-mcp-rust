//! Cross-language conformance: the nine canonical system.* modules -> exact MCP primitive.
//!
//! Drives `MCPServerFactory::build_tools` / `register_resource_handlers` from
//! the shared fixture at `apcore-mcp/conformance/fixtures/system_surface.json`,
//! against a registry built by a REAL `apcore::sys_modules::register_sys_modules`
//! call (the same pattern `src/acl_builder.rs`'s
//! `acl_template_targets_match_real_sys_module_ids` test uses for
//! aiperceivable/apcore-mcp#14). The Python and TypeScript bridges run the
//! identical fixture through their own factories against their own real
//! `registerSysModules` / `register_sys_modules`; all three must agree
//! byte-for-byte on which module ids become tools, which become resources,
//! which become resource templates, and the exact name/URI each one gets
//! (aiperceivable/apcore-mcp#15's "byte-identical tools/list, resources/list,
//! resources/templates/list" acceptance criterion).

mod common;

use std::collections::HashSet;
use std::sync::Arc;

use apcore::config::Config;
use apcore::executor::Executor;
use apcore::registry::Registry;
use apcore::sys_modules::register_sys_modules;
use apcore_mcp::server::factory::MCPServerFactory;
use apcore_mcp::ExecutionRouter;
use serde::Deserialize;

#[derive(Deserialize)]
struct Fixture {
    tools: Vec<ToolEntry>,
    not_tools: Vec<String>,
    resources: Vec<ResourceEntry>,
    resource_templates: Vec<TemplateEntry>,
}

#[derive(Deserialize)]
struct ToolEntry {
    module_id: String,
    name: String,
}

#[derive(Deserialize)]
struct ResourceEntry {
    module_id: String,
    uri: String,
}

#[derive(Deserialize)]
struct TemplateEntry {
    module_id: String,
    uri_template: String,
}

/// Build a registry with all nine canonical `system.*` modules registered,
/// via the same real `register_sys_modules` call
/// `acl_template_targets_match_real_sys_module_ids` uses.
fn real_registry_and_executor() -> (Arc<Registry>, Executor) {
    let registry = Arc::new(Registry::new());
    let mut config = Config::default();
    config.set("sys_modules.enabled", serde_json::json!(true));
    config.set("sys_modules.events.enabled", serde_json::json!(true));
    let executor = Executor::new(Arc::clone(&registry), Config::default());

    register_sys_modules(Arc::clone(&registry), &executor, &config, None)
        .expect("register_sys_modules should succeed with sys_modules.enabled=true");

    (registry, executor)
}

#[test]
fn system_control_modules_are_tools() {
    let Some(fixture) = common::load_fixture::<Fixture>("system_surface.json") else {
        return;
    };
    let (registry, _executor) = real_registry_and_executor();

    let factory = MCPServerFactory::new();
    let tools = factory
        .build_tools(&registry, None, None)
        .expect("build_tools should not fail");
    let tool_names: HashSet<&str> = tools.iter().map(|t| t.name.as_str()).collect();

    for expected in &fixture.tools {
        assert!(
            tool_names.contains(expected.name.as_str()),
            "{} missing from tools/list (got {tool_names:?})",
            expected.module_id
        );
    }
}

#[test]
fn readonly_system_modules_are_not_tools() {
    let Some(fixture) = common::load_fixture::<Fixture>("system_surface.json") else {
        return;
    };
    let (registry, _executor) = real_registry_and_executor();

    let factory = MCPServerFactory::new();
    let tools = factory
        .build_tools(&registry, None, None)
        .expect("build_tools should not fail");
    let tool_names: HashSet<&str> = tools.iter().map(|t| t.name.as_str()).collect();

    for module_id in &fixture.not_tools {
        assert!(
            !tool_names.contains(module_id.as_str()),
            "{module_id} must not be projected as a tool"
        );
    }
}

#[test]
fn readonly_system_modules_are_resources_and_templates() {
    let Some(fixture) = common::load_fixture::<Fixture>("system_surface.json") else {
        return;
    };
    // The router is only exercised by `read_resource`, which this test never
    // calls (it lists resources/templates only) — `ExecutionRouter::stub()`
    // is the same substitute `test_end_to_end_resource_read` in
    // `src/server/factory.rs` uses for exactly that reason.
    let (registry, _executor) = real_registry_and_executor();

    let factory = MCPServerFactory::new();
    let mut server = factory
        .create_server("conformance-test", "1.0.0")
        .expect("create_server should not fail");
    factory.register_resource_handlers(&mut server, &registry, Arc::new(ExecutionRouter::stub()));

    let resources = server
        .list_resources()
        .expect("list_resources handler should be registered");
    let resource_uris: HashSet<&str> = resources.iter().map(|r| r.uri.as_str()).collect();

    for expected in &fixture.resources {
        assert!(
            resource_uris.contains(expected.uri.as_str()),
            "{} missing from resources/list (got {resource_uris:?})",
            expected.module_id
        );
    }

    let templates = server
        .list_resource_templates()
        .expect("list_resource_templates handler should be registered");
    let template_uris: HashSet<&str> = templates.iter().map(|t| t.uri_template.as_str()).collect();

    for expected in &fixture.resource_templates {
        assert!(
            template_uris.contains(expected.uri_template.as_str()),
            "{} missing from resources/templates/list (got {template_uris:?})",
            expected.module_id
        );
    }
}
