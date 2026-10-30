use super::*;

pub(super) async fn schema() -> Json<Value> {
    let mut spec: Value = serde_json::from_str(include_str!("openapi.json"))
        .expect("embedded OpenAPI document is valid");
    spec["info"]["version"] = json!(env!("CARGO_PKG_VERSION"));
    Json(spec)
}
