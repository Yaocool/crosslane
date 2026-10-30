use super::*;

#[tokio::test]
async fn failed_config_writes_preserve_token_and_service_generations_before_returning() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    for (operation, mode, outcome) in [
        ("rotate-token", 0o300, "restored"),
        ("create-service", 0o300, "restored"),
        ("rotate-token", 0o500, "unchanged"),
        ("create-service", 0o500, "unchanged"),
    ] {
        let (directory, state) = state_with(vec![]).await;
        let metadata = std::fs::metadata(directory.path()).unwrap();
        if metadata.uid() == 0 {
            // Root bypasses the directory-read permission used for this fault.
            state.shutdown.cancel();
            return;
        }
        let token = expect_ok(
            dispatch(
                &state,
                Request::new(
                    1,
                    method::ADMIN_TOKEN_CREATE,
                    serde_json::json!({"name":"existing-operator"}),
                ),
            )
            .await,
        );
        let before = tokio::fs::read(&state.paths.config_file).await.unwrap();
        let digest = state.config.current().admin.rest.tokens[0]
            .token_sha256
            .clone();
        // 0300 permits rename but rejects directory fsync; 0500 rejects temp-file
        // creation before rename. Exercise both sides of the commit boundary.
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(mode)).unwrap();
        let (name, params) = if operation == "rotate-token" {
            (
                method::ADMIN_TOKEN_ROTATE,
                serde_json::json!({"id":token["token"]["id"]}),
            )
        } else {
            (
                method::SERVICE_CREATE,
                serde_json::json!({"name":"unacknowledged-service","host":"127.0.0.1"}),
            )
        };
        let response = dispatch(&state, Request::new(2, name, params)).await;
        std::fs::set_permissions(directory.path(), metadata.permissions()).unwrap();
        assert!(
            matches!(response, Response::Err { error, .. }
            if error.message.contains(&format!("previous configuration {outcome}"))),
            "the filesystem fault must reject {operation} after verifying disk rollback"
        );
        let on_disk = tokio::fs::read(&state.paths.config_file).await.unwrap();
        assert!(
            on_disk == before,
            "{operation} returned an error but left its new configuration committed"
        );
        assert_eq!(state.config.current().admin.rest.tokens.len(), 1);
        assert!(state.config.current().admin.rest.tokens[0].token_sha256 == digest);
        assert!(state.runtime_config_snapshot().admin.rest.tokens[0].token_sha256 == digest);
        assert!(state.config.current().proxy_service.is_empty());
        assert!(state.runtime_config_snapshot().proxy_service.is_empty());
        let persisted: Config = toml::from_str(&String::from_utf8(on_disk).unwrap()).unwrap();
        assert!(persisted.admin.rest.tokens[0].token_sha256 == digest);
        state.shutdown.cancel();
    }
}
