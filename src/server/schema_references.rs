//! Exact schema-reference checks without external resource retrieval.

use referencing::{Draft, Registry, Resource, Uri};
use serde_json::Value;

const LOCAL_ROOT_URI: &str = "apcore://local/output-schema";

/// Advertised schemas may reference local definitions, never external resources.
pub(super) struct LocalSchemaRetriever;

impl jsonschema::Retrieve for LocalSchemaRetriever {
    fn retrieve(
        &self,
        _uri: &jsonschema::Uri<&str>,
    ) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        Err(std::io::Error::other("External schema retrieval is disabled").into())
    }
}

pub(super) fn resolve_all_refs(schema: &Value) -> bool {
    try_resolve_all_refs(schema).is_some()
}

fn try_resolve_all_refs(schema: &Value) -> Option<()> {
    let resource = Resource::from_contents(schema.clone()).ok()?;
    let draft = resource.draft();
    if !matches!(
        draft,
        Draft::Draft7 | Draft::Draft201909 | Draft::Draft202012
    ) {
        return None;
    }
    let registry = Registry::options()
        .retriever(Box::new(LocalSchemaRetriever))
        .try_new(LOCAL_ROOT_URI, resource)
        .ok()?;
    let uri = Uri::parse(LOCAL_ROOT_URI.to_string()).ok()?;
    let mut pending = vec![(schema, registry.resolver(uri), draft)];
    while let Some((schema, resolver, parent_draft)) = pending.pop() {
        let draft = parent_draft.detect(schema).ok()?;
        // A single validator dialect must match every embedded schema resource.
        if draft != parent_draft {
            return None;
        }
        let resolver = resolver
            .in_subresource(draft.create_resource_ref(schema))
            .ok()?;
        for keyword in reference_keywords(draft) {
            if let Some(reference) = schema.get(*keyword).and_then(Value::as_str) {
                resolver.lookup(reference).ok()?;
            }
        }
        for child in draft.subresources_of(schema) {
            pending.push((child, resolver.clone(), draft));
        }
    }
    Some(())
}

fn reference_keywords(draft: Draft) -> &'static [&'static str] {
    match draft {
        Draft::Draft202012 => &["$ref", "$dynamicRef"],
        Draft::Draft201909 => &["$ref", "$recursiveRef"],
        _ => &["$ref"],
    }
}
