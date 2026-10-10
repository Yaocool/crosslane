use std::collections::HashMap;

use super::{array, invalid, json, string, BridgeError, Value};
use crate::{ResponsesToolName, ToolNameRegistry};

#[derive(Clone, Default)]
pub struct ToolNames {
    upstream: HashMap<String, String>,
    original: HashMap<String, ResponsesToolName>,
}

impl ToolNames {
    pub fn for_responses(body: &Value) -> Result<Self, BridgeError> {
        let mut names = HashMap::new();
        for tool in array(&body["tools"]) {
            if tool["type"] == "namespace" {
                let namespace = string(&tool["name"]);
                if namespace.is_empty() || !tool["tools"].is_array() {
                    return Err(invalid("namespace requires a name and a tools array"));
                }
                for child in array(&tool["tools"]) {
                    Self::add(&mut names, child, Some(namespace))?;
                }
            } else if tool["type"] == "function" {
                Self::add(&mut names, tool, None)?;
            }
        }
        let registry = ToolNameRegistry::new(names.keys().map(String::as_str));
        let mut result = Self::default();
        for (qualified, original) in names {
            let upstream = registry.kiro_name(&qualified);
            result.upstream.insert(qualified, upstream.clone());
            result.original.insert(upstream, original);
        }
        Ok(result)
    }

    fn add(
        names: &mut HashMap<String, ResponsesToolName>,
        tool: &Value,
        namespace: Option<&str>,
    ) -> Result<(), BridgeError> {
        if tool["type"] != "function" {
            return Err(invalid("namespace members must be function tools"));
        }
        let name = string(&tool["name"]);
        if name.is_empty() {
            return Err(invalid("function tool name is required"));
        }
        let qualified = namespace.map_or_else(|| name.to_owned(), |ns| format!("{ns}.{name}"));
        if names
            .insert(
                qualified,
                ResponsesToolName {
                    name: name.into(),
                    namespace: namespace.map(str::to_owned),
                },
            )
            .is_some()
        {
            return Err(invalid("duplicate or ambiguous tool name"));
        }
        Ok(())
    }

    pub(super) fn upstream_name(&self, item: &Value) -> String {
        let definition = item.get("function").unwrap_or(item);
        let name = string(&definition["name"]);
        let qualified = item["namespace"]
            .as_str()
            .map_or_else(|| name.to_owned(), |ns| format!("{ns}.{name}"));
        self.upstream.get(&qualified).cloned().unwrap_or(qualified)
    }

    pub fn restore(&self, value: &mut Value) {
        match value {
            Value::Object(object) => {
                if object
                    .get("type")
                    .is_some_and(|kind| kind == "function_call")
                {
                    if let Some(original) = object
                        .get("name")
                        .and_then(Value::as_str)
                        .and_then(|name| self.original.get(name))
                    {
                        object.insert("name".into(), json!(original.name));
                        if let Some(namespace) = &original.namespace {
                            object.insert("namespace".into(), json!(namespace));
                        }
                    }
                }
                for child in object.values_mut() {
                    self.restore(child);
                }
            }
            Value::Array(items) => {
                for item in items {
                    self.restore(item);
                }
            }
            _ => {}
        }
    }
}
