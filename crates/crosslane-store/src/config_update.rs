//! Comment-preserving updates for the human-maintained TOML configuration.

use std::collections::BTreeSet;

use anyhow::{anyhow, Context, Result};
use toml_edit::{DocumentMut, Item, TableLike};

/// Applies the semantic difference between two TOML values to the original
/// document while retaining comments and formatting around unchanged fields.
pub fn render_update_preserving_comments(
    raw: &str,
    before: &toml::Value,
    after: &toml::Value,
) -> Result<String> {
    let before = before
        .as_table()
        .ok_or_else(|| anyhow!("config root must be a TOML table"))?;
    let after = after
        .as_table()
        .ok_or_else(|| anyhow!("config root must be a TOML table"))?;
    let mut document = raw
        .parse::<DocumentMut>()
        .context("parse editable configuration")?;
    apply_table_diff(document.as_table_mut(), before, after)?;
    Ok(document.to_string())
}

fn apply_table_diff(
    target: &mut dyn TableLike,
    before: &toml::map::Map<String, toml::Value>,
    after: &toml::map::Map<String, toml::Value>,
) -> Result<()> {
    let keys = before
        .keys()
        .chain(after.keys())
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    for key in keys {
        let previous = before.get(key);
        let next = after.get(key);
        if previous == next {
            continue;
        }
        match (previous, next) {
            (_, None) => {
                target.remove(key);
            }
            (Some(toml::Value::Table(previous)), Some(toml::Value::Table(next))) => {
                if let Some(table) = target.get_mut(key).and_then(Item::as_table_like_mut) {
                    apply_table_diff(table, previous, next)?;
                } else {
                    insert_or_replace(target, key, value_to_item(next.clone().into())?)?;
                }
            }
            (_, Some(next)) => {
                insert_or_replace(target, key, value_to_item(next.clone())?)?;
            }
        }
    }
    Ok(())
}

fn insert_or_replace(target: &mut dyn TableLike, key: &str, mut next: Item) -> Result<()> {
    if let Some(current) = target.get_mut(key) {
        if let (Some(current_value), Some(next_value)) = (current.as_value(), next.as_value_mut()) {
            *next_value.decor_mut() = current_value.decor().clone();
        }
        *current = next;
    } else {
        target.insert(key, next);
    }
    Ok(())
}

fn value_to_item(value: toml::Value) -> Result<Item> {
    const WRAPPER: &str = "__crosslane_config_value";
    let mut table = toml::map::Map::new();
    table.insert(WRAPPER.into(), value);
    let encoded = toml::to_string(&toml::Value::Table(table))
        .context("serialize updated configuration value")?;
    let mut document = encoded
        .parse::<DocumentMut>()
        .context("parse updated configuration value")?;
    document
        .as_table_mut()
        .remove(WRAPPER)
        .ok_or_else(|| anyhow!("serialized configuration value is missing"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn updates_only_changed_values_and_keeps_comments() {
        let raw = r#"# top-level explanation
[notify]
# threshold explanation
low_credit_threshold_percent = 10.0 # inline note
max_notifications = 5

# Example service remains commented.
# [[proxy_service]]
# name = "example"
"#;
        let before = raw.parse::<toml::Value>().expect("before");
        let mut after = before.clone();
        let root = after.as_table_mut().expect("root");
        root.get_mut("notify")
            .and_then(toml::Value::as_table_mut)
            .expect("notify")
            .insert(
                "low_credit_threshold_percent".into(),
                toml::Value::Float(15.0),
            );
        root.insert(
            "proxy_service".into(),
            toml::Value::Array(vec![toml::Value::Table(toml::Table::from_iter([
                ("id".into(), toml::Value::String("svc_main".into())),
                ("name".into(), toml::Value::String("main".into())),
            ]))]),
        );

        let output = render_update_preserving_comments(raw, &before, &after).expect("render");
        assert!(output.contains("# top-level explanation"));
        assert!(output.contains("# threshold explanation"));
        assert!(output.contains("15.0 # inline note"));
        assert!(output.contains("# Example service remains commented."));
        let parsed = output.parse::<toml::Value>().expect("updated TOML");
        assert_eq!(
            parsed["notify"]["low_credit_threshold_percent"].as_float(),
            Some(15.0)
        );
        assert_eq!(parsed["proxy_service"][0]["id"].as_str(), Some("svc_main"));
    }
}
