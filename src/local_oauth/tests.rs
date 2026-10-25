use super::*;
use axum::body::{Body as HttpBody, to_bytes};
use tower::ServiceExt;

const OWNER: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const RESOURCE: &str = "https://mcp.example.com/mcp";
const CALLBACK: &str = "https://chatgpt.com/connector_platform_oauth_redirect";
// RFC 7636 test vector, not a generated credential.
const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";

fn config() -> LocalOAuthConfig {
    LocalOAuthConfig {
        resource: RESOURCE.into(),
        client_id: "qmcp-chatgpt".into(),
        redirect_uri: CALLBACK.into(),
        owner_secret_file: PathBuf::new(),
    }
}
fn encoded(pairs: &[(&str, &str)]) -> String {
    let mut url = reqwest::Url::parse(RESOURCE).unwrap();
    url.query_pairs_mut().extend_pairs(pairs.iter().copied());
    url.query().unwrap().to_owned()
}
async fn json(response: Response) -> serde_json::Value {
    serde_json::from_slice(&to_bytes(response.into_body(), 65536).await.unwrap()).unwrap()
}
struct App {
    auth: LocalOAuth,
    router: Router,
    client_id: String,
}
impl App {
    fn new() -> Self {
        let auth = LocalOAuth::new(config(), OWNER).unwrap();
        let router = auth.router(Router::new().route(
            "/mcp",
            get(|headers: HeaderMap| async move {
                assert!(!headers.contains_key(header::AUTHORIZATION));
                "protected"
            }),
        ));
        Self {
            auth,
            router,
            client_id: "qmcp-chatgpt".into(),
        }
    }
    async fn call(
        &self,
        method: &str,
        path: &str,
        form: &[(&str, &str)],
        headers: &[(&str, &str)],
    ) -> Response {
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header("host", self.auth.authority())
            .header("content-type", "application/x-www-form-urlencoded")
            .body(HttpBody::from(encoded(form)))
            .unwrap();
        for (key, value) in headers {
            request.headers_mut().insert(
                key.parse::<header::HeaderName>().unwrap(),
                value.parse().unwrap(),
            );
        }
        self.router.clone().oneshot(request).await.unwrap()
    }
    async fn register(&self, metadata: serde_json::Value) -> Response {
        let request = Request::builder()
            .method("POST")
            .uri("/oauth/register")
            .header("host", self.auth.authority())
            .header("content-type", "application/json")
            .body(HttpBody::from(metadata.to_string()))
            .unwrap();
        self.router.clone().oneshot(request).await.unwrap()
    }
    async fn dynamic(&self) -> Self {
        let response = self
            .register(
                json!({"redirect_uris": [CALLBACK], "token_endpoint_auth_method": "none",
            "grant_types": ["authorization_code", "refresh_token"]}),
            )
            .await;
        assert_eq!(response.status(), StatusCode::CREATED);
        let registered = json(response).await;
        assert_eq!(registered["token_endpoint_auth_method"], "none");
        assert!(registered.get("client_secret").is_none());
        Self {
            auth: self.auth.clone(),
            router: self.router.clone(),
            client_id: registered["client_id"].as_str().unwrap().into(),
        }
    }
    async fn begin(&self) -> (String, String) {
        let params = [
            ("client_id", self.client_id.as_str()),
            ("redirect_uri", CALLBACK),
            ("response_type", "code"),
            ("scope", SCOPE),
            ("resource", RESOURCE),
            ("state", "original-state"),
            ("code_challenge_method", "S256"),
            ("code_challenge", CHALLENGE),
        ];
        let response = self
            .call(
                "GET",
                &format!("/oauth/authorize?{}", encoded(&params)),
                &[],
                &[],
            )
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        let cookie = response.headers()[header::SET_COOKIE].to_str().unwrap();
        assert!(cookie.contains("Secure; HttpOnly; SameSite=Lax"));
        let cookie = cookie.split(';').next().unwrap().to_owned();
        let nonce = cookie.split_once('=').unwrap().1.to_owned();
        let html = String::from_utf8(to_bytes(response.into_body(), 8192).await.unwrap().to_vec())
            .unwrap();
        assert!(
            html.contains(&nonce) && html.contains("Log in and approve") && html.contains(CALLBACK)
        );
        assert!(html.contains(&self.client_id));
        assert!(!html.contains(OWNER));
        (cookie, nonce)
    }
    async fn consent(&self, cookie: &str, nonce: &str, owner: &str, decision: &str) -> Response {
        self.call(
            "POST",
            "/oauth/consent",
            &[
                ("transaction", nonce),
                ("owner", owner),
                ("decision", decision),
            ],
            &[("cookie", cookie), ("origin", &self.auth.0.issuer)],
        )
        .await
    }
    async fn callback(response: Response) -> HashMap<String, String> {
        assert_eq!(response.status(), StatusCode::OK);
        assert!(!response.headers().contains_key(header::LOCATION));
        let html = String::from_utf8(to_bytes(response.into_body(), 8192).await.unwrap().to_vec())
            .unwrap();
        assert!(html.contains("rel=\"noreferrer noopener\""));
        let target = html
            .split("href=\"")
            .nth(1)
            .unwrap()
            .split('"')
            .next()
            .unwrap()
            .replace("&amp;", "&");
        let url = reqwest::Url::parse(&target).unwrap();
        assert!(url.as_str().starts_with(&format!("{CALLBACK}?")));
        let fields: HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(fields["iss"], "https://mcp.example.com");
        assert_eq!(fields["state"], "original-state");
        fields
    }
    async fn code(&self) -> String {
        let (cookie, nonce) = self.begin().await;
        Self::callback(self.consent(&cookie, &nonce, OWNER, "approve").await).await["code"].clone()
    }
    async fn exchange(&self, code: &str, verifier: &str, resource: &str) -> Response {
        self.call(
            "POST",
            "/oauth/token",
            &[
                ("grant_type", "authorization_code"),
                ("client_id", self.client_id.as_str()),
                ("redirect_uri", CALLBACK),
                ("resource", resource),
                ("code", code),
                ("code_verifier", verifier),
            ],
            &[],
        )
        .await
    }
    async fn tokens(&self) -> serde_json::Value {
        let response = self.exchange(&self.code().await, VERIFIER, RESOURCE).await;
        assert_eq!(response.status(), StatusCode::OK);
        json(response).await
    }
    async fn refresh(&self, token: &str, resource: Option<&str>) -> Response {
        let mut fields = vec![
            ("grant_type", "refresh_token"),
            ("client_id", self.client_id.as_str()),
            ("refresh_token", token),
        ];
        if let Some(resource) = resource {
            fields.push(("resource", resource));
        }
        self.call("POST", "/oauth/token", &fields, &[]).await
    }
    async fn access(&self, token: &str) -> StatusCode {
        self.call(
            "GET",
            "/mcp",
            &[],
            &[("authorization", &format!("Bearer {token}"))],
        )
        .await
        .status()
    }
}

