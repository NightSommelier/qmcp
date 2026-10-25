use super::*;
use crate::{BridgeConfig, QmcpServer, policy::PermissionPolicy, transport};
use axum::body::Body;
use serde_json::Value;
use tower::ServiceExt;

const RESOURCE: &str = "https://mcp.example.com/mcp";
const ISSUER: &str = "https://mcp.example.com";
const CALLBACK: &str = "https://client.example/callback?existing=1";
// Public deterministic test fixture, not a runtime credential.
const SECRET: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
const VERIFIER: &str = "abcdefghijklmnopqrstuvwxyz0123456789ABCDEFG._~-";
const STATE: &str = "a&b=+ /?#%\"<>\0";

struct Harness {
    auth: LocalOAuth,
    app: Router,
}
struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    text: String,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_str(&self.text).unwrap()
    }
    fn location(&self) -> Url {
        assert_eq!(self.status, StatusCode::OK);
        assert!(!self.headers.contains_key(header::LOCATION));
        let link = self
            .text
            .split("<a href=\"")
            .nth(1)
            .unwrap()
            .split('"')
            .next()
            .unwrap();
        let link = link
            .replace("&quot;", "\"")
            .replace("&#39;", "'")
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&amp;", "&");
        Url::parse(&link).unwrap()
    }
    fn parameter(&self, key: &str) -> String {
        self.location()
            .query_pairs()
            .find(|(k, _)| k == key)
            .unwrap()
            .1
            .into_owned()
    }
}

fn encoded(pairs: &[(&str, &str)]) -> String {
    form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs.iter().copied())
        .finish()
}

impl Harness {
    fn new() -> Self {
        let auth = LocalOAuth::new(RESOURCE, SECRET).unwrap();
        let server = QmcpServer::new(
            BridgeConfig::new("127.0.0.1:1", false, PermissionPolicy::Cancel).unwrap(),
        )
        .unwrap();
        let app = transport::http_router_with_local_oauth(
            server,
            "127.0.0.1:3031".parse().unwrap(),
            auth.clone(),
            &[],
        )
        .unwrap();
        Self { auth, app }
    }

    async fn request(
        &self,
        method: &str,
        uri: &str,
        body: String,
        headers: &[(&str, &str)],
    ) -> Reply {
        let mut request = Request::builder()
            .method(method)
            .uri(uri)
            .header("host", "mcp.example.com");
        for (key, value) in headers {
            request = request.header(*key, *value);
        }
        let response = self
            .app
            .clone()
            .oneshot(request.body(Body::from(body)).unwrap())
            .await
            .unwrap();
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        assert_eq!(response.headers()["x-frame-options"], "DENY");
        let status = response.status();
        let headers = response.headers().clone();
        let text = String::from_utf8(
            to_bytes(response.into_body(), 65536)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        Reply {
            status,
            headers,
            text,
        }
    }

    async fn register(&self, value: Value) -> Reply {
        self.request(
            "POST",
            "/oauth/register",
            value.to_string(),
            &[("content-type", "application/json")],
        )
        .await
    }

    async fn client(&self) -> String {
        let response = self.register(json!({ "redirect_uris": [CALLBACK], "client_name": "<script>untrusted & client</script>" })).await;
        assert_eq!(response.status, StatusCode::CREATED, "{}", response.text);
        response.json()["client_id"].as_str().unwrap().into()
    }

    async fn authorize(&self, client: &str, changes: &[(&str, &str)]) -> Reply {
        let challenge = URL_SAFE_NO_PAD.encode(digest(VERIFIER));
        let mut pairs = vec![
            ("client_id", client),
            ("redirect_uri", CALLBACK),
            ("resource", RESOURCE),
            ("scope", SCOPE),
            ("response_type", "code"),
            ("code_challenge_method", "S256"),
            ("code_challenge", challenge.as_str()),
            ("state", STATE),
        ];
        for (key, value) in changes {
            pairs.retain(|(k, _)| k != key);
            pairs.push((key, value));
        }
        self.request(
            "GET",
            &format!("/oauth/authorize?{}", encoded(&pairs)),
            String::new(),
            &[],
        )
        .await
    }

    fn browser(reply: &Reply) -> (String, String) {
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
        let cookie = reply.headers[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        let csrf = reply
            .text
            .split("name=\"csrf\" value=\"")
            .nth(1)
            .unwrap()
            .split('"')
            .next()
            .unwrap()
            .to_owned();
        (cookie, csrf)
    }

    async fn consent(
        &self,
        cookie: &str,
        csrf: &str,
        secret: &str,
        decision: &str,
        origin: Option<&str>,
    ) -> Reply {
        let mut headers = vec![
            ("content-type", "application/x-www-form-urlencoded"),
            ("cookie", cookie),
        ];
        if let Some(origin) = origin {
            headers.push(("origin", origin));
        }
        self.request(
            "POST",
            "/oauth/authorize",
            encoded(&[
                ("csrf", csrf),
                ("owner_secret", secret),
                ("decision", decision),
            ]),
            &headers,
        )
        .await
    }

    async fn code(&self, client: &str) -> String {
        let page = self.authorize(client, &[]).await;
        let (cookie, csrf) = Self::browser(&page);
        let response = self
            .consent(&cookie, &csrf, SECRET, "approve", Some(ISSUER))
            .await;
        assert_eq!(response.status, StatusCode::OK);
        assert_eq!(response.parameter("iss"), ISSUER);
        assert_eq!(response.parameter("state"), STATE);
        assert_eq!(response.parameter("existing"), "1");
        response.parameter("code")
    }

    async fn refresh_client(&self) -> String {
        let reply = self
            .register(json!({
                "redirect_uris": [CALLBACK], "client_name": "ChatGPT", "scope": SCOPE,
                "token_endpoint_auth_method": "none",
                "grant_types": ["authorization_code", "refresh_token"], "response_types": ["code"],
                "client_uri": "https://client.example/", "logo_uri": "https://client.example/icon",
                "application_type": "web", "unknown_metadata": {"safe": [1, 2]}
            }))
            .await;
        assert_eq!(reply.status, StatusCode::CREATED);
        assert_eq!(
            reply.json()["grant_types"],
            json!(["authorization_code", "refresh_token"])
        );
        assert_eq!(reply.json()["token_endpoint_auth_method"], "none");
        reply.json()["client_id"].as_str().unwrap().to_owned()
    }

    async fn refresh(&self, client: &str, token: &str, changes: &[(&str, &str)]) -> Reply {
        let mut pairs = vec![
            ("grant_type", "refresh_token"),
            ("client_id", client),
            ("refresh_token", token),
        ];
        for (key, value) in changes {
            pairs.retain(|(k, _)| k != key);
            pairs.push((key, value));
        }
        self.request(
            "POST",
            "/oauth/token",
            encoded(&pairs),
            &[("content-type", "application/x-www-form-urlencoded")],
        )
        .await
    }

    async fn initial_tokens(&self, client: &str) -> Value {
        let code = self.code(client).await;
        let reply = self.exchange(client, &code, &[]).await;
        assert_eq!(reply.status, StatusCode::OK);
        reply.json()
    }

    async fn mcp_status(&self, access: &str) -> StatusCode {
        let bearer = format!("Bearer {access}");
        let initialize = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"test","version":"1"}}});
        self.request(
            "POST",
            "/mcp",
            initialize.to_string(),
            &[
                ("authorization", &bearer),
                ("content-type", "application/json"),
                ("accept", "application/json, text/event-stream"),
            ],
        )
        .await
        .status
    }

    async fn exchange(&self, client: &str, code: &str, changes: &[(&str, &str)]) -> Reply {
        let mut pairs = vec![
            ("grant_type", "authorization_code"),
            ("client_id", client),
            ("code", code),
            ("redirect_uri", CALLBACK),
            ("resource", RESOURCE),
            ("code_verifier", VERIFIER),
        ];
        for (key, value) in changes {
            pairs.retain(|(k, _)| k != key);
            pairs.push((key, value));
        }
        self.request(
            "POST",
            "/oauth/token",
            encoded(&pairs),
            &[("content-type", "application/x-www-form-urlencoded")],
        )
        .await
    }
}

#[derive(Clone, Default)]
struct DiagnosticWriter(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for DiagnosticWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn capture_diagnostics(filter: &str, future: impl std::future::Future<Output = ()>) -> String {
    let writer = DiagnosticWriter::default();
    let output = writer.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(filter))
        .without_time()
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    // A current-thread runtime keeps every request under this scoped subscriber.
    tracing::subscriber::with_default(subscriber, || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(future);
    });
    String::from_utf8(output.0.lock().unwrap().clone()).unwrap()
}

#[test]
fn diagnostic_classifiers_only_return_fixed_labels() {
    for (input, expected) in [
        (None, "omitted"),
        (Some("none"), "none"),
        (Some("client_secret_basic"), "client_secret_basic"),
        (Some("client_secret_post"), "client_secret_post"),
        (Some("malicious\nsecret"), "other"),
        (Some("NONE"), "other"),
    ] {
        let registration = Registration {
            redirect_uris: vec![CALLBACK.into()],
            client_name: None,
            token_endpoint_auth_method: input.map(str::to_owned),
            grant_types: Some(vec![
                "authorization_code".into(),
                "refresh_token".into(),
                "malicious\nsecret".into(),
            ]),
            response_types: None,
            scope: None,
        };
        let safe = registration_diagnostic(&registration);
        assert_eq!(safe.auth_method, expected);
        assert!(
            safe.grant_types_present
                && safe.grant_authcode
                && safe.grant_refresh
                && safe.grant_other
        );
    }
    assert_eq!(diagnostic_route("/oauth/register/malicious"), "/other");
    assert_eq!(
        diagnostic_method(&Method::from_bytes(b"MALICIOUS").unwrap()),
        "OTHER"
    );
    assert_eq!(diagnostic_error("malicious\nsecret").0, "other");
}

