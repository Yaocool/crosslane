use super::*;
use reqwest::Method;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

// Deliberately fake credentials; no real provider or user configuration is used.
pub(super) const WRITE_TOKEN: &str = "fixture-admin-write-token";
pub(super) const READ_TOKEN: &str = "fixture-admin-read-token";

fn digest(token: &str) -> String {
    format!("{:x}", Sha256::digest(token.as_bytes()))
}

pub(super) struct AdminFixture {
    pub(super) daemon: Daemon,
    pub(super) port: u16,
    upstream: MockServer,
    client: reqwest::Client,
}

impl AdminFixture {
    pub(super) async fn start(copilot: bool) -> Self {
        let upstream = MockServer::start().await;
        let home = tempfile::tempdir().unwrap();
        let socket = home.path().join("admin.sock");
        let port = unused_tcp_port();
        let mut config = Config::default();
        config.admin.socket = socket.to_string_lossy().into_owned();
        config.status_http.enabled = true;
        config.status_http.port = port;
        config.admin.rest.enabled = true;
        config.admin.rest.token_sha256 = digest(WRITE_TOKEN);
        config.admin.rest.read_token_sha256 = Some(digest(READ_TOKEN));
        if copilot {
            let mut provider = ProviderConfig {
                id: "copilot-team".into(),
                kind: "copilot".into(),
                ..ProviderConfig::default()
            };
            provider
                .settings
                .insert("github_api_base".into(), json!(upstream.uri()));
            provider
                .settings
                .insert("oauth_base".into(), json!(upstream.uri()));
            provider
                .settings
                .insert("allow_insecure_http".into(), json!(true));
            config.provider = vec![ProviderConfig::default(), provider];
        }
        config.validate().unwrap();
        tokio::fs::write(
            home.path().join("config.toml"),
            toml::to_string(&config).unwrap(),
        )
        .await
        .unwrap();
        let child = Self::spawn(home.path(), &upstream.uri());
        wait_for_socket(&socket).await;
        wait_for_http(port).await;
        Self {
            daemon: Daemon {
                child,
                socket,
                home,
                api_key: None,
            },
            port,
            upstream,
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(20))
                .build()
                .unwrap(),
        }
    }

    fn spawn(home: &Path, upstream: &str) -> Child {
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(home.join("rest-test.log"))
            .unwrap();
        Command::new(env!("CARGO_BIN_EXE_crosslaned"))
            .env("CROSSLANE_HOME", home)
            .env("CROSSLANE_CODEWHISPERER_URL", upstream)
            .env("CROSSLANE_AMAZONQ_URL", upstream)
            .env("CROSSLANE_RUNTIME_URL", upstream)
            .env("CROSSLANE_MANAGEMENT_URL", upstream)
            .env("RUST_LOG", "info")
            .stdout(log)
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap()
    }

    pub(super) fn request(
        &self,
        method: Method,
        path: &str,
        token: Option<&str>,
    ) -> reqwest::RequestBuilder {
        let request = self.client.request(
            method,
            format!("http://127.0.0.1:{}/admin/v1{path}", self.port),
        );
        if let Some(token) = token {
            request.bearer_auth(token)
        } else {
            request
        }
    }

    async fn api(&self, method: Method, path: &str, body: Option<Value>, expected: u16) -> Value {
        let request = self.request(method, path, Some(WRITE_TOKEN));
        let response = if let Some(body) = body {
            request.json(&body)
        } else {
            request
        }
        .send()
        .await
        .unwrap();
        assert_eq!(response.headers()["cache-control"], "no-store");
        assert!(response.headers()["x-request-id"]
            .to_str()
            .unwrap()
            .starts_with("admin_"));
        let status = response.status().as_u16();
        let body: Value = response.json().await.unwrap();
        assert_eq!(status, expected, "{path}: {body}");
        if expected >= 400 {
            assert!(body["error"]["code"].is_string(), "{body}");
        }
        body
    }

    async fn edit_auth(&self, enabled: bool, token: &str) {
        let file = self.daemon.home().join("config.toml");
        let raw = tokio::fs::read_to_string(&file).await.unwrap();
        let mut config: Config = toml::from_str(&raw).unwrap();
        config.admin.rest.enabled = enabled;
        config.admin.rest.token_sha256 = digest(token);
        tokio::fs::write(file, toml::to_string(&config).unwrap())
            .await
            .unwrap();
        assert_eq!(
            expect_ok(self.daemon.call("config.reload", json!({})).await)["applied"],
            true
        );
    }

    pub(super) async fn restart(&mut self) {
        self.daemon.child.kill().await.unwrap();
        self.daemon.child.wait().await.unwrap();
        self.daemon.child = Self::spawn(self.daemon.home(), &self.upstream.uri());
        wait_for_socket(&self.daemon.socket).await;
        wait_for_http(self.port).await;
    }
}