#[tokio::test]
async fn discovery_consent_pkce_and_protected_access_work() {
    let mut app = App::new();
    let server = crate::QmcpServer::new(
        crate::BridgeConfig::new(
            "ws://127.0.0.1:9/acp/ws",
            false,
            crate::policy::PermissionPolicy::Cancel,
        )
        .unwrap(),
    )
    .unwrap();
    let actual = crate::transport::http_router_with_local_oauth(
        server,
        "0.0.0.0:3031".parse().unwrap(),
        app.auth.clone(),
        &[],
    )
    .unwrap();
    let fake = std::mem::replace(&mut app.router, actual);
    let challenge = app.call("GET", "/mcp", &[], &[]).await;
    assert_eq!(challenge.status(), StatusCode::UNAUTHORIZED);
    assert!(
        challenge.headers()[header::WWW_AUTHENTICATE]
            .to_str()
            .unwrap()
            .contains("oauth-protected-resource/mcp")
    );
    for path in [
        "/.well-known/oauth-protected-resource",
        "/.well-known/oauth-protected-resource/mcp",
    ] {
        let response = app.call("GET", path, &[], &[]).await;
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        assert_eq!(json(response).await["resource"], RESOURCE);
    }
    let discovery = json(
        app.call("GET", "/.well-known/oauth-authorization-server", &[], &[])
            .await,
    )
    .await;
    assert_eq!(
        discovery["code_challenge_methods_supported"],
        json!(["S256"])
    );
    assert_eq!(
        discovery["registration_endpoint"],
        "https://mcp.example.com/oauth/register"
    );
    let actual = std::mem::replace(&mut app.router, fake);
    let tokens = app.dynamic().await.tokens().await;
    assert_eq!(tokens["token_type"], "bearer");
    assert_eq!(tokens["scope"], SCOPE);
    assert!(tokens["expires_in"].as_i64().unwrap() <= 1800);
    let request = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("host", app.auth.authority())
        .header(
            "authorization",
            format!("Bearer {}", tokens["access_token"].as_str().unwrap()),
        )
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .body(HttpBody::from(
            json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list",
            "params": {"_meta": {"padding": "x".repeat(9000)}}})
            .to_string(),
        ))
        .unwrap();
    let response = actual.oneshot(request).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "OAuth's form limit must not restrict MCP requests"
    );
    assert!(
        json(response).await["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == "new")
    );
    assert_eq!(
        app.access(tokens["access_token"].as_str().unwrap()).await,
        StatusCode::OK
    );
    assert_eq!(
        App::new()
            .access(tokens["access_token"].as_str().unwrap())
            .await,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn dcr_clients_complete_pkce_and_cannot_use_each_others_grants() {
    let mut app = App::new();
    app.auth = LocalOAuth::new(
        serde_json::from_value(json!({
            "resource": RESOURCE, "owner_secret_file": "unused",
        }))
        .unwrap(),
        OWNER,
    )
    .unwrap();
    app.router = app
        .auth
        .router(Router::new().route("/mcp", get(|| async { "protected" })));
    let a = app.dynamic().await;
    let b = app.dynamic().await;
    assert_ne!(a.client_id, b.client_id);
    let wrong_callback = a
        .call(
            "GET",
            &format!(
                "/oauth/authorize?{}",
                encoded(&[
                    ("client_id", &a.client_id),
                    ("redirect_uri", "https://other.example/callback"),
                    ("response_type", "code"),
                    ("resource", RESOURCE),
                    ("code_challenge_method", "S256"),
                    ("code_challenge", CHALLENGE),
                ])
            ),
            &[],
            &[],
        )
        .await;
    assert!(!wrong_callback.headers().contains_key(header::SET_COOKIE));
    let code = a.code().await;
    assert_eq!(
        b.exchange(&code, VERIFIER, RESOURCE).await.status(),
        StatusCode::BAD_REQUEST
    );
    let response = a.exchange(&code, VERIFIER, RESOURCE).await;
    assert_eq!(response.status(), StatusCode::OK);
    let first = json(response).await;
    let old = first["refresh_token"].as_str().unwrap();
    assert_eq!(b.refresh(old, None).await.status(), StatusCode::BAD_REQUEST);
    let rotated = json(a.refresh(old, None).await).await;
    assert_eq!(b.refresh(old, None).await.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        a.access(rotated["access_token"].as_str().unwrap()).await,
        StatusCode::OK
    );
    assert_eq!(a.refresh(old, None).await.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        a.access(rotated["access_token"].as_str().unwrap()).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        b.access(b.tokens().await["access_token"].as_str().unwrap())
            .await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn dcr_rejects_unsafe_or_unsupported_metadata() {
    let app = App::new();
    for change in [
        json!({"redirect_uris": []}),
        json!({"redirect_uris": ["https://evil.example/cb#fragment"]}),
        json!({"redirect_uris": ["http://public.example/cb"]}),
        json!({"redirect_uris": ["https://user:pass@example.com/cb"]}),
        json!({"token_endpoint_auth_method": "client_secret_basic"}),
        json!({"grant_types": ["client_credentials"]}),
        json!({"response_types": ["token"]}),
        json!({"scope": "admin"}),
    ] {
        let mut metadata =
            json!({"redirect_uris": [CALLBACK], "token_endpoint_auth_method": "none"});
        metadata
            .as_object_mut()
            .unwrap()
            .extend(change.as_object().unwrap().clone());
        let response = app.register(metadata).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(
            json(response).await["error"]
                .as_str()
                .unwrap()
                .starts_with("invalid_")
        );
    }
    let request = Request::builder()
        .method("POST")
        .uri("/oauth/register")
        .header("host", app.auth.authority())
        .header("content-type", "application/json")
        .body(HttpBody::from(
            "{\"redirect_uris\":[],\"redirect_uris\":[]}",
        ))
        .unwrap();
    let response = app.router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(json(response).await["error"], "invalid_client_metadata");
    let response = app
        .register(json!({"redirect_uris": [CALLBACK], "client_name": "x".repeat(9000)}))
        .await;
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn dcr_limits_recover_without_evicting_active_clients() {
    let app = App::new();
    let active = app.dynamic().await;
    let tokens = active.tokens().await;
    let pending = app.dynamic().await;
    let (cookie, nonce) = pending.begin().await;
    let coded = app.dynamic().await;
    let code = coded.code().await;
    let abandoned = app.dynamic().await;
    let metadata = json!({"redirect_uris": [CALLBACK]});
    let mut reached_rate = false;
    for _ in 0..20 {
        if app.register(metadata.clone()).await.status() == StatusCode::TOO_MANY_REQUESTS {
            reached_rate = true;
            break;
        }
    }
    assert!(reached_rate);
    let mut reached_capacity = false;
    for _ in 0..200 {
        app.auth.0.store.lock().unwrap().registration_rate.0 =
            Instant::now() - Duration::from_secs(61);
        if app.register(metadata.clone()).await.status() == StatusCode::SERVICE_UNAVAILABLE {
            reached_capacity = true;
            break;
        }
    }
    assert!(reached_capacity);
    {
        let mut store = app.auth.0.store.lock().unwrap();
        for (_, expiry) in store.registrations.values_mut() {
            if let Some(expiry) = expiry {
                *expiry = Instant::now() - Duration::from_secs(1);
            }
        }
    }
    assert_eq!(app.register(metadata).await.status(), StatusCode::CREATED);
    assert_eq!(
        active
            .refresh(tokens["refresh_token"].as_str().unwrap(), None)
            .await
            .status(),
        StatusCode::OK
    );
    assert!(
        App::callback(pending.consent(&cookie, &nonce, OWNER, "approve").await)
            .await
            .contains_key("code")
    );
    assert_eq!(
        coded.exchange(&code, VERIFIER, RESOURCE).await.status(),
        StatusCode::OK
    );
    assert!(
        app.auth
            .0
            .store
            .lock()
            .unwrap()
            .clients
            .check(&abandoned.client_id, None)
            .is_err()
    );
    assert_eq!(
        app.exchange(&app.code().await, VERIFIER, RESOURCE)
            .await
            .status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn consent_requires_matching_cookie_origin_owner_and_explicit_approval() {
    let app = App::new();
    let (cookie, nonce) = app.begin().await;
    let form = [
        ("transaction", nonce.as_str()),
        ("owner", OWNER),
        ("decision", "approve"),
    ];
    for headers in [
        vec![("origin", "https://evil.example")],
        vec![("cookie", cookie.as_str())],
        vec![
            ("origin", app.auth.0.issuer.as_str()),
            ("cookie", "__Host-qmcp-consent=wrong"),
        ],
    ] {
        assert_eq!(
            app.call("POST", "/oauth/consent", &form, &headers)
                .await
                .status(),
            StatusCode::FORBIDDEN
        );
    }
    assert!(
        App::callback(app.consent(&cookie, &nonce, OWNER, "approve").await)
            .await
            .contains_key("code")
    );
    assert_eq!(
        app.consent(&cookie, &nonce, OWNER, "approve")
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    for (owner, decision) in [("wrong", "approve"), (OWNER, "deny")] {
        let (cookie, nonce) = app.begin().await;
        let denied = app.consent(&cookie, &nonce, owner, decision).await;
        assert_eq!(App::callback(denied).await["error"], "access_denied");
    }
}

#[tokio::test]
async fn invalid_authorization_cannot_create_a_browser_transaction() {
    let app = App::new();
    let base = [
        ("client_id", "qmcp-chatgpt"),
        ("redirect_uri", CALLBACK),
        ("response_type", "code"),
        ("resource", RESOURCE),
        ("code_challenge_method", "S256"),
        ("code_challenge", CHALLENGE),
    ];
    for (key, value) in [
        ("redirect_uri", "https://evil.example/callback"),
        ("client_id", "unknown"),
        ("resource", "https://other.example/mcp"),
        ("scope", "admin"),
        ("code_challenge_method", "plain"),
    ] {
        let mut params = base.to_vec();
        params.retain(|(k, _)| *k != key);
        params.push((key, value));
        let response = app
            .call(
                "GET",
                &format!("/oauth/authorize?{}", encoded(&params)),
                &[],
                &[],
            )
            .await;
        assert!(!response.headers().contains_key(header::SET_COOKIE));
    }
    let mut params = base.to_vec();
    params.push(("client_id", "duplicate"));
    assert_eq!(
        app.call(
            "GET",
            &format!("/oauth/authorize?{}", encoded(&params)),
            &[],
            &[]
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    params = base
        .into_iter()
        .filter(|(key, _)| *key != "code_challenge")
        .collect();
    let response = app
        .call(
            "GET",
            &format!("/oauth/authorize?{}", encoded(&params)),
            &[],
            &[],
        )
        .await;
    assert!(!response.headers().contains_key(header::SET_COOKIE));
    assert!(app.auth.0.store.lock().unwrap().pending.is_empty());
}

#[tokio::test]
async fn codes_bind_resource_and_pkce_are_single_use_and_expire() {
    let app = App::new();
    let code = app.code().await;
    assert_eq!(
        app.exchange(&code, VERIFIER, "https://other.example/mcp")
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        app.exchange(&code, VERIFIER, RESOURCE).await.status(),
        StatusCode::OK
    );
    assert_eq!(
        app.exchange(&code, VERIFIER, RESOURCE).await.status(),
        StatusCode::BAD_REQUEST
    );
    let code = app.code().await;
    assert_eq!(
        app.exchange(&code, "wrong-verifier", RESOURCE)
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        app.exchange(&code, VERIFIER, RESOURCE).await.status(),
        StatusCode::BAD_REQUEST
    );
    let code = app.code().await;
    app.auth
        .0
        .store
        .lock()
        .unwrap()
        .codes
        .0
        .get_mut(&digest(&code))
        .unwrap()
        .until = Utc::now() - chrono::Duration::seconds(1);
    assert_eq!(
        app.exchange(&code, VERIFIER, RESOURCE).await.status(),
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn refresh_rotates_and_replay_revokes_only_its_family() {
    let app = App::new();
    let first = app.tokens().await;
    let other = app.tokens().await;
    let old = first["refresh_token"].as_str().unwrap();
    assert_eq!(
        app.refresh(old, Some("https://other.example/mcp"))
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    let response = app.refresh(old, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    let rotated = json(response).await;
    assert_ne!(rotated["refresh_token"], first["refresh_token"]);
    assert_eq!(
        app.access(first["access_token"].as_str().unwrap()).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        app.access(rotated["access_token"].as_str().unwrap()).await,
        StatusCode::OK
    );
    assert_eq!(
        app.refresh(old, None).await.status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        app.access(rotated["access_token"].as_str().unwrap()).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        app.refresh(rotated["refresh_token"].as_str().unwrap(), None)
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        app.access(other["access_token"].as_str().unwrap()).await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn refresh_lifetime_is_absolute_and_access_is_capped_at_family_expiry() {
    let app = App::new();
    let token = app.tokens().await;
    let expiry = Utc::now() + chrono::Duration::seconds(60);
    app.auth.0.store.lock().unwrap().tokens.families[0].expires = expiry;
    let rotated = json(
        app.refresh(token["refresh_token"].as_str().unwrap(), None)
            .await,
    )
    .await;
    assert!(rotated["expires_in"].as_i64().unwrap() <= 60);
    assert_eq!(
        app.auth.0.store.lock().unwrap().tokens.families[0].expires,
        expiry
    );
    app.auth.0.store.lock().unwrap().tokens.families[0].expires =
        Utc::now() - chrono::Duration::seconds(1);
    assert_eq!(
        app.refresh(rotated["refresh_token"].as_str().unwrap(), None)
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        app.access(rotated["access_token"].as_str().unwrap()).await,
        StatusCode::UNAUTHORIZED
    );
}

#[test]
fn startup_requires_a_private_regular_secret_file_and_valid_configuration() {
    use std::{
        io::Write,
        os::unix::fs::{OpenOptionsExt, PermissionsExt, symlink},
    };
    struct TempFile(PathBuf);
    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let secret =
        TempFile(std::env::temp_dir().join(format!("qmcp-owner-{}", uuid::Uuid::new_v4())));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&secret.0)
        .unwrap();
    writeln!(file, "{OWNER}").unwrap();
    let make_config = || {
        let mut cfg = config();
        cfg.owner_secret_file = secret.0.clone();
        cfg
    };
    assert!(LocalOAuth::from_config(make_config()).is_ok());
    std::fs::set_permissions(&secret.0, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(LocalOAuth::from_config(make_config()).is_err());
    std::fs::set_permissions(&secret.0, std::fs::Permissions::from_mode(0o600)).unwrap();
    let link = TempFile(secret.0.with_extension("link"));
    symlink(&secret.0, &link.0).unwrap();
    let mut cfg = make_config();
    cfg.owner_secret_file = link.0.clone();
    assert!(LocalOAuth::from_config(cfg).is_err());
    std::fs::write(&secret.0, "a human password").unwrap();
    assert!(LocalOAuth::from_config(make_config()).is_err());
    let mut cfg = config();
    cfg.resource = "http://mcp.example.com/mcp".into();
    assert!(LocalOAuth::new(cfg, OWNER).is_err());
}

#[tokio::test]
async fn http_guards_reject_host_origin_duplicates_and_oversized_bodies() {
    let app = App::new();
    for headers in [
        vec![("host", "evil.example")],
        vec![("origin", "https://evil.example")],
    ] {
        assert_eq!(
            app.call(
                "GET",
                "/.well-known/oauth-authorization-server",
                &[],
                &headers
            )
            .await
            .status(),
            StatusCode::FORBIDDEN
        );
    }
    let response = app
        .call(
            "POST",
            "/oauth/token",
            &[("client_id", "qmcp-chatgpt"), ("client_id", "again")],
            &[],
        )
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        app.call("POST", "/oauth/token", &[("huge", &"x".repeat(9000))], &[])
            .await
            .status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
    let token = app.tokens().await;
    assert_eq!(
        app.call(
            "GET",
            "/mcp",
            &[],
            &[
                (
                    "authorization",
                    &format!("Bearer {}", token["access_token"].as_str().unwrap())
                ),
                ("origin", "https://mcp.example.com")
            ]
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
}
