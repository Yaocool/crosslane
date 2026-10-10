//! Adapt tool schemas to gateways that reject root schema combinators.

use serde_json::{json, Map, Value};

/// An always-true conditional evaluates the original constraints on the same
/// instance, including annotations used by `unevaluatedProperties`. Keep local
/// definitions at the root so existing `$ref` targets continue to resolve.
pub fn compatible_tool_schema(schema: &Value) -> Value {
    let Some(original) = schema.as_object() else {
        return schema.clone();
    };
    if !["oneOf", "anyOf", "allOf"]
        .iter()
        .any(|key| original.contains_key(*key))
    {
        return schema.clone();
    }
    let mut root = original.clone();
    let mut constraints = Map::new();
    let mut moved = Vec::new();
    for key in ["oneOf", "anyOf", "allOf", "if", "then", "else"] {
        if let Some(value) = root.remove(key) {
            constraints.insert(key.into(), value);
            moved.push(key);
        }
    }
    let old_draft = original
        .get("$schema")
        .and_then(Value::as_str)
        .is_some_and(|draft| draft.contains("draft-04") || draft.contains("draft-06"));
    if old_draft {
        // Drafts 4/6 have no conditionals or unevaluated-properties annotations.
        if let Some(original_not) = root.remove("not") {
            constraints.insert("not".into(), original_not);
            moved.push("not");
        }
        root.insert("not".into(), json!({"not":constraints}));
    } else {
        root.insert("if".into(), json!(true));
        root.insert("then".into(), Value::Object(constraints));
    }
    let mut result = Value::Object(root);
    rewrite_refs(
        &mut result,
        &moved,
        if old_draft { "not/not" } else { "then" },
    );
    result
}

fn rewrite_refs(value: &mut Value, moved: &[&str], destination: &str) {
    rewrite_schema_refs(value, moved, destination, true);
}

fn rewrite_schema_refs(value: &mut Value, moved: &[&str], destination: &str, root: bool) {
    match value {
        Value::Object(object) => {
            // A nested schema resource owns its fragment references. Rewriting
            // those would point at this resource's unrelated `then` keyword.
            if !root
                && ["$id", "id"].iter().any(|key| {
                    object
                        .get(*key)
                        .and_then(Value::as_str)
                        .is_some_and(|id| !id.is_empty() && !id.starts_with('#'))
                })
            {
                return;
            }
            for keyword in ["$ref", "$dynamicRef"] {
                if let Some(Value::String(reference)) = object.get_mut(keyword) {
                    if let Some(pointer) = reference.strip_prefix("#/") {
                        let first = pointer.split('/').next().unwrap_or_default();
                        if moved.contains(&first) {
                            *reference = format!("#/{destination}/{pointer}");
                        }
                    }
                }
            }
            // Walk schema locations only. Objects inside `const`, `enum`, or
            // annotations are instance data and must remain byte-for-byte equal.
            for keyword in [
                "$defs",
                "definitions",
                "properties",
                "patternProperties",
                "dependentSchemas",
                "dependencies",
            ] {
                if let Some(Value::Object(schemas)) = object.get_mut(keyword) {
                    for child in schemas.values_mut() {
                        rewrite_schema_refs(child, moved, destination, false);
                    }
                }
            }
            for keyword in [
                "items",
                "prefixItems",
                "additionalItems",
                "contains",
                "additionalProperties",
                "propertyNames",
                "unevaluatedProperties",
                "unevaluatedItems",
                "if",
                "then",
                "else",
                "not",
                "allOf",
                "anyOf",
                "oneOf",
                "contentSchema",
            ] {
                if let Some(child) = object.get_mut(keyword) {
                    rewrite_schema_refs(child, moved, destination, false);
                }
            }
        }
        Value::Array(values) => {
            for child in values {
                rewrite_schema_refs(child, moved, destination, false);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn composition_keeps_definitions_closed_branches_and_existing_conditionals() {
        let schema = json!({"type":"object", "$defs":{"id":{"type":"string"}},
            "oneOf":[{"properties":{"id":{"$ref":"#/$defs/id"}},"required":["id"],"additionalProperties":false}],
            "if":{"required":["mode"]},"then":{"required":["id"]},"else":{"maxProperties":1},
            "unevaluatedProperties":false});
        let adapted = compatible_tool_schema(&schema);
        assert_eq!(adapted["$defs"], schema["$defs"]);
        assert_eq!(adapted["then"]["oneOf"], schema["oneOf"]);
        assert_eq!(adapted["then"]["if"], schema["if"]);
        assert_eq!(adapted["then"]["then"], schema["then"]);
        assert_eq!(adapted["then"]["else"], schema["else"]);
        assert_eq!(adapted["unevaluatedProperties"], false);
        assert_eq!(compatible_tool_schema(&adapted), adapted);
    }

    #[test]
    fn references_into_moved_constraints_follow_the_new_location() {
        let schema =
            json!({"oneOf":[{"type":"object"}],"properties":{"child":{"$ref":"#/oneOf/0"}}});
        assert_eq!(
            compatible_tool_schema(&schema)["properties"]["child"]["$ref"],
            "#/then/oneOf/0"
        );
        let ordinary = json!({"type":"object","properties":{"value":{"anyOf":[{"type":"string"},{"type":"null"}]}}});
        assert_eq!(compatible_tool_schema(&ordinary), ordinary);
    }

    #[test]
    fn older_drafts_keep_their_existing_negation_without_using_conditionals() {
        let schema = json!({"$schema":"http://json-schema.org/draft-04/schema#","oneOf":[{"type":"string"},{"type":"integer"}],"not":{"enum":[0]}});
        let adapted = compatible_tool_schema(&schema);
        assert!(adapted.get("if").is_none());
        assert_eq!(adapted["not"]["not"]["not"], schema["not"]);
        assert_eq!(adapted["not"]["not"]["oneOf"], schema["oneOf"]);
    }

    #[test]
    fn references_preserve_nested_resources_and_literal_instance_data() {
        let schema = json!({"oneOf":[{"type":"object"}],
            "$defs":{"resource":{"$id":"nested.json","oneOf":[{"type":"string"}],
                "properties":{"value":{"$ref":"#/oneOf/0"}}}},
            "properties":{"local":{"$ref":"#/oneOf/0"}},
            "const":{"$ref":"#/oneOf/0"},
            "default":{"oneOf":[{"$ref":"#/oneOf/0"}]}});
        let adapted = compatible_tool_schema(&schema);
        assert_eq!(adapted["$defs"], schema["$defs"]);
        assert_eq!(adapted["const"], schema["const"]);
        assert_eq!(adapted["default"], schema["default"]);
        assert_eq!(adapted["properties"]["local"]["$ref"], "#/then/oneOf/0");
    }
}