#[test]
fn diagnostics_capture_safe_routes_errors_and_dcr_rejections_only() {
    let mut secrets = Vec::new();
    let output = capture_diagnostics("warn,qmcp::oauth_diagnostics=info", async {
        let h = Harness::new();
        let r = h
            .request(
                "GET",
                "/canary-path?secret=canary-querysecret",
                String::new(),
                &[],
            )
            .await;
        assert_eq!(r.status, StatusCode::NOT_FOUND);
        let r = h
            .request("CANARYMETHOD", "/canary-path", String::new(), &[])
            .await;
        assert_eq!(r.status, StatusCode::NOT_FOUND);
        let r = h
            .request(
                "GET",
                "/mcp?secret=canary-querysecret",
                String::new(),
                &[
                    ("authorization", "Bearer canary-token"),
                    ("cookie", "canary-cookie"),
                ],
            )
            .await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED);
        for (header, value, reason) in [
            ("host", "canary-host", "host_rejected"),
            ("origin", "https://canary-origin.example", "origin_rejected"),
        ] {
            let r = h
                .request(
                    "GET",
                    "/canary-path?secret=canary-querysecret",
                    String::new(),
                    &[(header, value)],
                )
                .await;
            assert_eq!(r.status, StatusCode::FORBIDDEN, "{reason}");
        }
        let r = h
            .request(
                "POST",
                "/oauth/register?secret=canary-querysecret",
                "canary-body".into(),
                &[],
            )
            .await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST);
        let r = h
            .request(
                "POST",
                "/oauth/register",
                "{\"canary-unknownmetadatakey-malicious\":\"canary-body\", broken".into(),
                &[("content-type", "application/json")],
            )
            .await;
        assert_eq!(r.json(), json!({"error": "invalid_client_metadata"}));
        let invalid = [
            (
                json!({"redirect_uris": ["https://canary-redirect.example/cb#canary-code"]}),
                "invalid_redirect",
            ),
            (
                json!({"redirect_uris": [CALLBACK], "client_name": "canary-name\n"}),
                "invalid_client_name",
            ),
            (
                json!({"redirect_uris": [CALLBACK], "token_endpoint_auth_method": "client_secret_basic"}),
                "unsupported_auth_method",
            ),
            (
                json!({"redirect_uris": [CALLBACK], "token_endpoint_auth_method": "client_secret_post"}),
                "unsupported_auth_method",
            ),
            (
                json!({"redirect_uris": [CALLBACK], "token_endpoint_auth_method": "canary-authmethod"}),
                "unsupported_auth_method",
            ),
            (
                json!({"redirect_uris": [CALLBACK], "grant_types": ["authorization_code", "refresh_token", "canary-grant"]}),
                "unsupported_grant",
            ),
            (
                json!({"redirect_uris": [CALLBACK], "response_types": ["canary-response"]}),
                "unsupported_response",
            ),
            (
                json!({"redirect_uris": [CALLBACK], "scope": "canary-scope"}),
                "unsupported_scope",
            ),
        ];
        for (metadata, _) in invalid {
            let r = h.register(metadata).await;
            assert_eq!(r.status, StatusCode::BAD_REQUEST);
            assert_eq!(r.json(), json!({"error": "invalid_client_metadata"}));
            assert!(h.auth.0.store.lock().unwrap().clients.is_empty());
            assert!(
                !r.headers
                    .keys()
                    .any(|key| key.as_str().contains("diagnostic"))
            );
        }
        for path in [
            "/.well-known/oauth-protected-resource",
            "/.well-known/oauth-protected-resource/mcp",
            "/.well-known/oauth-authorization-server",
        ] {
            assert_eq!(
                h.request("GET", path, String::new(), &[]).await.status,
                StatusCode::OK
            );
        }
        let r = h
            .register(json!({
                "redirect_uris": [CALLBACK], "token_endpoint_auth_method": "none",
                "grant_types": ["authorization_code", "refresh_token"], "response_types": ["code"], "scope": SCOPE,
                "canary-unknownmetadatakey-malicious": {"nested": "canary-body"},
                "logo_uri": "https://canary-logo.example/icon"
            }))
            .await;
        assert_eq!(r.status, StatusCode::CREATED);
        assert_eq!(r.json()["token_endpoint_auth_method"], "none");
        let client = r.json()["client_id"].as_str().unwrap().to_owned();
        let page = h
            .authorize(&client, &[("future_extension", "canary-querysecret")])
            .await;
        let (cookie, csrf) = Harness::browser(&page);
        let r = h
            .consent(&cookie, &csrf, "canary-owner", "approve", Some(ISSUER))
            .await;
        assert_eq!(r.parameter("error"), "access_denied");
        let code = h.code(&client).await;
        let r = h
            .exchange(
                &client,
                &code,
                &[("future_extension", "canary-clientsecret")],
            )
            .await;
        assert_eq!(r.status, StatusCode::OK);
        let token = r.json()["access_token"].as_str().unwrap().to_owned();
        // Check generated credentials too, not just recognizable input canaries.
        let refresh = r.json()["refresh_token"].as_str().unwrap().to_owned();
        let rotated = h
            .refresh(
                &client,
                &refresh,
                &[("future_extension", "canary-refresh-secret")],
            )
            .await;
        assert_eq!(rotated.status, StatusCode::OK);
        secrets.push(rotated.json()["refresh_token"].as_str().unwrap().to_owned());
        secrets.push(rotated.json()["access_token"].as_str().unwrap().to_owned());
        assert_eq!(
            h.refresh(&client, &refresh, &[]).await.json()["error"],
            "invalid_grant"
        );
        assert_eq!(
            h.refresh(&client, "canary-refresh-secret", &[])
                .await
                .status,
            StatusCode::BAD_REQUEST
        );
        secrets.extend([client, code, token, refresh, cookie, csrf]);
        let r = h
            .exchange(
                "canary-clientid",
                "canary-code",
                &[("client_secret", "canary-clientsecret")],
            )
            .await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST);
        assert_eq!(
            h.register(json!({"redirect_uris": [CALLBACK]}))
                .await
                .status,
            StatusCode::CREATED
        );
    });
    assert!(!output.is_empty());
    for line in output.lines() {
        assert!(line.contains("qmcp::oauth_diagnostics"));
        assert!(!line.to_ascii_lowercase().contains("canary"), "{line}");
        assert!(!line.contains(SECRET), "{line}");
    }
    for secret in secrets {
        assert!(!secret.is_empty());
        assert!(!output.contains(&secret));
    }
    for expected in [
        "route=\"/other\" method=\"GET\" status=404",
        "route=\"/other\" method=\"OTHER\" status=404",
        "route=\"/mcp\" method=\"GET\" status=401 oauth_error=\"invalid_token\"",
        "route=\"/oauth/register\" method=\"POST\" status=400",
        "status=201 oauth_error=\"none\" reason=\"registered\"",
        "reason=\"host_rejected\"",
        "reason=\"origin_rejected\"",
        "reason=\"post_query\"",
        "reason=\"invalid_json\"",
        "reason=\"invalid_redirect\"",
        "reason=\"invalid_client_name\"",
        "reason=\"unsupported_auth_method\"",
        "reason=\"unsupported_grant\"",
        "reason=\"unsupported_response\"",
        "reason=\"unsupported_scope\"",
        "auth_method=\"none\"",
        "auth_method=\"omitted\"",
        "auth_method=\"other\"",
        "auth_method=\"client_secret_basic\"",
        "auth_method=\"client_secret_post\"",
        "grant_authcode=true grant_refresh=true grant_other=true",
        "status=201 oauth_error=\"none\" reason=\"registered\" auth_method=\"none\" grant_types_present=true grant_authcode=true grant_refresh=true grant_other=false",
        "route=\"/oauth/authorize\" method=\"POST\" status=200 oauth_error=\"access_denied\"",
        "route=\"/oauth/token\" method=\"POST\" status=400 oauth_error=\"invalid_grant\"",
        "route=\"/.well-known/oauth-protected-resource\"",
        "route=\"/.well-known/oauth-protected-resource/mcp\"",
        "route=\"/.well-known/oauth-authorization-server\"",
    ] {
        assert!(output.contains(expected), "missing {expected}: {output}");
    }
    let disabled = capture_diagnostics("warn", async {
        let h = Harness::new();
        assert_eq!(
            h.register(json!({"redirect_uris": [CALLBACK]}))
                .await
                .status,
            StatusCode::CREATED
        );
    });
    assert!(disabled.is_empty(), "{disabled}");
}