#[tokio::test]
async fn admin_rest_authentication_is_independent_rotatable_and_audited_without_secrets() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let fixture = AdminFixture::start(false).await;
    for path in ["/accounts", "/services", "/openapi.json", "/unknown"] {
        for token in [None, Some("wrong-token")] {
            let response = fixture
                .request(Method::GET, path, token)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 401);
            assert!(response.headers().contains_key("www-authenticate"));
        }
    }
    assert_eq!(
        fixture
            .request(
                Method::GET,
                "/accounts?token=fixture-admin-write-token",
                None
            )
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    for path in [
        "/accounts",
        "/services",
        "/providers",
        "/providers/kiro",
        "/models",
        "/runtime/status",
        "/runtime/stats",
        "/runtime/tasks",
        "/api-keys",
        "/openapi.json",
    ] {
        let response = fixture
            .request(Method::GET, path, Some(READ_TOKEN))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "{path}");
    }
    for (method, path) in [
        (Method::POST, "/accounts"),
        (Method::PATCH, "/services/missing"),
        (Method::DELETE, "/accounts/kiro/missing"),
        (Method::POST, "/api-keys/missing/reset-usage"),
    ] {
        let response = fixture
            .request(method, path, Some(READ_TOKEN))
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 403);
    }
    fixture.api(Method::PUT, "/accounts", None, 405).await;
    fixture.api(Method::GET, "/unknown", None, 404).await;
    fixture
        .api(Method::GET, "/accounts?group_by=invalid", None, 400)
        .await;
    fixture
        .api(Method::GET, "/accounts?limit=1001", None, 400)
        .await;
    let malformed = fixture
        .request(Method::POST, "/accounts", Some(WRITE_TOKEN))
        .header("content-type", "application/json")
        .body("{\"token\":\"fixture-malformed-secret")
        .send()
        .await
        .unwrap();
    assert_eq!(malformed.status(), 400);
    assert!(!malformed
        .text()
        .await
        .unwrap()
        .contains("fixture-malformed-secret"));
    fixture.api(Method::POST, "/accounts", Some(json!({"provider":"kiro","auth":"api-key","email":"bad@example.com","token":"ksk_fixture_invalid_fields","unexpected":"fixture-private-field"})), 400).await;
    let large = fixture
        .request(Method::POST, "/accounts", Some(WRITE_TOKEN))
        .json(&json!({"token":"x".repeat(2*1024*1024)}))
        .send()
        .await
        .unwrap();
    assert_eq!(large.status(), 413);
    assert_eq!(
        large.json::<Value>().await.unwrap()["error"]["code"],
        "invalid_request"
    );

    let schema = fixture.api(Method::GET, "/openapi.json", None, 200).await;
    assert_eq!(schema["openapi"], "3.1.0");
    assert!(schema.get("data").is_none());
    assert_eq!(schema["security"][0], json!({"AdminBearer":[]}));
    assert_eq!(schema["paths"].as_object().unwrap().len(), 25);
    let removal = &schema["paths"]["/accounts/{provider}/{account}"]["delete"];
    assert!(removal["parameters"]
        .as_array()
        .unwrap()
        .iter()
        .any(|parameter| parameter["name"] == "force"
            && parameter["in"] == "query"
            && parameter["schema"]["default"] == false));

    fixture.edit_auth(true, "fixture-rotated-admin-token").await;
    assert_eq!(
        fixture
            .request(Method::GET, "/accounts", Some(WRITE_TOKEN))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        fixture
            .request(
                Method::GET,
                "/accounts",
                Some("fixture-rotated-admin-token")
            )
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        fixture
            .request(Method::GET, "/accounts", Some(READ_TOKEN))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    fixture
        .edit_auth(false, "fixture-rotated-admin-token")
        .await;
    assert_eq!(
        fixture
            .request(
                Method::GET,
                "/accounts",
                Some("fixture-rotated-admin-token")
            )
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    let health: Value = fixture
        .client
        .get(format!("http://127.0.0.1:{}/health", fixture.port))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    status_http::assert_health_contract(&health);

    let log_path = fixture.daemon.home().join("rest-test.log");
    let deadline = Instant::now() + Duration::from_secs(5);
    let logs = loop {
        let text = tokio::fs::read_to_string(&log_path).await.unwrap();
        if text.contains("admin.http.request") {
            break text;
        }
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let events: Vec<Value> = logs
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|event| event["fields"]["event"] == "admin.http.request")
        .collect();
    assert!(events
        .iter()
        .any(|event| event["fields"]["role"] == "read_only" && event["fields"]["status"] == 403));
    for event in events {
        assert!(event["fields"]["request_id"]
            .as_str()
            .unwrap()
            .starts_with("admin_"));
        assert!(event["fields"]["route"].is_string());
        assert!(event["fields"]["duration_ms"].is_u64());
    }
    for secret in [
        WRITE_TOKEN.to_owned(),
        READ_TOKEN.into(),
        digest(WRITE_TOKEN),
        "fixture-rotated-admin-token".into(),
        "fixture-malformed-secret".into(),
        "fixture-private-field".into(),
        "token=fixture".into(),
    ] {
        assert!(
            !logs.contains(&secret),
            "admin log leaked a fixture credential"
        );
    }
    assert!(fixture
        .upstream
        .received_requests()
        .await
        .unwrap()
        .is_empty());
    fixture.daemon.stop().await;
}

#[tokio::test]
async fn admin_rest_kiro_and_service_mutations_persist_and_respect_effective_bindings() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let mut fixture = AdminFixture::start(false).await;
    let first = fixture
        .api(
            Method::POST,
            "/accounts",
            Some(json!({
                "provider":"kiro","auth":"api-key","email":"FIRST@example.com",
                "token":"ksk_fixture_private_kiro_key","tags":[" beta ","alpha","alpha"]
            })),
            201,
        )
        .await;
    assert_eq!(first["data"]["email"], "first@example.com");
    assert_eq!(first["data"]["tags"], json!(["alpha", "beta"]));
    assert!(first["data"]["quota"]["remaining"].is_null());
    assert!(!first.to_string().contains("ksk_"));
    let first_id = first["data"]["id"].as_str().unwrap().to_owned();
    fixture.api(Method::POST, "/accounts", Some(json!({
        "provider":"kiro","auth":"api-key","email":"first@example.com","token":"ksk_fixture_duplicate"
    })), 409).await;
    let second = fixture.api(Method::POST, "/accounts", Some(json!({
        "provider":"kiro","auth":"oauth","email":"second@example.com","tag":"other",
        "credentials":{"auth_method":"idc","access_token":"fixture-private-access","refresh_token":"fixture-private-refresh",
            "client_id":"fixture-client","client_secret":"fixture-private-client-secret","region":"us-east-1","expires_at":4_000_000_000i64}
    })), 201).await;
    assert!(!second.to_string().contains("fixture-private"));
    let second_id = second["data"]["id"].as_str().unwrap().to_owned();
    let second_path = format!("/accounts/kiro/{second_id}");
    fixture
        .api(
            Method::POST,
            &format!("{second_path}/enabled"),
            Some(json!({"enabled":false})),
            200,
        )
        .await;
    assert_eq!(
        fixture.api(Method::GET, &second_path, None, 200).await["data"]["enabled"],
        false
    );
    fixture
        .api(
            Method::POST,
            &format!("{second_path}/enabled"),
            Some(json!({"enabled":true})),
            200,
        )
        .await;
    let third = fixture.api(Method::POST, "/accounts", Some(json!({
        "provider":"kiro","auth":"api-key","email":"third@example.com","token":"ksk_fixture_untagged"
    })), 201).await;
    let third_id = third["data"]["id"].as_str().unwrap().to_owned();
    fixture
        .api(
            Method::POST,
            &format!("/accounts/kiro/{first_id}/refresh"),
            None,
            502,
        )
        .await;
    Mock::given(method("GET")).and(path("/getUsageLimits"))
        .and(header("authorization", "Bearer ksk_fixture_private_kiro_key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"usageBreakdownList":[{
            "resourceType":"CREDIT","currentUsageWithPrecision":26.75,"usageLimitWithPrecision":100.0
        }]}))).mount(&fixture.upstream).await;
    let refreshed = fixture
        .api(
            Method::POST,
            &format!("/accounts/kiro/{first_id}/refresh"),
            None,
            200,
        )
        .await;
    assert_eq!(refreshed["data"]["quota"]["remaining"], 73.25);
    assert_eq!(
        refreshed["data"]["operation"],
        json!({"ok":true,"refreshed":false,"persisted":true,"usage_refreshed":true})
    );
    let listed = fixture
        .api(
            Method::GET,
            "/accounts?provider=kiro&group_by=tag&limit=2",
            None,
            200,
        )
        .await;
    assert_eq!(listed["data"]["total"], 3);
    assert_eq!(listed["data"]["has_more"], true);
    assert_eq!(
        fixture
            .api(Method::GET, "/accounts?untagged_only=true", None, 200)
            .await["data"]["total"],
        1
    );
    let port = unused_tcp_port();
    let created = fixture.api(Method::POST, "/services", Some(json!({
        "name":"rest-team","host":"127.0.0.1","port":port,"tags":["alpha"],"providers":["kiro"]
    })), 201).await;
    let id = created["data"]["service"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let key = created["data"]["api_key"]["key"]
        .as_str()
        .unwrap()
        .to_owned();
    let service_path = format!("/services/{id}");
    let service = fixture
        .api(Method::GET, "/services/rest-team?group_by=tag", None, 200)
        .await;
    assert_eq!(service["data"]["account_pool"]["total_accounts"], 1);
    assert_eq!(
        service["data"]["account_pool"]["groups"][0]["accounts"][0]["quota"]["remaining"],
        73.25
    );
    assert!(!service.to_string().contains(&key));
    assert_eq!(
        fixture
            .request(Method::GET, "/accounts", Some(&key))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        fixture
            .client
            .get(format!("http://127.0.0.1:{port}/admin/v1/accounts"))
            .bearer_auth(WRITE_TOKEN)
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    let keys = fixture
        .api(
            Method::GET,
            &format!("{service_path}/api-keys?show_secret=true"),
            None,
            200,
        )
        .await;
    assert!(!keys.to_string().contains(&key));
    assert!(!fixture
        .api(Method::GET, "/api-keys", None, 200)
        .await
        .to_string()
        .contains(&key));
    fixture
        .api(
            Method::DELETE,
            &format!("/accounts/kiro/{first_id}"),
            None,
            409,
        )
        .await;
    let bindings = fixture
        .api(
            Method::POST,
            &format!("{service_path}/accounts"),
            Some(json!({"accounts":[format!("kiro/{second_id}")]})),
            200,
        )
        .await;
    assert_eq!(bindings["data"]["total_accounts"], 2);
    let membership = fixture
        .api(Method::GET, &format!("{second_path}/services"), None, 200)
        .await;
    assert!(membership.to_string().contains(&id));
    fixture
        .api(
            Method::POST,
            &format!("/accounts/kiro/{first_id}/tags"),
            Some(json!({"remove":["alpha"]})),
            409,
        )
        .await;
    fixture
        .api(
            Method::PATCH,
            &service_path,
            Some(json!({"enabled":false})),
            200,
        )
        .await;
    assert!(tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .is_err());
    let temporarily_occupied = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .unwrap();
    fixture
        .api(
            Method::PATCH,
            &service_path,
            Some(json!({"enabled":true})),
            409,
        )
        .await;
    assert_eq!(
        fixture.api(Method::GET, &service_path, None, 200).await["data"]["service"]["enabled"],
        false
    );
    drop(temporarily_occupied);
    fixture
        .api(
            Method::PATCH,
            &service_path,
            Some(json!({"enabled":true})),
            200,
        )
        .await;
    wait_for_http(port).await;
    // A failed listen must roll back both the new service and its generated client key.
    let occupied = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let occupied_port = occupied.local_addr().unwrap().port();
    fixture
        .api(
            Method::POST,
            "/services",
            Some(json!({"name":"cannot-listen","host":"127.0.0.1","port":occupied_port})),
            409,
        )
        .await;
    assert_eq!(
        fixture.api(Method::GET, "/services", None, 200).await["data"]["services"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        fixture.api(Method::GET, "/api-keys", None, 200).await["data"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    fixture.restart().await;
    assert_eq!(
        fixture
            .api(Method::GET, &format!("{service_path}/accounts"), None, 200)
            .await["data"]["total_accounts"],
        2
    );
    assert_eq!(
        fixture
            .api(
                Method::GET,
                &format!("/accounts/kiro/{first_id}"),
                None,
                200
            )
            .await["data"]["tags"],
        json!(["alpha", "beta"])
    );
    let health: Value = fixture
        .client
        .get(format!("http://127.0.0.1:{}/health", fixture.port))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    status_http::assert_health_contract(&health);
    assert_eq!(health["total_accounts"], 1);
    let unbound = fixture
        .api(
            Method::DELETE,
            &format!("{service_path}/accounts"),
            Some(json!({"accounts":[format!("kiro/{first_id}")]})),
            200,
        )
        .await;
    assert_eq!(unbound["data"]["total_accounts"], 1);
    fixture
        .api(
            Method::POST,
            &format!("/accounts/kiro/{first_id}/tags"),
            Some(json!({"remove":["alpha"],"add":["archive"]})),
            200,
        )
        .await;
    fixture
        .api(
            Method::DELETE,
            &format!("/accounts/kiro/{first_id}"),
            None,
            200,
        )
        .await;
    fixture.api(Method::DELETE, &service_path, None, 200).await;
    fixture.api(Method::DELETE, &second_path, None, 200).await;
    fixture
        .api(
            Method::DELETE,
            &format!("/accounts/kiro/{third_id}"),
            None,
            200,
        )
        .await;
    fixture.restart().await;
    assert_eq!(
        fixture.api(Method::GET, "/accounts", None, 200).await["data"]["total"],
        0
    );
    assert_eq!(
        fixture.api(Method::GET, "/services", None, 200).await["data"]["services"],
        json!([])
    );
    assert_eq!(
        fixture.api(Method::GET, "/api-keys", None, 200).await["data"],
        json!([])
    );
    fixture.daemon.stop().await;
}

#[tokio::test]
async fn admin_rest_copilot_import_stores_initial_tags_and_reports_native_quota() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let mut fixture = AdminFixture::start(true).await;
    for (endpoint, body) in [
        ("/user", json!({"id":7,"login":"rest-copilot-fixture"})),
        (
            "/copilot_internal/v2/token",
            json!({"token":"fixture-copilot-api-secret","expires_at":4_000_000_000i64,"endpoints":{"api":fixture.upstream.uri()}}),
        ),
        (
            "/models",
            json!({"data":[{"id":"gpt-test","capabilities":{"supported_endpoints":["/chat/completions"]}}]}),
        ),
        (
            "/copilot_internal/user",
            json!({"copilot_plan":"pro","quota_snapshots":{"premium_interactions":{
            "entitlement":300,"remaining":279,"quota_remaining":279.875,"token_based_billing":true,"unlimited":false
        }},"unknown_private_field":"fixture-private-upstream"}),
        ),
    ] {
        Mock::given(method("GET"))
            .and(path(endpoint))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&fixture.upstream)
            .await;
    }
    fixture.api(Method::POST, "/accounts", Some(json!({"provider":"copilot-team","auth":"token","token":"fixture-github-private-token","tags":[" "]})), 400).await;
    assert!(fixture
        .upstream
        .received_requests()
        .await
        .unwrap()
        .is_empty());
    let account = fixture.api(Method::POST, "/accounts", Some(json!({
        "provider":"copilot-team","auth":"token","token":"fixture-github-private-token","label":"REST fixture","tags":["team-a","team-a"],"tag":"test"
    })), 201).await;
    assert_eq!(account["data"]["provider_id"], "copilot-team");
    assert_eq!(account["data"]["tags"], json!(["team-a", "test"]));
    let id = account["data"]["id"].as_str().unwrap().to_owned();
    let account_path = format!("/accounts/copilot-team/{id}");
    let account = fixture.api(Method::GET, &account_path, None, 200).await;
    assert_eq!(account["data"]["quota"]["remaining"], 279.875);
    assert_eq!(account["data"]["quota"]["unit"], "ai_credits");
    for secret in [
        "fixture-github-private-token",
        "fixture-copilot-api-secret",
        "fixture-private-upstream",
    ] {
        assert!(!account.to_string().contains(secret));
    }
    let created = fixture.api(Method::POST, "/services", Some(json!({
        "name":"copilot-rest","host":"127.0.0.1","port":unused_tcp_port(),"providers":["copilot-team"],"default_provider":"copilot-team","account_tag":"team-a"
    })), 201).await;
    let service_path = format!(
        "/services/{}",
        created["data"]["service"]["id"].as_str().unwrap()
    );
    let service = fixture.api(Method::GET, &service_path, None, 200).await;
    assert_eq!(service["data"]["account_pool"]["total_accounts"], 1);
    assert_eq!(
        service["data"]["account_pool"]["groups"][0]["provider_id"],
        "copilot-team"
    );
    assert_eq!(
        service["data"]["account_pool"]["groups"][0]["accounts"][0]["quota"]["remaining"],
        279.875
    );
    fixture.api(Method::DELETE, &account_path, None, 409).await;
    fixture.restart().await;
    assert_eq!(
        fixture.api(Method::GET, &account_path, None, 200).await["data"]["tags"],
        json!(["team-a", "test"])
    );
    let accounts_file = fixture
        .daemon
        .home()
        .join("providers/copilot-team/accounts.json");
    let backup = accounts_file.with_extension("backup");
    let config_file = fixture.daemon.home().join("config.toml");
    let original_config = tokio::fs::read(&config_file).await.unwrap();
    tokio::fs::rename(&accounts_file, &backup).await.unwrap();
    tokio::fs::create_dir(&accounts_file).await.unwrap();
    fixture
        .api(
            Method::DELETE,
            &format!("{account_path}?force=true"),
            None,
            500,
        )
        .await;
    tokio::fs::remove_dir(&accounts_file).await.unwrap();
    tokio::fs::rename(&backup, &accounts_file).await.unwrap();
    assert_eq!(
        tokio::fs::read(&config_file).await.unwrap(),
        original_config
    );
    let pool = fixture.api(Method::GET, &service_path, None, 200).await;
    assert_eq!(pool["data"]["account_pool"]["total_accounts"], 1);
    assert_eq!(
        pool["data"]["account_pool"]["excluded_account_ids"],
        json!([])
    );
    fixture
        .api(
            Method::DELETE,
            &format!("{account_path}?force=false"),
            None,
            409,
        )
        .await;
    let removed = fixture
        .api(
            Method::DELETE,
            &format!("{account_path}?force=true"),
            None,
            200,
        )
        .await;
    assert_eq!(
        removed["data"],
        json!({"removed":id,"provider_id":"copilot-team","reference":format!("copilot-team/{id}")})
    );
    let pool = fixture.api(Method::GET, &service_path, None, 200).await;
    assert_eq!(pool["data"]["account_pool"]["total_accounts"], 0);
    assert_eq!(
        pool["data"]["account_pool"]["excluded_account_ids"],
        json!([format!("copilot-team/{id}")])
    );
    assert_eq!(pool["data"]["service"]["running"], true);
    fixture.restart().await;
    let pool = fixture.api(Method::GET, &service_path, None, 200).await;
    assert_eq!(pool["data"]["account_pool"]["total_accounts"], 0);
    assert_eq!(
        fixture
            .api(Method::GET, "/accounts?provider=copilot-team", None, 200)
            .await["data"]["total"],
        0
    );
    fixture.daemon.stop().await;
}

#[tokio::test]
async fn admin_rest_force_remove_unbinds_all_kiro_services_and_persists_after_restart() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let mut fixture = AdminFixture::start(false).await;
    let mut ids = Vec::new();
    for name in ["remove", "keep"] {
        let account = fixture
            .api(
                Method::POST,
                "/accounts",
                Some(json!({
                    "provider":"kiro","auth":"api-key","email":format!("{name}@example.invalid"),
                    "token":format!("ksk_fixture_{name}"),"tags":["team-a"]
                })),
                201,
            )
            .await;
        ids.push(account["data"]["id"].as_str().unwrap().to_owned());
    }
    let account_path = format!("/accounts/kiro/{}", ids[0]);
    let aggregate_path = format!("/accounts/all/{}", ids[0]);
    fixture
        .api(
            Method::POST,
            &format!("{aggregate_path}/enabled"),
            Some(json!({"enabled":false})),
            400,
        )
        .await;
    fixture
        .api(
            Method::POST,
            &format!("{aggregate_path}/tags"),
            Some(json!({"add":["must-not-change"]})),
            400,
        )
        .await;
    fixture
        .api(
            Method::DELETE,
            &format!("{aggregate_path}?force=true"),
            None,
            400,
        )
        .await;
    let unchanged = fixture.api(Method::GET, &account_path, None, 200).await;
    assert_eq!(unchanged["data"]["enabled"], true);
    assert_eq!(unchanged["data"]["tags"], json!(["team-a"]));
    let mut services = Vec::new();
    for (name, tag, remaining) in [
        ("global", None, 1),
        ("tagged", Some("team-a"), 1),
        ("manual", Some("other"), 0),
    ] {
        let created = fixture
            .api(
                Method::POST,
                "/services",
                Some(json!({
                    "name":name,"host":"127.0.0.1","port":unused_tcp_port(),"account_tag":tag
                })),
                201,
            )
            .await;
        let path = format!(
            "/services/{}",
            created["data"]["service"]["id"].as_str().unwrap()
        );
        if name == "manual" {
            fixture
                .api(
                    Method::POST,
                    &format!("{path}/accounts"),
                    Some(json!({"accounts":[format!("kiro/{}", ids[0])]})),
                    200,
                )
                .await;
        }
        if name == "tagged" {
            fixture
                .api(Method::PATCH, &path, Some(json!({"enabled":false})), 200)
                .await;
        }
        services.push((name, path, remaining));
    }
    fixture.api(Method::DELETE, &account_path, None, 409).await;
    fixture
        .api(
            Method::DELETE,
            &format!("{account_path}?force=false"),
            None,
            409,
        )
        .await;
    fixture
        .api(
            Method::DELETE,
            &format!("{account_path}?force=invalid"),
            None,
            400,
        )
        .await;
    let denied = fixture
        .request(
            Method::DELETE,
            &format!("{account_path}?force=true"),
            Some(READ_TOKEN),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), 403);
    let removed = fixture
        .api(
            Method::DELETE,
            &format!("{account_path}?force=true"),
            None,
            200,
        )
        .await;
    assert_eq!(removed["data"]["removed"], ids[0]);
    for restart in [false, true] {
        if restart {
            fixture.restart().await;
        }
        fixture.api(Method::GET, &account_path, None, 404).await;
        let accounts = fixture
            .api(Method::GET, "/accounts?provider=kiro", None, 200)
            .await;
        assert_eq!(accounts["data"]["total"], 1);
        for (name, path, remaining) in &services {
            let service = fixture.api(Method::GET, path, None, 200).await;
            assert_eq!(
                service["data"]["account_pool"]["total_accounts"],
                *remaining
            );
            assert_eq!(service["data"]["service"]["running"], *name != "tagged");
            assert_eq!(
                service["data"]["account_pool"]["excluded_account_ids"],
                json!([ids[0]])
            );
            assert_eq!(
                service["data"]["account_pool"]["manual_account_ids"],
                json!([])
            );
        }
    }
    fixture.daemon.stop().await;
}

