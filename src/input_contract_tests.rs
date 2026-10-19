//! Issue #20: exercise the real Registry, Executor and shipped router adapter.
use super::*;
use apcore::registry::ModuleDescriptor;
use apcore::{Config, Module, ModuleAnnotations};
use serde_json::json;

#[path = "../tests/common/mod.rs"]
mod common;

#[derive(Debug)]
struct Echo {
    schema: Value,
}
#[async_trait::async_trait]
impl Module for Echo {
    fn description(&self) -> &str {
        "Echo validated inputs and preserve an omitted limit's default"
    }
    fn input_schema(&self) -> Value {
        self.schema.clone()
    }
    fn output_schema(&self) -> Value {
        json!({"type": "object"})
    }
    async fn execute(
        &self,
        inputs: Value,
        _context: &apcore::Context<Value>,
    ) -> Result<Value, apcore::ModuleError> {
        Ok(json!({"limit": inputs.get("limit").cloned().unwrap_or(json!(7)), "received": inputs}))
    }
}

#[tokio::test]
async fn mcp_input_contract() {
    let fixture: Value =
        common::load_fixture("mcp_input_contract.json").expect("canonical input contract fixture");
    for case in fixture["schema_cases"].as_array().expect("schema cases") {
        for validate_inputs in [false, true] {
            let schema = case["input_schema"].clone();
            let registry = Arc::new(Registry::new());
            let module = Echo {
                schema: schema.clone(),
            };
            let descriptor = descriptor_for(&module);
            registry
                .register("demo.echo", Box::new(module), descriptor)
                .unwrap();
            let factory = MCPServerFactory::new();
            let mut tool = factory
                .build_tools(&registry, None, None)
                .unwrap()
                .remove(0);
            let mut expected = schema.clone();
            if expected.as_object().unwrap().is_empty() {
                expected = json!({"properties": {}});
            }
            expected["type"] = json!("object");
            assert_eq!(tool.input_schema, expected, "{}", case["id"]);
            let validator = jsonschema::validator_for(&tool.input_schema).unwrap();
            let executor = Arc::new(Executor::new(
                Arc::clone(&registry),
                Arc::new(Config::default()),
            ));
            let router = ExecutionRouter::new(
                Box::new(ApcoreExecutorAdapter {
                    inner: Arc::clone(&executor),
                    strategy: None,
                }),
                validate_inputs,
                None,
            );
            for sample in case["samples"].as_array().unwrap() {
                let arguments = sample["arguments"].clone();
                let valid = sample["valid"].as_bool().unwrap();
                assert_eq!(
                    validator.is_valid(&arguments),
                    valid,
                    "{} / {}",
                    case["id"],
                    sample["id"]
                );
                assert_eq!(
                    executor
                        .validate("demo.echo", &arguments, None)
                        .await
                        .unwrap()
                        .valid,
                    valid,
                    "{} / {}",
                    case["id"],
                    sample["id"]
                );
                let direct = executor
                    .call("demo.echo", arguments.clone(), None, None)
                    .await;
                assert_eq!(direct.is_ok(), valid, "{} / {}", case["id"], sample["id"]);
                let (content, is_error, _) =
                    router.handle_call("demo.echo", &arguments, None).await;
                assert_eq!(
                    is_error, !valid,
                    "{} / {}: {:?}",
                    case["id"], sample["id"], content
                );
                if valid {
                    let actual: Value =
                        serde_json::from_str(content[0].data.as_str().unwrap()).unwrap();
                    assert_eq!(actual, direct.unwrap(), "{} / {}", case["id"], sample["id"]);
                    assert_eq!(actual["received"], sample["arguments"]);
                    if let Some(limit) = arguments.get("limit") {
                        assert_eq!(&actual["limit"], limit);
                    }
                }
                assert_eq!(arguments, sample["arguments"]);
            }
            if let Some(properties) = tool
                .input_schema
                .get_mut("properties")
                .and_then(Value::as_object_mut)
            {
                if let Some(property) = properties.values_mut().next() {
                    property["description"] = json!("Changed on the advertised copy");
                }
            }
            tool.input_schema["properties"] = json!({"changed": {"type": "string"}});
            assert_eq!(
                registry
                    .get_definition("demo.echo")
                    .unwrap()
                    .unwrap()
                    .input_schema,
                schema
            );
        }
    }
}

fn descriptor_for(module: &Echo) -> ModuleDescriptor {
    ModuleDescriptor {
        module_id: "demo.echo".into(),
        name: None,
        description: module.description().into(),
        documentation: None,
        input_schema: module.schema.clone(),
        output_schema: module.output_schema(),
        version: "1.0.0".into(),
        tags: vec![],
        annotations: Some(ModuleAnnotations::default()),
        examples: vec![],
        metadata: Default::default(),
        display: None,
        sunset_date: None,
        dependencies: vec![],
        enabled: true,
    }
}

#[tokio::test]
async fn mcp_root_schema_contract() {
    let fixture: Value = common::load_fixture("mcp_input_contract.json").unwrap();
    for case in fixture["root_schema_cases"].as_array().unwrap() {
        let schema = case["input_schema"].clone();
        let registry = Arc::new(Registry::new());
        let module = Echo {
            schema: schema.clone(),
        };
        let descriptor = descriptor_for(&module);
        registry
            .register("demo.echo", Box::new(module), descriptor.clone())
            .unwrap();
        let factory = MCPServerFactory::new();
        let result = factory.build_tool(&descriptor, "Echo inputs", None);
        if case["accepted"].as_bool().unwrap() {
            let tool = result.unwrap();
            assert_eq!(tool.input_schema["type"], "object");
            assert_eq!(factory.build_tools(&registry, None, None).unwrap().len(), 1);
            assert!(jsonschema::validator_for(&schema)
                .unwrap()
                .is_valid(&json!({})));
            assert!(jsonschema::validator_for(&tool.input_schema)
                .unwrap()
                .is_valid(&json!({})));
            let executor = Executor::new(Arc::clone(&registry), Arc::new(Config::default()));
            assert!(
                executor
                    .validate("demo.echo", &json!({}), None)
                    .await
                    .unwrap()
                    .valid
            );
        } else {
            assert!(result
                .unwrap_err()
                .to_string()
                .contains("MCP input schema must allow an object root"));
            assert!(factory
                .build_tools(&registry, None, None)
                .unwrap()
                .is_empty());
        }
        assert_eq!(
            registry
                .get_definition("demo.echo")
                .unwrap()
                .unwrap()
                .input_schema,
            schema
        );
    }
}