#[test]
fn diagnostics_preserve_dcr_body_rate_and_capacity_failures() {
    let output = capture_diagnostics("warn,qmcp::oauth_diagnostics=info", async {
        let h = Harness::new();
        for (body, headers, status) in [
            (
                "canary-body".into(),
                vec![],
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
            ),
            (
                "x".repeat(MAX_BODY + 1),
                vec![("content-type", "application/json")],
                StatusCode::PAYLOAD_TOO_LARGE,
            ),
        ] {
            assert_eq!(
                h.request("POST", "/oauth/register", body, &headers)
                    .await
                    .status,
                status
            );
        }
        let permits = h.auth.0.body_slots.acquire_many(16).await.unwrap();
        assert_eq!(
            h.register(json!({"redirect_uris": [CALLBACK]}))
                .await
                .status,
            StatusCode::TOO_MANY_REQUESTS
        );
        drop(permits);
        h.auth.0.store.lock().unwrap().registration_rate = Rate {
            start: Some(Instant::now()),
            used: REGISTRATION_RATE,
        };
        assert_eq!(
            h.register(json!({"redirect_uris": [CALLBACK]}))
                .await
                .status,
            StatusCode::TOO_MANY_REQUESTS
        );

        let h = Harness::new();
        for _ in 0..16 {
            h.client().await;
        }
        assert_eq!(
            h.register(json!({"redirect_uris": [CALLBACK]}))
                .await
                .status,
            StatusCode::TOO_MANY_REQUESTS
        );
        {
            let mut store = h.auth.0.store.lock().unwrap();
            for i in store.registration_origins.len()..MAX_REGISTRATION_ORIGINS {
                store.registration_origins.insert(
                    format!("https://origin-{i}.example"),
                    Rate {
                        start: Some(Instant::now()),
                        used: 1,
                    },
                );
            }
        }
        assert_eq!(
            h.register(json!({"redirect_uris": ["https://new-origin.example/cb"]}))
                .await
                .status,
            StatusCode::TOO_MANY_REQUESTS
        );
        {
            let mut store = h.auth.0.store.lock().unwrap();
            store.registration_origins.clear();
            for i in store.clients.len()..MAX_UNAPPROVED_CLIENTS {
                store.clients.insert(
                    format!("canary-clientid-{i}"),
                    Client {
                        redirects: vec![CALLBACK.into()],
                        name: "canary-clientname".into(),
                        expires: Instant::now() + CLIENT_TTL,
                        approved: false,
                        refresh_allowed: false,
                        authorization_rate: Rate::default(),
                        login_rate: Rate::default(),
                        exchange_rate: Rate::default(),
                    },
                );
            }
        }
        let r = h.register(json!({"redirect_uris": [CALLBACK]})).await;
        assert_eq!(r.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(r.json(), json!({"error": "temporarily_unavailable"}));
    });
    for expected in [
        "reason=\"body_rejected\"",
        "reason=\"registration_rate\"",
        "reason=\"origin_rate\"",
        "reason=\"origin_capacity\"",
        "reason=\"client_capacity\"",
        "status=415",
        "status=413",
        "status=429",
        "status=503",
    ] {
        assert!(output.contains(expected), "missing {expected}: {output}");
    }
    assert!(!output.contains("canary"));
}

#[tokio::test]
async fn discovery_and_all_route_host_origin_guards() {
    let h = Harness::new();
    for path in [
        "/.well-known/oauth-protected-resource",
        "/.well-known/oauth-protected-resource/mcp",
    ] {
        let r = h.request("GET", path, String::new(), &[]).await;
        assert_eq!(r.status, StatusCode::OK);
        assert_eq!(r.json()["resource"], RESOURCE);
        assert_eq!(r.json()["authorization_servers"], json!([ISSUER]));
    }
    let r = h
        .request(
            "GET",
            "/.well-known/oauth-authorization-server",
            String::new(),
            &[],
        )
        .await;
    let metadata = r.json();
    assert_eq!(metadata["issuer"], ISSUER);
    assert_eq!(
        metadata["code_challenge_methods_supported"],
        json!(["S256"])
    );
    assert_eq!(
        metadata["grant_types_supported"],
        json!(["authorization_code", "refresh_token"])
    );
    assert_eq!(
        metadata["token_endpoint_auth_methods_supported"],
        json!(["none"])
    );
    assert_eq!(
        metadata["authorization_response_iss_parameter_supported"],
        true
    );
    assert!(metadata.get("refresh_token").is_none());
    for (key, path) in [
        ("authorization_endpoint", "/oauth/authorize"),
        ("token_endpoint", "/oauth/token"),
        ("registration_endpoint", "/oauth/register"),
    ] {
        assert_eq!(metadata[key], format!("{ISSUER}{path}"));
    }
    for path in [
        "/mcp",
        "/unknown",
        "/oauth/authorize",
        "/oauth/register",
        "/oauth/token",
        "/.well-known/oauth-authorization-server",
        "/.well-known/oauth-protected-resource",
    ] {
        let r = h
            .request("GET", path, String::new(), &[("host", "evil.example")])
            .await;
        assert_eq!(r.status, StatusCode::FORBIDDEN, "{path}");
        let r = h
            .request("GET", path, String::new(), &[("origin", ISSUER)])
            .await;
        assert_eq!(r.status, StatusCode::FORBIDDEN, "{path}");
    }
    let r = h
        .request(
            "POST",
            "/oauth/token?owner_secret=not-allowed",
            String::new(),
            &[],
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn dcr_validation_body_bounds_and_rate_limit() {
    for value in [
        json!({"redirect_uris": []}),
        json!({"redirect_uris": ["http://remote.example/cb"]}),
        json!({"redirect_uris": ["https://u:p@client.example/cb"]}),
        json!({"redirect_uris": ["https://client.example/cb#x"]}),
        json!({"redirect_uris": ["https://client.example/cb?code=x"]}),
        json!({"redirect_uris": [CALLBACK], "token_endpoint_auth_method": "client_secret_post"}),
        json!({"redirect_uris": [CALLBACK], "grant_types": ["refresh_token"]}),
        json!({"redirect_uris": [CALLBACK], "grant_types": []}),
        json!({"redirect_uris": [CALLBACK], "grant_types": ["authorization_code", "authorization_code"]}),
        json!({"redirect_uris": [CALLBACK], "grant_types": ["authorization_code", "refresh_token", "refresh_token"]}),
        json!({"redirect_uris": [CALLBACK], "grant_types": ["authorization_code", "client_credentials"]}),
        json!({"redirect_uris": [CALLBACK], "scope": "other"}),
        json!({"redirect_uris": [CALLBACK], "response_types": ["token"]}),
        json!({"redirect_uris": [CALLBACK], "client_name": "x".repeat(129)}),
        json!({"redirect_uris": vec![CALLBACK; 9]}),
    ] {
        let h = Harness::new();
        let r = h.register(value).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{}", r.text);
        assert!(h.auth.0.store.lock().unwrap().clients.is_empty());
    }
    for uri in [
        "http://localhost:3210/callback",
        "http://127.0.0.1:3210/callback",
        "http://[::1]:3210/callback",
        CALLBACK,
    ] {
        assert!(valid_redirect(uri), "{uri}");
    }
    for uri in [
        "http://localhost.evil/cb",
        "http://192.168.0.1/cb",
        "javascript:alert(1)",
        "https://client.example",
        "https://client.example/cb?iss=x",
    ] {
        assert!(!valid_redirect(uri), "{uri}");
    }
    let h = Harness::new();
    let r = h
        .request(
            "POST",
            "/oauth/register",
            "x".repeat(MAX_BODY + 1),
            &[("content-type", "application/json")],
        )
        .await;
    assert_eq!(r.status, StatusCode::PAYLOAD_TOO_LARGE);
    for _ in 0..16 {
        h.client().await;
    }
    assert_eq!(
        h.register(json!({"redirect_uris": [CALLBACK]}))
            .await
            .status,
        StatusCode::TOO_MANY_REQUESTS
    );
}

#[tokio::test]
async fn owner_login_consent_csrf_and_origin_are_mandatory() {
    let h = Harness::new();
    let client = h.client().await;
    let page = h.authorize(&client, &[]).await;
    assert!(
        page.text
            .contains("&lt;script&gt;untrusted &amp; client&lt;/script&gt;")
    );
    assert!(!page.text.contains("<script>"));
    assert!(!page.text.contains(SECRET));
    let set_cookie = page.headers[header::SET_COOKIE].to_str().unwrap();
    for attribute in [
        "Secure",
        "HttpOnly",
        "SameSite=Lax",
        "Path=/oauth/authorize",
        "Max-Age=300",
    ] {
        assert!(set_cookie.contains(attribute));
    }
    let (cookie, csrf) = Harness::browser(&page);
    for origin in [
        None,
        Some("null"),
        Some("https://evil.example"),
        Some("https://mcp.example.com.evil"),
        Some("https://mcp.example.com/"),
    ] {
        assert_eq!(
            h.consent(&cookie, &csrf, SECRET, "approve", origin)
                .await
                .status,
            StatusCode::FORBIDDEN
        );
    }
    assert_eq!(
        h.consent(&cookie, "wrong-csrf", SECRET, "approve", Some(ISSUER))
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        h.consent(&cookie, &csrf, SECRET, "approve", Some(ISSUER))
            .await
            .status,
        StatusCode::OK
    );
    // The invalid CSRF did not destroy the flow; valid approval is now consumed.
    assert_eq!(
        h.consent(&cookie, &csrf, SECRET, "approve", Some(ISSUER))
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    h.auth.0.store.lock().unwrap().codes.clear();
    for secret in ["", "wrong-password"] {
        let page = h.authorize(&client, &[]).await;
        let (cookie, csrf) = Harness::browser(&page);
        let r = h
            .consent(&cookie, &csrf, secret, "approve", Some(ISSUER))
            .await;
        assert_eq!(r.parameter("error"), "access_denied");
        assert_eq!(r.parameter("iss"), ISSUER);
        assert!(!r.text.contains(secret) || secret.is_empty());
    }
    let page = h.authorize(&client, &[]).await;
    let (cookie, csrf) = Harness::browser(&page);
    let r = h.consent(&cookie, &csrf, "", "deny", Some(ISSUER)).await;
    assert_eq!(r.parameter("error"), "access_denied");
    assert_eq!(r.parameter("state"), STATE);
    assert!(h.auth.0.store.lock().unwrap().codes.is_empty());
    let page = h.authorize(&client, &[]).await;
    let (cookie, csrf) = Harness::browser(&page);
    let r = h.consent(&cookie, &csrf, SECRET, "", Some(ISSUER)).await;
    assert_eq!(r.parameter("error"), "access_denied");
    assert!(h.auth.0.store.lock().unwrap().codes.is_empty());
}

#[tokio::test]
async fn authorize_exact_redirect_resource_scope_pkce_and_encoded_errors() {
    let h = Harness::new();
    let client = h.client().await;
    for changes in [
        vec![("response_type", "token")],
        vec![("resource", "https://other.example/mcp")],
        vec![("scope", "other")],
        vec![("code_challenge_method", "plain")],
        vec![("code_challenge", "short")],
    ] {
        let r = h.authorize(&client, &changes).await;
        assert_eq!(r.status, StatusCode::OK);
        assert_eq!(r.parameter("iss"), ISSUER);
        assert_eq!(r.parameter("state"), STATE);
        assert!(r.location().query_pairs().any(|(k, _)| k == "error"));
    }
    for target in [
        "https://client.example/callback",
        "https://client.example/callback?existing=2",
        "https://evil.example/callback",
    ] {
        let r = h.authorize(&client, &[("redirect_uri", target)]).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST);
        assert!(!r.headers.contains_key(header::LOCATION));
    }
    assert_eq!(
        h.authorize(&client, &[("state", &"x".repeat(1025))])
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        h.authorize(&client, &[("owner_secret", SECRET)])
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    assert!(h.auth.0.store.lock().unwrap().transactions.is_empty());
}

#[tokio::test]
async fn code_exchange_pkce_bindings_and_replay() {
    for (key, wrong) in [
        ("client_id", "other-client"),
        ("redirect_uri", "https://client.example/callback"),
        ("resource", "https://other.example/mcp"),
        ("scope", "other"),
        ("code_verifier", "too-short"),
        (
            "code_verifier",
            "abcdefghijklmnopqrstuvwxyz0123456789ABCDEFG!bad",
        ),
        (
            "code_verifier",
            "abcdefghijklmnopqrstuvwxyz0123456789ABCDEFG._~x",
        ),
    ] {
        let h = Harness::new();
        let client = h.client().await;
        let code = h.code(&client).await;
        assert_eq!(
            h.exchange(&client, &code, &[(key, wrong)]).await.status,
            StatusCode::BAD_REQUEST,
            "{key}"
        );
        assert_eq!(
            h.exchange(&client, &code, &[]).await.status,
            StatusCode::BAD_REQUEST
        );
        assert!(h.auth.0.store.lock().unwrap().tokens.is_empty());
    }
    let h = Harness::new();
    let client = h.client().await;
    let code = h.code(&client).await;
    let (a, b) = tokio::join!(
        h.exchange(&client, &code, &[]),
        h.exchange(&client, &code, &[])
    );
    assert_eq!(
        [a.status, b.status]
            .iter()
            .filter(|s| **s == StatusCode::OK)
            .count(),
        1
    );
    let token_reply = if a.status == StatusCode::OK { a } else { b };
    let json = token_reply.json();
    assert_eq!(json["expires_in"], 1800);
    assert_eq!(json["scope"], SCOPE);
    assert!(json.get("refresh_token").is_none());
    let token = json["access_token"].as_str().unwrap();
    {
        let store = h.auth.0.store.lock().unwrap();
        assert!(store.codes.is_empty());
        assert!(store.tokens.contains_key(&digest(token)));
    }
    assert!(h.auth.accepts(token));
    assert!(!h.auth.accepts("not-a-token"));
}

#[tokio::test]
async fn token_protects_real_mcp_transport_without_origin_stripping() {
    let h = Harness::new();
    let client = h.client().await;
    let code = h.code(&client).await;
    let token = h.exchange(&client, &code, &[]).await.json()["access_token"]
        .as_str()
        .unwrap()
        .to_owned();
    let initialize = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"test","version":"1"}}}).to_string();
    for authorization in [None, Some("Bearer bad")] {
        let mut headers = vec![
            ("content-type", "application/json"),
            ("accept", "application/json, text/event-stream"),
        ];
        if let Some(value) = authorization {
            headers.push(("authorization", value));
        }
        let r = h
            .request("POST", "/mcp", initialize.clone(), &headers)
            .await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED);
        assert!(
            r.headers[header::WWW_AUTHENTICATE]
                .to_str()
                .unwrap()
                .contains("resource_metadata")
        );
    }
    let bearer = format!("Bearer {token}");
    let headers = [
        ("content-type", "application/json"),
        ("accept", "application/json, text/event-stream"),
        ("authorization", bearer.as_str()),
    ];
    let r = h
        .request("POST", "/mcp", initialize.clone(), &headers)
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    assert!(r.json()["result"]["serverInfo"].is_object());
    let mut browser_headers = headers.to_vec();
    browser_headers.push(("origin", ISSUER));
    assert_eq!(
        h.request("POST", "/mcp", initialize, &browser_headers)
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    h.auth
        .0
        .store
        .lock()
        .unwrap()
        .tokens
        .get_mut(&digest(&token))
        .unwrap()
        .expires = Instant::now() - Duration::from_secs(1);
    assert!(!h.auth.accepts(&token));
}