impl AdminFixture {
    async fn batch_report(&self, id: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let response = self
                .request(
                    Method::GET,
                    &format!("/account-batches/{id}"),
                    Some(READ_TOKEN),
                )
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 200);
            let report: Value = response.json().await.unwrap();
            if report["data"]["finished_at"].is_i64() {
                return report["data"].clone();
            }
            assert!(Instant::now() < deadline, "batch did not finish: {report}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

#[tokio::test]
async fn admin_rest_batches_validate_before_creating_and_preserve_partial_success_after_restart() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let mut fixture = AdminFixture::start(false).await;
    for input in [
        json!({"provider":"kiro","auth":"api-key","accounts":[{"email":"must-not-create@example.com","token":"ksk_fixture_batch_key"},{"email":"invalid","token":"ksk_fixture_invalid"}]}),
        json!({"provider":"kiro","auth":"sso","csv":"email,password\nrow@example.com,password,extra"}),
        json!({"provider":"kiro","auth":"api-key","accounts":[],"concurrency":9}),
        json!({"provider":"kiro","auth":"sso","accounts":[],"csv":"email,password\nrow@example.com,password"}),
    ] {
        fixture
            .api(Method::POST, "/account-batches", Some(input), 400)
            .await;
    }
    for (bytes, expected) in [
        (vec![0xff, 0xfe], 400),
        (vec![b'x'; 2 * 1024 * 1024 + 1], 413),
    ] {
        let response = fixture
            .request(
                Method::POST,
                "/account-batches?provider=kiro",
                Some(WRITE_TOKEN),
            )
            .header("content-type", "text/csv")
            .body(bytes)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
        assert_eq!(
            response.json::<Value>().await.unwrap()["error"]["code"],
            "invalid_request"
        );
    }
    assert_eq!(
        fixture.api(Method::GET, "/accounts", None, 200).await["data"]["total"],
        0
    );
    assert_eq!(
        fixture
            .api(Method::GET, "/account-batches", None, 200)
            .await["data"]["batches"],
        json!([])
    );
    assert!(fixture
        .upstream
        .received_requests()
        .await
        .unwrap()
        .is_empty());
    for path in ["/account-batches", "/account-batches/missing/cancel"] {
        assert_eq!(
            fixture
                .request(Method::POST, path, Some(READ_TOKEN))
                .json(&json!({}))
                .send()
                .await
                .unwrap()
                .status(),
            403
        );
    }
    let request = json!({"provider":"kiro","auth":"api-key","tags":["team-a"],"accounts":[
        {"email":"one@example.com","token":"ksk_fixture_first_batch","tag":"one"},
        {"email":"one@example.com","token":"ksk_fixture_duplicate_batch"},
        {"email":"two@example.com","token":"ksk_fixture_second_batch"}
    ]});
    let response = fixture
        .request(Method::POST, "/account-batches", Some(WRITE_TOKEN))
        .json(&request)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 202);
    let location = response.headers()["location"].to_str().unwrap().to_owned();
    let accepted: Value = response.json().await.unwrap();
    let id = accepted["data"]["id"].as_str().unwrap().to_owned();
    assert_eq!(location, format!("/admin/v1/account-batches/{id}"));
    let report = fixture.batch_report(&id).await;
    assert_eq!(report["status"], "completed_with_errors");
    assert_eq!(report["succeeded"], 2);
    assert_eq!(report["failed"], 1);
    assert_eq!(report["rows"][1]["error"]["code"], "conflict");
    assert!(report["rows"][2]["reference"]
        .as_str()
        .unwrap()
        .starts_with("kiro/acc_"));
    for secret in [
        "ksk_fixture_first_batch",
        "ksk_fixture_duplicate_batch",
        "ksk_fixture_second_batch",
    ] {
        assert!(!report.to_string().contains(secret));
    }
    let first_id = report["rows"][0]["account_id"].as_str().unwrap().to_owned();
    assert_eq!(
        fixture
            .api(
                Method::GET,
                &format!("/accounts/kiro/{first_id}"),
                None,
                200
            )
            .await["data"]["tags"],
        json!(["one", "team-a"])
    );
    assert!(fixture
        .api(Method::GET, "/account-batches", None, 200)
        .await["data"]["batches"][0]
        .get("rows")
        .is_none());
    fixture
        .api(
            Method::POST,
            &format!("/account-batches/{id}/cancel"),
            None,
            200,
        )
        .await;
    fixture.restart().await;
    assert_eq!(
        fixture.api(Method::GET, "/accounts", None, 200).await["data"]["total"],
        2
    );
    fixture
        .api(Method::GET, &format!("/account-batches/{id}"), None, 404)
        .await;
    fixture.daemon.stop().await;
}

