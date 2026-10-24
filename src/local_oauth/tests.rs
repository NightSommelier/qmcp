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
        Url::parse(self.headers[header::LOCATION].to_str().unwrap()).unwrap()
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
        assert_eq!(response.status, StatusCode::SEE_OTHER);
        assert_eq!(response.parameter("iss"), ISSUER);
        assert_eq!(response.parameter("state"), STATE);
        assert_eq!(response.parameter("existing"), "1");
        response.parameter("code")
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
        json!(["authorization_code"])
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
        json!({"redirect_uris": [CALLBACK], "scope": "other"}),
        json!({"redirect_uris": [CALLBACK], "logo_uri": "https://internal.example/ssrf"}),
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
    for _ in 0..15 {
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
        StatusCode::BAD_REQUEST
    );
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
        assert_eq!(r.status, StatusCode::SEE_OTHER);
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
        ("grant_type", "refresh_token"),
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
    h.auth.0.store.lock().unwrap().authorization_rate.used = 60;
    assert_eq!(
        h.authorize(&client, &[]).await.status,
        StatusCode::TOO_MANY_REQUESTS
    );
    {
        let mut store = h.auth.0.store.lock().unwrap();
        store.exchange_rate.start = Some(Instant::now());
        store.exchange_rate.used = 120;
    }
    assert_eq!(
        h.exchange(&client, "unknown", &[]).await.status,
        StatusCode::TOO_MANY_REQUESTS
    );
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