#[tokio::test]
async fn expiry_capacity_pruning_and_global_rate_bounds() {
    let h = Harness::new();
    let client = h.client().await;
    let page = h.authorize(&client, &[]).await;
    let (cookie, csrf) = Harness::browser(&page);
    for transaction in h.auth.0.store.lock().unwrap().transactions.values_mut() {
        transaction.expires = Instant::now() - Duration::from_secs(1);
    }
    assert_eq!(
        h.consent(&cookie, &csrf, SECRET, "approve", Some(ISSUER))
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    let code = h.code(&client).await;
    h.auth
        .0
        .store
        .lock()
        .unwrap()
        .codes
        .get_mut(&digest(&code))
        .unwrap()
        .expires = Instant::now() - Duration::from_secs(1);
    assert_eq!(
        h.exchange(&client, &code, &[]).await.status,
        StatusCode::BAD_REQUEST
    );
    {
        let mut store = h.auth.0.store.lock().unwrap();
        for i in 0..MAX_CLIENTS {
            store.clients.insert(
                format!("filler-{i}"),
                Client {
                    name: String::new(),
                    redirects: vec![CALLBACK.into()],
                    expires: Instant::now() + CLIENT_TTL,
                    approved: false,
                    refresh_allowed: false,
                    authorization_rate: Rate::default(),
                    login_rate: Rate::default(),
                    exchange_rate: Rate::default(),
                },
            );
        }
    }
    assert_eq!(
        h.register(json!({"redirect_uris": [CALLBACK]}))
            .await
            .status,
        StatusCode::SERVICE_UNAVAILABLE
    );
    for client in h.auth.0.store.lock().unwrap().clients.values_mut() {
        client.approved = false;
        client.expires = Instant::now() - Duration::from_secs(1);
    }
    assert_eq!(
        h.register(json!({"redirect_uris": [CALLBACK]}))
            .await
            .status,
        StatusCode::CREATED
    );
    assert_eq!(h.auth.0.store.lock().unwrap().clients.len(), 1);
    let mut rate = Rate::default();
    let now = Instant::now();
    for _ in 0..8 {
        assert!(rate.allow(now, 8));
    }
    assert!(!rate.allow(now, 8));
    assert!(rate.allow(now + Duration::from_secs(60), 8));
    let h = Harness::new();
    let client = h.client().await;
    for _ in 0..8 {
        let page = h.authorize(&client, &[]).await;
        let (cookie, csrf) = Harness::browser(&page);
        h.consent(&cookie, &csrf, "wrong", "approve", Some(ISSUER))
            .await;
    }
    let page = h.authorize(&client, &[]).await;
    let (cookie, csrf) = Harness::browser(&page);
    assert_eq!(
        h.consent(&cookie, &csrf, SECRET, "approve", Some(ISSUER))
            .await
            .status,
        StatusCode::TOO_MANY_REQUESTS
    );
}

fn grant(client: &str) -> Grant {
    Grant {
        client: client.into(),
        redirect: CALLBACK.into(),
        resource: RESOURCE.into(),
        scope: SCOPE.into(),
        challenge: digest(VERIFIER),
    }
}

#[tokio::test]
async fn every_state_table_is_capped_and_expired_entries_release_slots() {
    let h = Harness::new();
    let client = h.client().await;
    {
        let mut store = h.auth.0.store.lock().unwrap();
        for i in 0..MAX_TRANSACTIONS {
            store.transactions.insert(
                digest(&i.to_string()),
                Transaction {
                    grant: grant(&client),
                    state: None,
                    csrf: digest("csrf"),
                    expires: Instant::now() + TRANSACTION_TTL,
                },
            );
        }
    }
    let response = h.authorize(&client, &[]).await;
    assert_eq!(response.parameter("error"), "temporarily_unavailable");
    assert_eq!(
        h.auth.0.store.lock().unwrap().transactions.len(),
        MAX_TRANSACTIONS
    );
    for t in h.auth.0.store.lock().unwrap().transactions.values_mut() {
        t.expires = Instant::now() - Duration::from_secs(1);
    }
    let page = h.authorize(&client, &[]).await;
    assert_eq!(h.auth.0.store.lock().unwrap().transactions.len(), 1);
    let (cookie, csrf) = Harness::browser(&page);
    {
        let mut store = h.auth.0.store.lock().unwrap();
        for i in 0..MAX_CODES {
            store.codes.insert(
                digest(&i.to_string()),
                Code {
                    grant: grant(&client),
                    expires: Instant::now() + CODE_TTL,
                },
            );
        }
    }
    let response = h
        .consent(&cookie, &csrf, SECRET, "approve", Some(ISSUER))
        .await;
    assert_eq!(response.parameter("error"), "temporarily_unavailable");
    assert_eq!(h.auth.0.store.lock().unwrap().codes.len(), MAX_CODES);
    for c in h.auth.0.store.lock().unwrap().codes.values_mut() {
        c.expires = Instant::now() - Duration::from_secs(1);
    }
    let code = h.code(&client).await;
    assert_eq!(h.auth.0.store.lock().unwrap().codes.len(), 1);
    {
        let mut store = h.auth.0.store.lock().unwrap();
        for i in 0..MAX_TOKENS {
            store.tokens.insert(
                digest(&i.to_string()),
                Token {
                    family: None,
                    client: client.clone(),
                    resource: RESOURCE.into(),
                    scope: SCOPE.into(),
                    expires: Instant::now() + TOKEN_TTL,
                },
            );
        }
    }
    assert_eq!(
        h.exchange(&client, &code, &[]).await.status,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(h.auth.0.store.lock().unwrap().tokens.len(), MAX_TOKENS);
    for token in h.auth.0.store.lock().unwrap().tokens.values_mut() {
        token.expires = Instant::now() - Duration::from_secs(1);
    }
    let code = h.code(&client).await;
    assert_eq!(h.exchange(&client, &code, &[]).await.status, StatusCode::OK);
    assert_eq!(h.auth.0.store.lock().unwrap().tokens.len(), 1);
}

#[tokio::test]
async fn body_concurrency_duplicates_cookies_and_rate_limits_fail_closed() {
    let h = Harness::new();
    let client = h.client().await;
    let permits = h.auth.0.body_slots.acquire_many(16).await.unwrap();
    assert_eq!(
        h.register(json!({"redirect_uris": [CALLBACK]}))
            .await
            .status,
        StatusCode::TOO_MANY_REQUESTS
    );
    drop(permits);
    assert!(fields(b"state=one&state=two").is_err());
    for invalid in [
        b"state=%".as_slice(),
        b"state=%GG",
        b"state=%FF",
        b"state=\xff",
    ] {
        assert!(fields(invalid).is_err());
    }
    assert_eq!(
        param(&fields(b"state=%EF%BF%BD").ok().unwrap(), "state"),
        "\u{fffd}"
    );
    assert!(fields(&vec![b'x'; MAX_BODY + 1]).is_err());
    let page = h.authorize(&client, &[]).await;
    let (cookie, csrf) = Harness::browser(&page);
    assert_eq!(
        h.consent("", &csrf, SECRET, "approve", Some(ISSUER))
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    let duplicate = format!("{cookie}; {cookie}");
    assert_eq!(
        h.consent(&duplicate, &csrf, SECRET, "approve", Some(ISSUER))
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    let response = h
        .request(
            "POST",
            "/oauth/authorize",
            encoded(&[
                ("csrf", &csrf),
                ("decision", "approve"),
                ("owner_secret", SECRET),
            ]),
            &[
                ("content-type", "application/x-www-form-urlencoded"),
                ("cookie", &cookie),
                ("origin", ISSUER),
                ("origin", ISSUER),
            ],
        )
        .await;
    assert_eq!(response.status, StatusCode::FORBIDDEN);
    h.auth.0.store.lock().unwrap().authorization_rate.used = AGGREGATE_RATE;
    assert_eq!(
        h.authorize(&client, &[]).await.status,
        StatusCode::TOO_MANY_REQUESTS
    );
    {
        let mut store = h.auth.0.store.lock().unwrap();
        store.exchange_rate.start = Some(Instant::now());
        store.exchange_rate.used = AGGREGATE_RATE;
    }
    assert_eq!(
        h.exchange(&client, "unknown", &[]).await.status,
        StatusCode::TOO_MANY_REQUESTS
    );
}

#[tokio::test]
async fn browser_pages_have_exact_policy_and_private_explicit_completion_links() {
    let h = Harness::new();
    let client = h.client().await;
    for decision in ["approve", "deny"] {
        let page = h.authorize(&client, &[]).await;
        assert_eq!(page.headers["referrer-policy"], "same-origin");
        assert_eq!(
            page.headers["content-security-policy"],
            "default-src 'none'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'"
        );
        assert!(
            page.text
                .contains("method=\"post\" action=\"/oauth/authorize\"")
        );
        let (cookie, csrf) = Harness::browser(&page);
        let result = h
            .consent(&cookie, &csrf, SECRET, decision, Some(ISSUER))
            .await;
        assert_eq!(result.headers["referrer-policy"], "same-origin");
        assert_eq!(result.headers[header::CACHE_CONTROL], "no-store");
        assert_eq!(result.headers["pragma"], "no-cache");
        assert_eq!(
            result.headers["content-security-policy"],
            page.headers["content-security-policy"]
        );
        assert!(!result.headers.contains_key(header::LOCATION));
        assert!(
            result
                .text
                .contains("rel=\"noreferrer noopener\">Continue to ChatGPT</a>")
        );
        assert!(result.text.contains("&amp;iss="));
        for forbidden in [
            "<script",
            "<form",
            "http-equiv",
            SECRET,
            "<script>untrusted",
        ] {
            assert!(!result.text.contains(forbidden), "{forbidden}");
        }
        assert_eq!(result.parameter("state"), STATE);
        assert_eq!(result.parameter("iss"), ISSUER);
        assert_eq!(result.parameter("existing"), "1");
        if decision == "approve" {
            assert_eq!(
                h.exchange(&client, &result.parameter("code"), &[])
                    .await
                    .status,
                StatusCode::OK
            );
        } else {
            assert_eq!(result.parameter("error"), "access_denied");
            assert!(
                !result
                    .location()
                    .query_pairs()
                    .any(|(key, _)| key == "code")
            );
        }
    }
    let metadata = h
        .request(
            "GET",
            "/.well-known/oauth-authorization-server",
            String::new(),
            &[],
        )
        .await;
    assert_eq!(metadata.headers["referrer-policy"], "no-referrer");
}

#[tokio::test]
async fn approved_clients_survive_one_hour_and_spam_cannot_evict_them() {
    let h = Harness::new();
    let response = h.register(json!({"redirect_uris": [CALLBACK]})).await;
    assert!(response.json().get("client_id_expires_in").is_none());
    let approved = response.json()["client_id"].as_str().unwrap().to_owned();
    h.code(&approved).await;
    let pending = h.client().await;
    {
        let mut store = h.auth.0.store.lock().unwrap();
        assert!(store.clients[&approved].approved);
        assert!(!store.clients[&pending].approved);
        // Advance pruning past the original registration deadline, without sleeping.
        store.prune(Instant::now() + CLIENT_TTL + Duration::from_secs(1));
        assert!(store.clients.contains_key(&approved));
        assert!(!store.clients.contains_key(&pending));
    }
    assert_eq!(h.authorize(&approved, &[]).await.status, StatusCode::OK);
    for i in 0..MAX_UNAPPROVED_CLIENTS {
        let mut store = h.auth.0.store.lock().unwrap();
        store.clients.insert(
            format!("spam-{i}"),
            Client {
                redirects: vec![CALLBACK.into()],
                name: String::new(),
                expires: Instant::now() + CLIENT_TTL,
                approved: false,
                refresh_allowed: false,
                authorization_rate: Rate::default(),
                login_rate: Rate::default(),
                exchange_rate: Rate::default(),
            },
        );
    }
    assert_eq!(
        h.register(json!({"redirect_uris": [CALLBACK]}))
            .await
            .status,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert!(
        h.auth
            .0
            .store
            .lock()
            .unwrap()
            .clients
            .contains_key(&approved)
    );
    assert_eq!(h.authorize(&approved, &[]).await.status, StatusCode::OK);
    {
        let mut store = h.auth.0.store.lock().unwrap();
        store.prune(Instant::now() + CLIENT_TTL + Duration::from_secs(1));
        assert_eq!(store.clients.len(), 1);
    }
    assert_eq!(
        h.register(json!({"redirect_uris": [CALLBACK]}))
            .await
            .status,
        StatusCode::CREATED
    );
}

#[tokio::test]
async fn invalid_consent_does_not_consume_owner_attempts_or_transaction() {
    let h = Harness::new();
    let client = h.client().await;
    let page = h.authorize(&client, &[]).await;
    let (cookie, csrf) = Harness::browser(&page);
    for _ in 0..32 {
        assert_eq!(
            h.consent("", &csrf, SECRET, "approve", Some(ISSUER))
                .await
                .status,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            h.consent(&cookie, "wrong", SECRET, "approve", Some(ISSUER))
                .await
                .status,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            h.consent(&cookie, &csrf, SECRET, "approve", Some("null"))
                .await
                .status,
            StatusCode::FORBIDDEN
        );
    }
    {
        let store = h.auth.0.store.lock().unwrap();
        assert_eq!(store.clients[&client].login_rate.used, 0);
        assert_eq!(store.transactions.len(), 1);
    }
    let result = h
        .consent(&cookie, &csrf, SECRET, "approve", Some(ISSUER))
        .await;
    assert!(!result.parameter("code").is_empty());
    assert_eq!(
        h.auth.0.store.lock().unwrap().clients[&client]
            .login_rate
            .used,
        1
    );
}

#[tokio::test]
async fn attacker_transactions_and_token_requests_do_not_lock_other_clients() {
    let h = Harness::new();
    let legitimate = h.client().await;
    let attacker = h.client().await;
    for _ in 0..8 {
        let page = h.authorize(&attacker, &[]).await;
        let (cookie, csrf) = Harness::browser(&page);
        let result = h
            .consent(&cookie, &csrf, "wrong", "approve", Some(ISSUER))
            .await;
        assert_eq!(result.parameter("error"), "access_denied");
    }
    let page = h.authorize(&attacker, &[]).await;
    let (cookie, csrf) = Harness::browser(&page);
    assert_eq!(
        h.consent(&cookie, &csrf, SECRET, "approve", Some(ISSUER))
            .await
            .status,
        StatusCode::TOO_MANY_REQUESTS
    );
    for _ in 0..MAX_CLIENT_TRANSACTIONS - 1 {
        Harness::browser(&h.authorize(&attacker, &[]).await);
    }
    assert_eq!(
        h.authorize(&attacker, &[]).await.parameter("error"),
        "temporarily_unavailable"
    );
    // Unknown codes, even naming the victim, cannot spend its exchange budget.
    for _ in 0..160 {
        assert_eq!(
            h.exchange(&legitimate, "unknown", &[]).await.status,
            StatusCode::BAD_REQUEST
        );
    }
    let code = h.code(&legitimate).await;
    assert_eq!(
        h.exchange(&legitimate, &code, &[]).await.status,
        StatusCode::OK
    );
    assert_eq!(
        h.auth.0.store.lock().unwrap().clients[&legitimate]
            .exchange_rate
            .used,
        1
    );
    // Saturating an attacker's known-client bucket cannot block another client.
    h.auth
        .0
        .store
        .lock()
        .unwrap()
        .clients
        .get_mut(&attacker)
        .unwrap()
        .exchange_rate = Rate {
        start: Some(Instant::now()),
        used: 60,
    };
    let code = h.code(&legitimate).await;
    assert_eq!(
        h.exchange(&legitimate, &code, &[]).await.status,
        StatusCode::OK
    );
}

#[tokio::test]
async fn unknown_oauth_extensions_are_ignored_without_fetching_or_rendering_urls() {
    let h = Harness::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let untrusted = format!(
        "http://{}/<script>not-rendered</script>",
        listener.local_addr().unwrap()
    );
    let r = h
        .register(json!({
            "redirect_uris": [CALLBACK], "client_uri": untrusted, "logo_uri": untrusted,
            "policy_uri": untrusted, "tos_uri": untrusted, "extension": {"nested": [1, 2]}
        }))
        .await;
    assert_eq!(r.status, StatusCode::CREATED);
    assert!(!r.text.contains("not-rendered"));
    let client = r.json()["client_id"].as_str().unwrap().to_owned();
    let page = h
        .authorize(&client, &[("future_extension", &untrusted)])
        .await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(!page.text.contains("not-rendered"));
    let (cookie, csrf) = Harness::browser(&page);
    let result = h
        .consent(&cookie, &csrf, SECRET, "approve", Some(ISSUER))
        .await;
    let token = h
        .exchange(
            &client,
            &result.parameter("code"),
            &[("future_extension", &untrusted)],
        )
        .await;
    assert_eq!(token.status, StatusCode::OK);
    assert!(!token.text.contains("not-rendered"));
    assert!(
        tokio::time::timeout(Duration::from_millis(100), listener.accept())
            .await
            .is_err(),
        "metadata URL was fetched"
    );
    let duplicate = format!(
        "{{\"redirect_uris\":[\"{CALLBACK}\"],\"token_endpoint_auth_method\":\"none\",\"token_endpoint_auth_method\":\"none\"}}"
    );
    assert_eq!(
        h.request(
            "POST",
            "/oauth/register",
            duplicate,
            &[("content-type", "application/json")]
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
    assert!(fields(b"future_extension=one&future_extension=two").is_ok());
    for key in [
        "client_id",
        "resource",
        "code",
        "code_verifier",
        "grant_type",
        "code_challenge_method",
        "client_assertion",
        "client_assertion_type",
    ] {
        assert!(fields(format!("{key}=one&{key}=two").as_bytes()).is_err());
    }
    let code = h.code(&client).await;
    assert_eq!(
        h.exchange(&client, &code, &[("grant_type", "refresh_token")])
            .await
            .json()["error"],
        "invalid_grant"
    );
    assert_eq!(
        h.exchange(&client, &code, &[("grant_type", "client_credentials")])
            .await
            .json()["error"],
        "unsupported_grant_type"
    );
    // Dispatching another grant must not consume an authorization code.
    assert_eq!(h.exchange(&client, &code, &[]).await.status, StatusCode::OK);
    assert_eq!(
        h.authorize(&client, &[("response_mode", "fragment")])
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn client_code_token_and_registration_origin_limits_are_isolated_and_bounded() {
    let h = Harness::new();
    let attacker = h.client().await;
    let legitimate = h.client().await;
    {
        let mut store = h.auth.0.store.lock().unwrap();
        for i in 0..MAX_CLIENT_CODES {
            store.codes.insert(
                digest(&format!("code-{i}")),
                Code {
                    grant: grant(&attacker),
                    expires: Instant::now() + CODE_TTL,
                },
            );
        }
    }
    let page = h.authorize(&attacker, &[]).await;
    let (cookie, csrf) = Harness::browser(&page);
    assert_eq!(
        h.consent(&cookie, &csrf, SECRET, "approve", Some(ISSUER))
            .await
            .parameter("error"),
        "temporarily_unavailable"
    );
    assert_eq!(h.auth.0.store.lock().unwrap().codes.len(), MAX_CLIENT_CODES);
    let code = h.code(&legitimate).await;
    assert_eq!(
        h.exchange(&legitimate, &code, &[]).await.status,
        StatusCode::OK
    );
    {
        let mut store = h.auth.0.store.lock().unwrap();
        store.codes.clear();
        for i in 0..MAX_CLIENT_TOKENS {
            store.tokens.insert(
                digest(&format!("token-{i}")),
                Token {
                    family: None,
                    client: attacker.clone(),
                    resource: RESOURCE.into(),
                    scope: SCOPE.into(),
                    expires: Instant::now() + TOKEN_TTL,
                },
            );
        }
    }
    let code = h.code(&attacker).await;
    assert_eq!(
        h.exchange(&attacker, &code, &[]).await.status,
        StatusCode::SERVICE_UNAVAILABLE
    );
    let code = h.code(&legitimate).await;
    assert_eq!(
        h.exchange(&legitimate, &code, &[]).await.status,
        StatusCode::OK
    );

    let h = Harness::new();
    for _ in 0..16 {
        h.client().await;
    }
    assert_eq!(
        h.register(json!({"redirect_uris": [CALLBACK]}))
            .await
            .status,
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(
        h.register(json!({"redirect_uris": ["https://unrelated.example/callback"]}))
            .await
            .status,
        StatusCode::CREATED
    );
    {
        let mut store = h.auth.0.store.lock().unwrap();
        for i in store.registration_origins.len()..MAX_REGISTRATION_ORIGINS {
            store.registration_origins.insert(
                format!("https://origin-{i}.example"),
                Rate {
                    start: Some(Instant::now()),
                    used: 1,
                },
            );
        }
    }
    assert_eq!(
        h.register(json!({"redirect_uris": ["https://new-origin.example/callback"]}))
            .await
            .status,
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(
        h.auth.0.store.lock().unwrap().registration_origins.len(),
        MAX_REGISTRATION_ORIGINS
    );
    h.auth
        .0
        .store
        .lock()
        .unwrap()
        .prune(Instant::now() + Duration::from_secs(61));
    assert!(
        h.auth
            .0
            .store
            .lock()
            .unwrap()
            .registration_origins
            .is_empty()
    );
    assert_eq!(
        h.register(json!({"redirect_uris": ["https://new-origin.example/callback"]}))
            .await
            .status,
        StatusCode::CREATED
    );
}

#[tokio::test]
async fn recognized_authentication_fields_and_null_duplicates_stay_strict() {
    let h = Harness::new();
    let duplicate = format!(
        "{{\"redirect_uris\":[\"{CALLBACK}\"],\"token_endpoint_auth_method\":null,\"token_endpoint_auth_method\":\"none\"}}"
    );
    assert_eq!(
        h.request(
            "POST",
            "/oauth/register",
            duplicate,
            &[("content-type", "application/json")]
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
    let client = h.client().await;
    for key in ["client_secret", "client_assertion", "client_assertion_type"] {
        assert_eq!(
            h.authorize(&client, &[(key, "not-supported")]).await.status,
            StatusCode::BAD_REQUEST
        );
        let code = h.code(&client).await;
        assert_eq!(
            h.exchange(&client, &code, &[(key, "not-supported")])
                .await
                .status,
            StatusCode::BAD_REQUEST
        );
    }
}

#[tokio::test]
async fn chatgpt_dcr_grant_sets_are_normalized_and_metadata_urls_are_not_fetched() {
    let h = Harness::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let metadata_url = format!("http://{}/metadata", listener.local_addr().unwrap());
    for grants in [
        json!(["authorization_code", "refresh_token"]),
        json!(["refresh_token", "authorization_code"]),
    ] {
        let reply = h
            .register(json!({
                "redirect_uris": [CALLBACK], "client_name": "ChatGPT",
                "token_endpoint_auth_method": "none", "grant_types": grants,
                "response_types": ["code"], "scope": SCOPE,
                "client_uri": metadata_url, "logo_uri": metadata_url,
                "application_type": "web", "unknown_safe": {"nested": true}
            }))
            .await;
        assert_eq!(reply.status, StatusCode::CREATED);
        assert_eq!(
            reply.json()["grant_types"],
            json!(["authorization_code", "refresh_token"])
        );
        assert_eq!(reply.json()["token_endpoint_auth_method"], "none");
        assert!(!reply.text.contains("metadata"));
        let client = reply.json()["client_id"].as_str().unwrap().to_owned();
        assert!(h.auth.0.store.lock().unwrap().clients[&client].refresh_allowed);
    }
    for metadata in [
        json!({"redirect_uris": [CALLBACK]}),
        json!({"redirect_uris": [CALLBACK], "grant_types": ["authorization_code"]}),
    ] {
        let reply = h.register(metadata).await;
        assert_eq!(reply.status, StatusCode::CREATED);
        assert_eq!(reply.json()["grant_types"], json!(["authorization_code"]));
        let client = reply.json()["client_id"].as_str().unwrap().to_owned();
        assert!(!h.auth.0.store.lock().unwrap().clients[&client].refresh_allowed);
        let tokens = h.initial_tokens(&client).await;
        assert!(tokens.get("refresh_token").is_none());
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(100), listener.accept())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn browser_consent_discloses_registered_grant_lifetimes_before_owner_login() {
    assert_eq!(TOKEN_TTL.as_secs() % 60, 0);
    assert_eq!(FAMILY_TTL.as_secs() % 3600, 0);
    let h = Harness::new();
    let callback = format!("{CALLBACK}&label=%3Cuntrusted%3E");
    for (grants, refresh_allowed) in [
        (None, false),
        (Some(json!(["authorization_code"])), false),
        (Some(json!(["authorization_code", "refresh_token"])), true),
        (Some(json!(["refresh_token", "authorization_code"])), true),
    ] {
        let mut metadata = json!({
            "redirect_uris": [callback], "client_name": "<script>untrusted & client</script>",
            "token_endpoint_auth_method": "none",
            "refresh_allowed": !refresh_allowed,
            "consent_duration": "<script>untrusted-duration</script>"
        });
        if let Some(grants) = grants {
            metadata["grant_types"] = grants;
        }
        let registration = h.register(metadata).await;
        assert_eq!(registration.status, StatusCode::CREATED);
        let client = registration.json()["client_id"]
            .as_str()
            .unwrap()
            .to_owned();
        let page = h
            .authorize(
                &client,
                &[
                    ("redirect_uri", &callback),
                    (
                        "refresh_allowed",
                        if refresh_allowed { "false" } else { "true" },
                    ),
                    (
                        "grant_types",
                        if refresh_allowed {
                            "authorization_code"
                        } else {
                            "refresh_token"
                        },
                    ),
                    ("consent_duration", "<script>untrusted-duration</script>"),
                ],
            )
            .await;
        // Inspect the GET response only: no owner credentials or approval submitted.
        Harness::browser(&page);
        assert!(page.text.contains(&format!(
            "Approve grants all QueryMT tools on {RESOURCE} (scope {SCOPE})."
        )));
        assert!(
            page.text
                .contains("Only approve a client and callback you trust.")
        );
        assert_eq!(
            page.text.contains("Access lasts up to 30 minutes."),
            !refresh_allowed
        );
        assert_eq!(
            page.text
                .contains("Each access token lasts up to 30 minutes."),
            refresh_allowed
        );
        assert_eq!(
            page.text.contains("Access can be renewed without a new login for up to 24 hours from the initial code exchange; renewal does not extend this deadline."),
            refresh_allowed
        );
        assert_eq!(page.text.contains("24 hours"), refresh_allowed);
        assert_eq!(page.text.contains("renew"), refresh_allowed);
        assert!(!page.text.contains("for 30 minutes"));
        assert!(
            page.text
                .contains("&lt;script&gt;untrusted &amp; client&lt;/script&gt;")
        );
        assert!(
            page.text
                .contains(&format!("Redirect: {}", escape(&callback)))
        );
        for forbidden in ["<script", "untrusted-duration", SECRET] {
            assert!(!page.text.contains(forbidden), "{forbidden}");
        }
        let store = h.auth.0.store.lock().unwrap();
        assert_eq!(store.clients[&client].refresh_allowed, refresh_allowed);
        assert!(!store.clients[&client].approved);
        assert!(store.codes.is_empty());
        assert!(store.tokens.is_empty());
        assert!(store.families.is_empty());
        assert!(store.refresh.is_empty());
    }
}

#[tokio::test]
async fn refresh_issuance_requires_registered_permission_owner_consent_and_pkce() {
    let h = Harness::new();
    let client = h.refresh_client().await;
    assert!(h.auth.0.store.lock().unwrap().refresh.is_empty());
    let page = h.authorize(&client, &[]).await;
    let (cookie, csrf) = Harness::browser(&page);
    let denied = h
        .consent(&cookie, &csrf, SECRET, "deny", Some(ISSUER))
        .await;
    assert_eq!(denied.parameter("error"), "access_denied");
    assert!(h.auth.0.store.lock().unwrap().refresh.is_empty());
    let code = h.code(&client).await;
    let bad = h
        .exchange(&client, &code, &[("code_verifier", "too-short")])
        .await;
    assert_eq!(bad.json()["error"], "invalid_grant");
    assert!(h.auth.0.store.lock().unwrap().refresh.is_empty());
    let tokens = h.initial_tokens(&client).await;
    assert_eq!(tokens["expires_in"], 1800);
    let refresh = tokens["refresh_token"].as_str().unwrap();
    assert_eq!(URL_SAFE_NO_PAD.decode(refresh).unwrap().len(), 32);
    assert!(!h.auth.accepts(refresh));
    let no_refresh_client = h.client().await;
    assert_eq!(
        h.refresh(&no_refresh_client, refresh, &[]).await.json()["error"],
        "invalid_grant"
    );
    let no_refresh = h.initial_tokens(&no_refresh_client).await;
    assert!(no_refresh.get("refresh_token").is_none());
    assert_eq!(
        h.refresh(
            &no_refresh_client,
            no_refresh["access_token"].as_str().unwrap(),
            &[]
        )
        .await
        .json()["error"],
        "invalid_grant"
    );
    assert_eq!(
        h.refresh(&client, refresh, &[]).await.status,
        StatusCode::OK
    );
}

#[tokio::test]
async fn refresh_rotation_replay_revokes_only_its_family_including_real_mcp_access() {
    let h = Harness::new();
    let client = h.refresh_client().await;
    let other_client = h.refresh_client().await;
    let original = h.initial_tokens(&client).await;
    let independent = h.initial_tokens(&client).await;
    let other = h.initial_tokens(&other_client).await;
    let old = original["refresh_token"].as_str().unwrap();
    let family = digest(old);
    let deadline = h.auth.0.store.lock().unwrap().families[&family].expires;
    let rotated = h.refresh(&client, old, &[]).await;
    assert_eq!(rotated.status, StatusCode::OK);
    let rotated = rotated.json();
    let current = rotated["refresh_token"].as_str().unwrap();
    assert!(old != current);
    assert!(original["access_token"] != rotated["access_token"]);
    assert_eq!(rotated["expires_in"], 1800);
    {
        let store = h.auth.0.store.lock().unwrap();
        assert_eq!(store.families[&family].expires, deadline);
        assert!(store.refresh[&digest(old)].consumed);
        assert!(!store.refresh[&digest(current)].consumed);
        assert_eq!(store.families[&family].records, 2);
    }
    for tokens in [&original, &rotated, &independent, &other] {
        assert_eq!(
            h.mcp_status(tokens["access_token"].as_str().unwrap()).await,
            StatusCode::OK
        );
    }
    let replay = h.refresh(&client, old, &[]).await;
    assert_eq!(replay.status, StatusCode::BAD_REQUEST);
    assert_eq!(replay.json(), json!({"error": "invalid_grant"}));
    for tokens in [&original, &rotated] {
        assert_eq!(
            h.mcp_status(tokens["access_token"].as_str().unwrap()).await,
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        h.refresh(&client, current, &[]).await.json()["error"],
        "invalid_grant"
    );
    for (id, tokens) in [(&client, &independent), (&other_client, &other)] {
        assert_eq!(
            h.mcp_status(tokens["access_token"].as_str().unwrap()).await,
            StatusCode::OK
        );
        assert_eq!(
            h.refresh(id, tokens["refresh_token"].as_str().unwrap(), &[])
                .await
                .status,
            StatusCode::OK
        );
    }
    let mut store = h.auth.0.store.lock().unwrap();
    store.prune(Instant::now() + TOKEN_TTL + Duration::from_secs(1));
    assert!(store.families[&family].revoked);
    assert!(store.refresh.contains_key(&digest(old)));
    assert!(store.refresh.contains_key(&digest(current)));
    store.prune(deadline);
    assert!(!store.families.contains_key(&family));
    assert!(!store.refresh.contains_key(&digest(old)));
}

#[tokio::test]
async fn invalid_refresh_bindings_and_malformed_requests_do_not_consume_or_revoke() {
    let h = Harness::new();
    let client = h.refresh_client().await;
    let other = h.refresh_client().await;
    let original = h.initial_tokens(&client).await;
    let raw = original["refresh_token"].as_str().unwrap();
    for (key, value, expected) in [
        ("client_id", other.as_str(), "invalid_grant"),
        ("client_id", "", "invalid_grant"),
        ("resource", "https://other.example/mcp", "invalid_target"),
        (
            "resource",
            "https://mcp.example.com:443/mcp",
            "invalid_target",
        ),
        ("resource", "https://mcp.example.com/mcp/", "invalid_target"),
        ("resource", "", "invalid_target"),
        ("scope", "qmcp:access other", "invalid_scope"),
        ("scope", "", "invalid_scope"),
        ("scope", "qmcp:access ", "invalid_scope"),
        ("refresh_token", "not-an-opaque-token", "invalid_grant"),
        ("client_secret", "", "invalid_grant"),
        ("client_assertion", "canary-refresh-secret", "invalid_grant"),
        ("code", "mixed-grants", "invalid_grant"),
        ("owner_secret", "not-required", "invalid_grant"),
    ] {
        let reply = h.refresh(&client, raw, &[(key, value)]).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST);
        assert_eq!(reply.json()["error"], expected, "{key}");
        let store = h.auth.0.store.lock().unwrap();
        assert!(!store.refresh[&digest(raw)].consumed);
        assert!(!store.families[&digest(raw)].revoked);
    }
    let body = encoded(&[
        ("grant_type", "refresh_token"),
        ("client_id", &client),
        ("refresh_token", raw),
    ]);
    for malformed in [
        format!("{body}&refresh_token={raw}"),
        format!("{body}&client_id={client}"),
        format!("{body}&resource={RESOURCE}&resource={RESOURCE}"),
        format!("{body}&scope={SCOPE}&scope={SCOPE}"),
        format!("{body}&broken=%GG"),
        encoded(&[("grant_type", "refresh_token"), ("refresh_token", raw)]),
    ] {
        assert_eq!(
            h.request(
                "POST",
                "/oauth/token",
                malformed,
                &[("content-type", "application/x-www-form-urlencoded")]
            )
            .await
            .status,
            StatusCode::BAD_REQUEST
        );
    }
    for (path, extra, status) in [
        ("/oauth/token?x=1", None, StatusCode::BAD_REQUEST),
        (
            "/oauth/token",
            Some(("origin", ISSUER)),
            StatusCode::FORBIDDEN,
        ),
        (
            "/oauth/token",
            Some(("origin", "null")),
            StatusCode::FORBIDDEN,
        ),
        (
            "/oauth/token",
            Some(("host", "evil.example")),
            StatusCode::FORBIDDEN,
        ),
        (
            "/oauth/token",
            Some(("authorization", "Basic canary-refresh-secret")),
            StatusCode::BAD_REQUEST,
        ),
    ] {
        let mut headers = vec![("content-type", "application/x-www-form-urlencoded")];
        if let Some(extra) = extra {
            headers.push(extra);
        }
        assert_eq!(
            h.request("POST", path, body.clone(), &headers).await.status,
            status
        );
    }
    assert!(h.auth.accepts(original["access_token"].as_str().unwrap()));
    let rotated = h
        .refresh(
            &client,
            raw,
            &[
                ("resource", RESOURCE),
                ("scope", SCOPE),
                ("future_extension", "ignored"),
            ],
        )
        .await;
    assert_eq!(rotated.status, StatusCode::OK);
    // Even a consumed digest with wrong bindings must not revoke the family.
    for changes in [
        vec![("client_id", other.as_str())],
        vec![("resource", "wrong")],
        vec![("scope", "wrong")],
    ] {
        assert_eq!(
            h.refresh(&client, raw, &changes).await.status,
            StatusCode::BAD_REQUEST
        );
        assert!(!h.auth.0.store.lock().unwrap().families[&digest(raw)].revoked);
    }
    assert_eq!(
        h.refresh(
            &client,
            rotated.json()["refresh_token"].as_str().unwrap(),
            &[]
        )
        .await
        .status,
        StatusCode::OK
    );
}

#[tokio::test]
async fn refresh_absolute_expiry_pruning_and_restart_are_volatile() {
    let h = Harness::new();
    let client = h.refresh_client().await;
    let start = Instant::now();
    let original = h.initial_tokens(&client).await;
    let access = original["access_token"].as_str().unwrap();
    let raw = original["refresh_token"].as_str().unwrap();
    {
        let mut store = h.auth.0.store.lock().unwrap();
        assert!(store.families[&digest(raw)].expires >= start + FAMILY_TTL);
        assert!(store.families[&digest(raw)].expires <= Instant::now() + FAMILY_TTL);
        store.tokens.get_mut(&digest(access)).unwrap().expires =
            Instant::now() - Duration::from_secs(1);
    }
    assert!(!h.auth.accepts(access));
    let rotated = h.refresh(&client, raw, &[]).await;
    assert_eq!(rotated.status, StatusCode::OK);
    let rotated = rotated.json();
    let current = rotated["refresh_token"].as_str().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    h.auth
        .0
        .store
        .lock()
        .unwrap()
        .families
        .get_mut(&digest(raw))
        .unwrap()
        .expires = deadline;
    let last = h.refresh(&client, current, &[]).await;
    assert_eq!(last.status, StatusCode::OK);
    assert!(last.json()["expires_in"].as_u64().unwrap() <= 10);
    {
        let mut store = h.auth.0.store.lock().unwrap();
        assert_eq!(store.families[&digest(raw)].expires, deadline);
        assert!(
            store.tokens[&digest(last.json()["access_token"].as_str().unwrap())].expires
                <= deadline
        );
        store.prune(deadline);
        assert!(store.families.is_empty());
        assert!(store.refresh.is_empty());
        assert!(store.tokens.is_empty());
        assert!(store.codes.is_empty());
        assert!(store.transactions.is_empty());
    }
    assert_eq!(
        h.refresh(&client, current, &[]).await.json()["error"],
        "invalid_grant"
    );
    let fresh = h.initial_tokens(&client).await;
    let restarted = Harness::new();
    assert!(
        !restarted
            .auth
            .accepts(fresh["access_token"].as_str().unwrap())
    );
    assert_eq!(
        restarted
            .refresh(&client, fresh["refresh_token"].as_str().unwrap(), &[])
            .await
            .json()["error"],
        "invalid_grant"
    );
    assert!(restarted.auth.0.store.lock().unwrap().clients.is_empty());
    assert_eq!(
        restarted.authorize(&client, &[]).await.status,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_refresh_exchanges_issue_once_then_replay_revokes_the_winner() {
    let h = Arc::new(Harness::new());
    let client = h.refresh_client().await;
    let tokens = h.initial_tokens(&client).await;
    let raw = tokens["refresh_token"].as_str().unwrap().to_owned();
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let mut jobs = Vec::new();
    for _ in 0..2 {
        let (h, client, raw, barrier) = (h.clone(), client.clone(), raw.clone(), barrier.clone());
        jobs.push(tokio::spawn(async move {
            barrier.wait().await;
            h.refresh(&client, &raw, &[]).await
        }));
    }
    let a = jobs.remove(0).await.unwrap();
    let b = jobs.remove(0).await.unwrap();
    assert_eq!(
        [a.status, b.status]
            .iter()
            .filter(|s| **s == StatusCode::OK)
            .count(),
        1
    );
    let (winner, replay) = if a.status == StatusCode::OK {
        (a, b)
    } else {
        (b, a)
    };
    assert_eq!(replay.status, StatusCode::BAD_REQUEST);
    assert_eq!(replay.json()["error"], "invalid_grant");
    let winner = winner.json();
    assert_eq!(
        h.refresh(&client, winner["refresh_token"].as_str().unwrap(), &[])
            .await
            .json()["error"],
        "invalid_grant"
    );
    assert_eq!(
        h.mcp_status(winner["access_token"].as_str().unwrap()).await,
        StatusCode::UNAUTHORIZED
    );
    let store = h.auth.0.store.lock().unwrap();
    assert_eq!(store.families.len(), 1);
    assert_eq!(store.refresh.len(), 2);
    assert!(store.tokens.is_empty());
}

// Synthetic full tables exercise rejection paths without thousands of owner logins.
fn fill_capacity(store: &mut Store, kind: &str, client: &str, now: Instant) {
    match kind {
        "families" | "client_families" => {
            let (limit, owner) = if kind == "families" {
                (MAX_FAMILIES, "unrelated")
            } else {
                (MAX_CLIENT_FAMILIES, client)
            };
            while if kind == "families" {
                store.families.len()
            } else {
                store
                    .families
                    .values()
                    .filter(|f| f.client == client)
                    .count()
            } < limit
            {
                let id = digest(&format!("filler-family-{}", store.families.len()));
                store.families.insert(
                    id,
                    Family {
                        client: owner.into(),
                        resource: RESOURCE.into(),
                        scope: SCOPE.into(),
                        expires: now + FAMILY_TTL,
                        records: 1,
                        revoked: false,
                    },
                );
                store.refresh.insert(
                    id,
                    RefreshRecord {
                        family: id,
                        consumed: false,
                    },
                );
            }
        }
        "records" => {
            while store.refresh.len() < MAX_REFRESH_RECORDS {
                let family_id = digest(&format!("filler-record-family-{}", store.families.len()));
                let count = MAX_FAMILY_RECORDS.min(MAX_REFRESH_RECORDS - store.refresh.len());
                store.families.insert(
                    family_id,
                    Family {
                        client: "unrelated".into(),
                        resource: RESOURCE.into(),
                        scope: SCOPE.into(),
                        expires: now + FAMILY_TTL,
                        records: count,
                        revoked: false,
                    },
                );
                for i in 0..count {
                    store.refresh.insert(
                        digest(&format!("filler-record-{}-{i}", store.families.len())),
                        RefreshRecord {
                            family: family_id,
                            consumed: i + 1 != count,
                        },
                    );
                }
            }
            assert!(store.families.len() <= MAX_FAMILIES);
        }
        "access" | "client_access" => {
            let (limit, owner) = if kind == "access" {
                (MAX_TOKENS, "unrelated")
            } else {
                (MAX_CLIENT_TOKENS, client)
            };
            while if kind == "access" {
                store.tokens.len()
            } else {
                store.tokens.values().filter(|t| t.client == client).count()
            } < limit
            {
                store.tokens.insert(
                    digest(&format!("filler-access-{}", store.tokens.len())),
                    Token {
                        family: None,
                        client: owner.into(),
                        resource: RESOURCE.into(),
                        scope: SCOPE.into(),
                        expires: now + TOKEN_TTL,
                    },
                );
            }
        }
        _ => panic!("unknown capacity fixture"),
    }
}

fn expire_capacity_fillers(store: &mut Store, original_family: Option<Digest32>) {
    let expired = Instant::now() - Duration::from_secs(1);
    for (id, family) in &mut store.families {
        if Some(*id) != original_family {
            family.expires = expired;
        }
    }
    for token in store.tokens.values_mut() {
        if token.family.is_none() {
            token.expires = expired;
        }
    }
    store.prune(Instant::now());
}

#[tokio::test]
async fn initial_refresh_family_capacity_failure_preserves_code_and_pruning_releases_slots() {
    for kind in [
        "families",
        "client_families",
        "records",
        "access",
        "client_access",
    ] {
        let h = Harness::new();
        let client = h.refresh_client().await;
        let code = h.code(&client).await;
        {
            let mut store = h.auth.0.store.lock().unwrap();
            fill_capacity(&mut store, kind, &client, Instant::now());
        }
        let reply = h.exchange(&client, &code, &[]).await;
        assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE, "{kind}");
        assert_eq!(reply.json(), json!({"error": "temporarily_unavailable"}));
        {
            let mut store = h.auth.0.store.lock().unwrap();
            assert!(store.codes.contains_key(&digest(&code)));
            assert!(!store.tokens.values().any(|t| t.family.is_some()));
            expire_capacity_fillers(&mut store, None);
            assert!(store.families.is_empty());
            assert!(store.refresh.is_empty());
            assert!(store.tokens.is_empty());
        }
        let reply = h.exchange(&client, &code, &[]).await;
        assert_eq!(reply.status, StatusCode::OK, "{kind}");
        assert!(reply.json()["refresh_token"].is_string());
        let store = h.auth.0.store.lock().unwrap();
        assert_eq!(store.families.len(), 1);
        assert_eq!(store.refresh.len(), 1);
        assert_eq!(store.tokens.len(), 1);
        assert!(store.codes.is_empty());
    }
}

#[tokio::test]
async fn refresh_capacity_and_rate_failures_preserve_current_token_and_replay_evidence() {
    for kind in ["records", "access", "client_access", "rate"] {
        let h = Harness::new();
        let client = h.refresh_client().await;
        let original = h.initial_tokens(&client).await;
        let old = original["refresh_token"].as_str().unwrap();
        let rotated = h.refresh(&client, old, &[]).await.json();
        let current = rotated["refresh_token"].as_str().unwrap();
        {
            let mut store = h.auth.0.store.lock().unwrap();
            if kind == "rate" {
                store.clients.get_mut(&client).unwrap().exchange_rate.used = 60;
            } else {
                fill_capacity(&mut store, kind, &client, Instant::now());
            }
        }
        let reply = h.refresh(&client, current, &[]).await;
        assert_eq!(
            reply.status,
            if kind == "rate" {
                StatusCode::TOO_MANY_REQUESTS
            } else {
                StatusCode::SERVICE_UNAVAILABLE
            },
            "{kind}"
        );
        {
            let mut store = h.auth.0.store.lock().unwrap();
            assert!(!store.refresh[&digest(current)].consumed);
            assert!(store.refresh[&digest(old)].consumed);
            assert!(!store.families[&digest(old)].revoked);
            assert_eq!(store.families[&digest(old)].records, 2);
            expire_capacity_fillers(&mut store, Some(digest(old)));
            store.clients.get_mut(&client).unwrap().exchange_rate = Rate::default();
        }
        assert!(h.auth.accepts(rotated["access_token"].as_str().unwrap()));
        assert_eq!(
            h.refresh(&client, current, &[]).await.status,
            StatusCode::OK
        );
        assert_eq!(
            h.refresh(&client, old, &[]).await.json()["error"],
            "invalid_grant"
        );
        assert!(!h.auth.accepts(rotated["access_token"].as_str().unwrap()));
    }
}

#[tokio::test]
async fn refresh_chain_is_bounded_without_evicting_history_or_extending_absolute_lifetime() {
    let h = Harness::new();
    let client = h.refresh_client().await;
    let original = h.initial_tokens(&client).await;
    let old = original["refresh_token"].as_str().unwrap();
    let family_id = digest(old);
    let deadline = h.auth.0.store.lock().unwrap().families[&family_id].expires;
    let start = Instant::now();
    let mut current = old.to_owned();
    for i in 1..MAX_FAMILY_RECORDS {
        // Ten-minute virtual steps exercise real access expiry and fixed rate windows.
        let now = start + Duration::from_secs(i as u64 * 600);
        let request = fields(
            encoded(&[
                ("grant_type", "refresh_token"),
                ("client_id", &client),
                ("refresh_token", &current),
            ])
            .as_bytes(),
        )
        .ok()
        .unwrap();
        let response = {
            let mut store = h.auth.0.store.lock().unwrap();
            store.prune(now);
            exchange_refresh(&mut store, &request, now)
        };
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), MAX_BODY).await.unwrap();
        let response: Value = serde_json::from_slice(&body).unwrap();
        current = response["refresh_token"].as_str().unwrap().to_owned();
    }
    {
        let mut store = h.auth.0.store.lock().unwrap();
        assert_eq!(store.refresh.len(), MAX_FAMILY_RECORDS);
        assert_eq!(store.families[&family_id].records, MAX_FAMILY_RECORDS);
        assert_eq!(store.families[&family_id].expires, deadline);
        assert!(store.refresh[&digest(old)].consumed);
        let now = start + Duration::from_secs(MAX_FAMILY_RECORDS as u64 * 600);
        store.prune(now);
        let request =
            fields(encoded(&[("client_id", &client), ("refresh_token", &current)]).as_bytes())
                .ok()
                .unwrap();
        assert_eq!(
            exchange_refresh(&mut store, &request, now).status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert!(!store.refresh[&digest(&current)].consumed);
        let replay = fields(encoded(&[("client_id", &client), ("refresh_token", old)]).as_bytes())
            .ok()
            .unwrap();
        assert_eq!(
            exchange_refresh(&mut store, &replay, now).status(),
            StatusCode::BAD_REQUEST
        );
        assert!(store.families[&family_id].revoked);
        assert!(store.tokens.is_empty());
        assert_eq!(store.refresh.len(), MAX_FAMILY_RECORDS);
        store.prune(deadline);
        assert!(store.families.is_empty());
        assert!(store.refresh.is_empty());
    }
}

// Optional browser harness: only a public test credential and a fresh loopback port.
// The driver forwards browser-generated headers; it never reads deployment files.
#[tokio::test]
#[ignore = "driven by target/oauth-review/browser-check.py"]
async fn browser_fixture_server() {
    let dir =
        std::env::var_os("QMCP_OAUTH_BROWSER_FIXTURE_DIR").expect("explicit fixture directory");
    let dir = std::path::PathBuf::from(dir);
    let h = Harness::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    std::fs::write(
        dir.join("port"),
        listener.local_addr().unwrap().port().to_string(),
    )
    .unwrap();
    axum::serve(listener, h.app)
        .with_graceful_shutdown(async move {
            for _ in 0..1200 {
                if dir.join("done").exists() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .unwrap();
}

#[test]
fn resource_and_secret_validation_do_not_echo_credentials() {
    for resource in [
        "http://mcp.example.com/mcp",
        "https://u:p@mcp.example.com/mcp",
        "https://mcp.example.com/",
        "https://mcp.example.com/mcp?x=1",
        "https://mcp.example.com/mcp#x",
        "https://MCP.example.com/mcp",
    ] {
        assert!(LocalOAuth::new(resource, SECRET).is_err(), "{resource}");
    }
    for secret in [
        "password".into(),
        "a".repeat(64),
        "01".repeat(32),
        SECRET.to_uppercase(),
    ] {
        let e = LocalOAuth::new(RESOURCE, &secret)
            .err()
            .unwrap()
            .to_string();
        assert!(!e.contains(&secret));
    }
}

#[cfg(unix)]
#[test]
fn secret_file_requires_absolute_owned_0600_regular_file_without_links() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let dir = std::env::temp_dir().join(format!("qmcp-oauth-fixture-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = dir.join("fixture");
    std::fs::write(&path, format!("{SECRET}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert!(LocalOAuth::from_secret_file(RESOURCE, &path).is_ok());
    assert!(LocalOAuth::from_secret_file(RESOURCE, Path::new("relative")).is_err());
    for mode in [0o644, 0o660, 0o400, 0o1600] {
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        assert!(LocalOAuth::from_secret_file(RESOURCE, &path).is_err());
    }
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let link = dir.join("symlink");
    symlink(&path, &link).unwrap();
    assert!(LocalOAuth::from_secret_file(RESOURCE, &link).is_err());
    let hardlink = dir.join("hardlink");
    std::fs::hard_link(&path, &hardlink).unwrap();
    assert!(LocalOAuth::from_secret_file(RESOURCE, &hardlink).is_err());
    assert!(LocalOAuth::from_secret_file(RESOURCE, &dir).is_err());
    std::fs::remove_dir_all(&dir).unwrap();
}