#[cfg(feature = "sso")]
#[tokio::test]
async fn admin_rest_accepts_cli_sso_csv_and_uses_the_existing_sso_duplicate_guard() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let fixture = AdminFixture::start(false).await;
    for email in ["one@example.com", "two@example.com"] {
        fixture.api(Method::POST,"/accounts",Some(json!({"provider":"kiro","auth":"api-key","email":email,"token":"ksk_fixture_existing"})),201).await;
    }
    let csv="\u{feff}email,password\r\n\"ONE@example.com\",\" fixture,\"\"private-password \"\r\ntwo@example.com,second-private-password\r\n";
    let response=fixture.request(Method::POST,"/account-batches?provider=kiro&auth=sso&tag=team-a&concurrency=2&start_url=https%3A%2F%2Fexample.awsapps.com%2Fstart",Some(WRITE_TOKEN))
        .header("content-type","text/csv; charset=utf-8").body(csv).send().await.unwrap();
    assert_eq!(response.status(), 202);
    let accepted: Value = response.json().await.unwrap();
    let id = accepted["data"]["id"].as_str().unwrap();
    let report = fixture.batch_report(id).await;
    assert_eq!(report["failed"], 2);
    assert_eq!(report["rows"][0]["display_name"], "one@example.com");
    for row in report["rows"].as_array().unwrap() {
        assert_eq!(row["error"]["code"], "conflict");
        assert!(row["error"]["message"]
            .as_str()
            .unwrap()
            .contains("already exists"));
    }
    assert!(!report.to_string().contains("private-password"));
    // The common SSO handler rejects existing accounts before launching Chromium.
    assert!(fixture
        .upstream
        .received_requests()
        .await
        .unwrap()
        .is_empty());
    let log = tokio::fs::read_to_string(fixture.daemon.home().join("rest-test.log"))
        .await
        .unwrap();
    assert!(!log.contains("private-password"));
    fixture.daemon.stop().await;
}
