//! Single-owner OAuth with public-client registration. State is lost on restart.
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

#[cfg(unix)]
use anyhow::Context;
use anyhow::{Result, bail};
use axum::{
    Form, Json, Router,
    extract::{DefaultBodyLimit, Query, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use chrono::{DateTime, Utc};
use oxide_auth::{
    endpoint::{AccessTokenFlow, AuthorizationFlow, OwnerConsent, RefreshFlow, Solicitation},
    frontends::simple::{
        endpoint::{FnSolicitor, Generic, Vacant},
        extensions::{AddonList, Extended, Pkce},
        request::{Body, Request as OAuthRequest, Response as OAuthResponse, Status},
    },
    primitives::{
        authorizer::Authorizer,
        generator::RandomGenerator,
        grant::Grant,
        issuer::{IssuedToken, Issuer, RefreshedToken, TokenMap},
        registrar::{Client, ClientMap, ExactUrl, Registrar},
    },
};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

const SCOPE: &str = "qmcp:access";
const COOKIE: &str = "__Host-qmcp-consent";
type Digest32 = [u8; 32];
type HttpResult = std::result::Result<Response, StatusCode>;

/// Settings for --oauth-local-config. A preconfigured client/callback pair is optional.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalOAuthConfig {
    pub resource: String,
    #[serde(default)]
    pub client_id: String,
    #[serde(default)]
    pub redirect_uri: String,
    pub owner_secret_file: PathBuf,
}

/// Embedded authorization server; no credentials are exposed through Debug.
#[derive(Clone)]
pub struct LocalOAuth(Arc<Server>);

struct Server {
    config: LocalOAuthConfig,
    issuer: String,
    owner: Digest32,
    store: Mutex<Store>,
    readers: tokio::sync::Semaphore,
}

struct Store {
    clients: ClientMap,
    registrations: HashMap<String, (Client, Option<Instant>)>,
    registration_rate: (Instant, u8),
    codes: Codes,
    tokens: Tokens,
    pending: HashMap<Digest32, (OAuthRequest, Instant)>,
}

impl Store {
    fn prune_clients(&mut self) {
        self.tokens.prune();
        let now = Utc::now();
        self.codes.0.retain(|_, grant| grant.until > now);
        self.registrations.retain(|id, (_, expires)| {
            expires.is_none_or(|expiry| expiry > Instant::now())
                || self.codes.0.values().any(|grant| &grant.client_id == id)
                || self.pending.values().any(|(request, expiry)| {
                    *expiry > Instant::now() && request.query.get("client_id") == Some(id)
                })
                || self
                    .tokens
                    .families
                    .iter()
                    .any(|family| !family.revoked && &family.client_id == id)
        });
        self.clients = self
            .registrations
            .values()
            .map(|(client, _)| client.clone())
            .collect();
    }

    fn endpoint(&mut self) -> Generic<&ClientMap, &mut Codes, &mut Tokens> {
        Generic {
            registrar: &self.clients,
            authorizer: &mut self.codes,
            issuer: &mut self.tokens,
            solicitor: Vacant,
            scopes: Vacant,
            response: Vacant,
        }
    }

    fn authorize(
        &mut self,
        request: OAuthRequest,
        decision: OwnerConsent<OAuthResponse>,
    ) -> std::result::Result<OAuthResponse, StatusCode> {
        let mut decision = Some(decision);
        let endpoint = self.endpoint().with_solicitor(FnSolicitor(
            move |_: &mut OAuthRequest, _: Solicitation<'_>| {
                decision.take().unwrap_or(OwnerConsent::Denied)
            },
        ));
        AuthorizationFlow::prepare(Extended::extend_with(endpoint, pkce()))
            .and_then(|mut flow| flow.execute(request))
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)
    }
}

#[derive(Default)]
struct Codes(HashMap<Digest32, Grant>);

impl Authorizer for Codes {
    fn authorize(&mut self, mut grant: Grant) -> std::result::Result<String, ()> {
        self.0.retain(|_, grant| grant.until > Utc::now());
        if self.0.len() >= 128 {
            return Err(());
        }
        grant.until = Utc::now() + chrono::Duration::seconds(90);
        let code = random();
        self.0.insert(digest(&code), grant);
        Ok(code)
    }

    fn extract(&mut self, code: &str) -> std::result::Result<Option<Grant>, ()> {
        Ok(self
            .0
            .remove(&digest(code))
            .filter(|grant| grant.until > Utc::now()))
    }
}

struct Family {
    client_id: String,
    current: IssuedToken,
    expires: DateTime<Utc>,
    used: HashSet<Digest32>,
    revoked: bool,
}

struct Tokens {
    inner: TokenMap<RandomGenerator>,
    families: Vec<Family>,
}

impl Tokens {
    fn prune(&mut self) {
        self.families.retain(|family| {
            if family.expires > Utc::now() {
                return true;
            }
            self.inner.revoke(&family.current.token);
            if let Some(refresh) = &family.current.refresh {
                self.inner.revoke(refresh);
            }
            false
        });
    }

    // Replaying another client's token must not revoke that client's grant.
    fn replayed(&mut self, client_id: &str, refresh: &str) -> bool {
        let Some(family) = self
            .families
            .iter_mut()
            .find(|f| f.client_id == client_id && f.used.contains(&digest(refresh)))
        else {
            return false;
        };
        family.revoked = true;
        self.inner.revoke(&family.current.token);
        if let Some(refresh) = &family.current.refresh {
            self.inner.revoke(refresh);
        }
        true
    }
}

impl Issuer for Tokens {
    fn issue(&mut self, mut grant: Grant) -> std::result::Result<IssuedToken, ()> {
        self.prune();
        if self.families.len() >= 128 {
            return Err(());
        }
        grant.until = Utc::now() + chrono::Duration::minutes(30);
        let client_id = grant.client_id.clone();
        let token = self.inner.issue(grant)?;
        self.families.push(Family {
            client_id,
            current: token.clone(),
            expires: Utc::now() + chrono::Duration::hours(24),
            used: HashSet::new(),
            revoked: false,
        });
        Ok(token)
    }

    fn refresh(
        &mut self,
        refresh: &str,
        mut grant: Grant,
    ) -> std::result::Result<RefreshedToken, ()> {
        let family = self
            .families
            .iter_mut()
            .find(|f| !f.revoked && f.current.refresh.as_deref() == Some(refresh))
            .ok_or(())?;
        if family.expires <= Utc::now() || family.used.len() >= 127 {
            return Err(());
        }
        grant.until = family
            .expires
            .min(Utc::now() + chrono::Duration::minutes(30));
        let token = self.inner.refresh(refresh, grant)?;
        family.used.insert(digest(refresh));
        family.current = IssuedToken {
            token: token.token.clone(),
            refresh: token.refresh.clone(),
            until: token.until,
            token_type: token.token_type.clone(),
        };
        Ok(token)
    }

    fn recover_token<'a>(&'a self, token: &'a str) -> std::result::Result<Option<Grant>, ()> {
        self.inner.recover_token(token)
    }

    fn recover_refresh<'a>(&'a self, refresh: &'a str) -> std::result::Result<Option<Grant>, ()> {
        let Some(family) = self
            .families
            .iter()
            .find(|f| !f.revoked && f.current.refresh.as_deref() == Some(refresh))
        else {
            return Ok(None);
        };
        Ok(self.inner.recover_refresh(refresh)?.map(|mut grant| {
            grant.until = family.expires;
            grant
        }))
    }
}

impl LocalOAuth {
    /// Read a private regular file containing a generated 32-byte hex owner secret.
    pub fn from_config(config: LocalOAuthConfig) -> Result<Self> {
        let secret = read_owner_secret(&config.owner_secret_file)?;
        Self::new(config, secret.strip_suffix('\n').unwrap_or(&secret))
    }

    fn new(config: LocalOAuthConfig, secret: &str) -> Result<Self> {
        let resource = crate::oauth::https_url(&config.resource)?;
        if resource.path() != "/mcp"
            || resource.as_str() != config.resource
            || config.client_id.is_empty() != config.redirect_uri.is_empty()
            || config.client_id.len() > 128
            || !config
                .client_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        {
            bail!(
                "embedded OAuth requires a canonical HTTPS /mcp resource and optional client ID/callback pair"
            );
        }
        if secret.len() != 64 || !secret.bytes().all(|b| b.is_ascii_hexdigit()) {
            bail!("OAuth owner secret must be 32 random bytes encoded as 64 hex characters");
        }
        let mut registrations = HashMap::new();
        if !config.client_id.is_empty() {
            registrations.insert(
                config.client_id.clone(),
                (
                    public_client(
                        &config.client_id,
                        std::slice::from_ref(&config.redirect_uri),
                    )?,
                    None,
                ),
            );
        }
        let clients = registrations
            .values()
            .map(|(client, _)| client.clone())
            .collect();
        Ok(Self(Arc::new(Server {
            issuer: resource.origin().ascii_serialization(),
            owner: digest(secret),
            config,
            readers: tokio::sync::Semaphore::new(16),
            store: Mutex::new(Store {
                clients,
                registrations,
                registration_rate: (Instant::now(), 0),
                codes: Codes::default(),
                tokens: Tokens {
                    inner: TokenMap::new(RandomGenerator::new(32)),
                    families: Vec::new(),
                },
                pending: HashMap::new(),
            }),
        })))
    }

    pub(crate) fn authority(&self) -> &str {
        self.0
            .config
            .resource
            .strip_prefix("https://")
            .expect("validated HTTPS resource")
            .strip_suffix("/mcp")
            .expect("validated /mcp resource")
    }

    pub(crate) fn router(&self, mcp: Router) -> Router {
        let discovery = get(resource_metadata).with_state(self.clone());
        mcp.layer(middleware::from_fn_with_state(self.clone(), require_token))
            .route("/.well-known/oauth-protected-resource", discovery.clone())
            .route("/.well-known/oauth-protected-resource/mcp", discovery)
            .merge(
                Router::new()
                    .route(
                        "/.well-known/oauth-authorization-server",
                        get(server_metadata),
                    )
                    .route("/oauth/register", post(register))
                    .route("/oauth/authorize", get(authorize))
                    .route("/oauth/consent", post(consent))
                    .route("/oauth/token", post(exchange))
                    .layer(DefaultBodyLimit::max(8192))
                    .with_state(self.clone()),
            )
            .layer(middleware::from_fn_with_state(self.clone(), guard))
    }
}

#[cfg(unix)]
fn read_owner_secret(path: &std::path::Path) -> Result<String> {
    use std::{
        io::Read,
        os::unix::fs::{MetadataExt, OpenOptionsExt},
    };
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .context("open OAuth owner secret")?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.mode() & 0o777 != 0o600 || metadata.nlink() != 1 {
        bail!("OAuth owner secret must be a regular file with mode 0600 and no hard links");
    }
    let mut secret = String::new();
    file.take(130)
        .read_to_string(&mut secret)
        .context("read OAuth owner secret")?;
    Ok(secret)
}

#[cfg(not(unix))]
fn read_owner_secret(_path: &std::path::Path) -> Result<String> {
    bail!("embedded OAuth requires Unix secret-file permissions")
}

fn public_client(id: &str, callbacks: &[String]) -> Result<Client> {
    if callbacks.is_empty() || callbacks.len() > 8 {
        bail!("invalid redirect URI count");
    }
    let mut redirects = Vec::new();
    for callback in callbacks {
        let url = reqwest::Url::parse(callback)?;
        let loopback = url.host_str().is_some_and(|host| {
            host == "localhost"
                || host
                    .trim_matches(['[', ']'])
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        });
        if callback.len() > 2048
            || url.host_str().is_none()
            || !(url.scheme() == "https" || (url.scheme() == "http" && loopback))
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
            || url.as_str() != callback
            || url.query_pairs().any(|(key, _)| {
                matches!(
                    key.as_ref(),
                    "code" | "state" | "iss" | "error" | "error_description"
                )
            })
        {
            bail!("invalid redirect URI");
        }
        redirects.push(callback.parse::<ExactUrl>()?.into());
    }
    Ok(Client::public(id, redirects.remove(0), SCOPE.parse()?)
        .with_additional_redirect_uris(redirects))
}

#[derive(Deserialize)]
struct Registration {
    redirect_uris: Vec<String>,
    client_name: Option<String>,
    token_endpoint_auth_method: Option<String>,
    grant_types: Option<Vec<String>>,
    response_types: Option<Vec<String>>,
    scope: Option<String>,
}

async fn register(
    State(auth): State<LocalOAuth>,
    metadata: std::result::Result<Json<Registration>, axum::extract::rejection::JsonRejection>,
) -> HttpResult {
    let metadata = match metadata {
        Ok(Json(metadata)) => metadata,
        Err(error) if error.status() == StatusCode::PAYLOAD_TOO_LARGE => return Err(error.status()),
        Err(_) => return Ok(invalid("invalid_client_metadata")),
    };
    if metadata
        .token_endpoint_auth_method
        .as_deref()
        .is_some_and(|method| method != "none")
        || metadata.grant_types.as_ref().is_some_and(|grants| {
            !grants.iter().any(|g| g == "authorization_code")
                || grants
                    .iter()
                    .any(|g| g != "authorization_code" && g != "refresh_token")
        })
        || metadata
            .response_types
            .as_ref()
            .is_some_and(|types| types != &["code"])
        || metadata
            .scope
            .as_deref()
            .is_some_and(|scope| scope != SCOPE)
        || metadata
            .client_name
            .as_ref()
            .is_some_and(|name| name.len() > 128)
    {
        return Ok(invalid("invalid_client_metadata"));
    }
    let id = random();
    let Ok(client) = public_client(&id, &metadata.redirect_uris) else {
        return Ok(invalid("invalid_redirect_uri"));
    };
    let mut store = auth
        .0
        .store
        .lock()
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    if store.registration_rate.0.elapsed() >= Duration::from_secs(60) {
        store.registration_rate = (Instant::now(), 0);
    }
    if store.registration_rate.1 >= 10 {
        return Err(StatusCode::TOO_MANY_REQUESTS);
    }
    store.registration_rate.1 += 1;
    store.prune_clients();
    if store.registrations.len() >= 128 {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }
    store.clients.register_client(client.clone());
    store.registrations.insert(
        id.clone(),
        (client, Some(Instant::now() + Duration::from_secs(3600))),
    );
    let mut response = json!({"client_id": id, "client_id_issued_at": Utc::now().timestamp(),
        "redirect_uris": metadata.redirect_uris, "token_endpoint_auth_method": "none",
        "grant_types": ["authorization_code", "refresh_token"], "response_types": ["code"], "scope": SCOPE});
    if let Some(name) = metadata.client_name {
        response["client_name"] = json!(name);
    }
    Ok((StatusCode::CREATED, Json(response)).into_response())
}

fn pkce() -> AddonList {
    let mut addons = AddonList::new();
    addons.push_code(Pkce::required());
    addons
}
fn digest(value: &str) -> Digest32 {
    Sha256::digest(value).into()
}
fn random() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}
fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}
fn invalid(name: &'static str) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({"error": name}))).into_response()
}
fn one<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let mut values = headers.get_all(name).iter();
    let value = values.next()?.to_str().ok()?;
    values.next().is_none().then_some(value)
}
fn fields(
    pairs: Vec<(String, String)>,
) -> std::result::Result<HashMap<String, String>, StatusCode> {
    let mut result = HashMap::new();
    for (key, value) in pairs {
        if result.insert(key, value).is_some() {
            return Err(StatusCode::BAD_REQUEST);
        }
    }
    Ok(result)
}

async fn guard(State(auth): State<LocalOAuth>, request: Request, next: Next) -> HttpResult {
    let consent = request.uri().path() == "/oauth/consent" && request.method() == "POST";
    if one(request.headers(), "host") != Some(auth.authority())
        || (request.headers().contains_key(header::ORIGIN)
            && (!consent || one(request.headers(), "origin") != Some(&auth.0.issuer)))
        || (consent && one(request.headers(), "origin") != Some(&auth.0.issuer))
    {
        return Err(StatusCode::FORBIDDEN);
    }
    let oauth = request.uri().path().starts_with("/oauth/")
        || request.uri().path().starts_with("/.well-known/");
    let mut response = if oauth {
        if request.uri().to_string().len() > 8192
            || (request.method() == "POST" && request.uri().query().is_some())
        {
            return Err(StatusCode::BAD_REQUEST);
        }
        let _permit = auth
            .0
            .readers
            .try_acquire()
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
        {
            let mut store = auth
                .0
                .store
                .lock()
                .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
            store
                .pending
                .retain(|_, (_, expires)| *expires > Instant::now());
        }
        tokio::time::timeout(Duration::from_secs(10), next.run(request))
            .await
            .map_err(|_| StatusCode::REQUEST_TIMEOUT)?
    } else {
        next.run(request).await
    };
    for (name, value) in [
        ("cache-control", "no-store"),
        ("referrer-policy", "same-origin"),
        ("x-content-type-options", "nosniff"),
        (
            "content-security-policy",
            "default-src 'none'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'",
        ),
    ] {
        response
            .headers_mut()
            .insert(name, HeaderValue::from_static(value));
    }
    Ok(response)
}

async fn resource_metadata(State(auth): State<LocalOAuth>) -> Json<serde_json::Value> {
    Json(
        json!({"resource": auth.0.config.resource, "authorization_servers": [auth.0.issuer],
        "scopes_supported": [SCOPE], "bearer_methods_supported": ["header"]}),
    )
}
async fn server_metadata(State(auth): State<LocalOAuth>) -> Json<serde_json::Value> {
    Json(
        json!({"issuer": auth.0.issuer, "authorization_response_iss_parameter_supported": true,
        "authorization_endpoint": format!("{}/oauth/authorize", auth.0.issuer),
        "token_endpoint": format!("{}/oauth/token", auth.0.issuer),
        "registration_endpoint": format!("{}/oauth/register", auth.0.issuer),
        "response_types_supported": ["code"], "grant_types_supported": ["authorization_code", "refresh_token"],
        "token_endpoint_auth_methods_supported": ["none"], "code_challenge_methods_supported": ["S256"],
        "scopes_supported": [SCOPE]}),
    )
}

fn oauth_response(response: OAuthResponse, issuer: &str) -> Response {
    if let Some(mut location) = response.location {
        location.query_pairs_mut().append_pair("iss", issuer);
        // A same-origin consent form cannot safely redirect across form-action CSP.
        return Html(format!(
            "<!doctype html><title>Return to client</title><a href=\"{}\" rel=\"noreferrer noopener\">Continue to client</a>",
            escape(location.as_str()),
        )).into_response();
    }
    let status = match response.status {
        Status::Ok => StatusCode::OK,
        Status::Unauthorized => StatusCode::UNAUTHORIZED,
        _ => StatusCode::BAD_REQUEST,
    };
    match response.body {
        Some(Body::Json(body)) => {
            (status, [(header::CONTENT_TYPE, "application/json")], body).into_response()
        }
        Some(Body::Text(body)) => (status, Html(body)).into_response(),
        None => status.into_response(),
    }
}

async fn authorize(
    State(auth): State<LocalOAuth>,
    Query(pairs): Query<Vec<(String, String)>>,
) -> HttpResult {
    let query = fields(pairs)?;
    if !query.contains_key("client_id")
        || !query.contains_key("redirect_uri")
        || query.get("resource") != Some(&auth.0.config.resource)
        || query.get("scope").is_some_and(|s| s != SCOPE)
        || ["owner", "password", "client_secret", "access_token"]
            .iter()
            .any(|k| query.contains_key(*k))
    {
        return Ok(invalid("invalid_request"));
    }
    let request = OAuthRequest {
        query,
        ..Default::default()
    };
    let nonce = random();
    let page = format!(
        "<!doctype html><title>Authorize qmcp</title><h1>Authorize qmcp</h1><p>Allow {} to control this QueryMT instance. Callback: {}</p><form method=post action=/oauth/consent><input type=hidden name=transaction value=\"{}\"><label>Owner secret <input type=password name=owner required autocomplete=off></label><button name=decision value=approve>Log in and approve</button><button name=decision value=deny formnovalidate>Deny</button></form>",
        escape(&request.query["client_id"]),
        escape(&request.query["redirect_uri"]),
        nonce
    );
    let mut store = auth
        .0
        .store
        .lock()
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    if store.pending.len() >= 128 {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }
    let response = store.authorize(
        request.clone(),
        OwnerConsent::InProgress(OAuthResponse {
            body: Some(Body::Text(page)),
            ..Default::default()
        }),
    )?;
    let solicited = response.status == Status::Ok;
    let mut response = oauth_response(response, &auth.0.issuer);
    if solicited {
        store.pending.insert(
            digest(&nonce),
            (request, Instant::now() + Duration::from_secs(300)),
        );
        response.headers_mut().insert(
            header::SET_COOKIE,
            HeaderValue::from_str(&format!(
                "{COOKIE}={nonce}; Secure; HttpOnly; SameSite=Lax; Path=/; Max-Age=300"
            ))
            .expect("random hex cookie is header safe"),
        );
    }
    Ok(response)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Consent {
    transaction: String,
    owner: Option<String>,
    decision: String,
}

async fn consent(
    State(auth): State<LocalOAuth>,
    headers: HeaderMap,
    Form(form): Form<Consent>,
) -> HttpResult {
    let cookies: Vec<_> = headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|h| h.to_str().ok())
        .flat_map(|h| h.split(';'))
        .filter_map(|c| {
            c.trim()
                .strip_prefix(&format!("{COOKIE}="))
                .map(str::to_owned)
        })
        .collect();
    if cookies.len() != 1 || !bool::from(digest(&cookies[0]).ct_eq(&digest(&form.transaction))) {
        return Err(StatusCode::FORBIDDEN);
    }
    let mut store = auth
        .0
        .store
        .lock()
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    let (request, expires) = store
        .pending
        .remove(&digest(&form.transaction))
        .ok_or(StatusCode::FORBIDDEN)?;
    if expires <= Instant::now() {
        return Err(StatusCode::FORBIDDEN);
    }
    let approved = form.decision == "approve"
        && form
            .owner
            .is_some_and(|owner| bool::from(digest(&owner).ct_eq(&auth.0.owner)));
    let decision = if approved {
        OwnerConsent::Authorized("owner".into())
    } else {
        OwnerConsent::Denied
    };
    let response = store.authorize(request, decision)?;
    let mut response = oauth_response(response, &auth.0.issuer);
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_static(
            "__Host-qmcp-consent=; Secure; HttpOnly; SameSite=Lax; Path=/; Max-Age=0",
        ),
    );
    Ok(response)
}

async fn exchange(
    State(auth): State<LocalOAuth>,
    headers: HeaderMap,
    Form(pairs): Form<Vec<(String, String)>>,
) -> HttpResult {
    let body = fields(pairs)?;
    let refresh = body.get("grant_type").is_some_and(|g| g == "refresh_token");
    if headers.contains_key(header::AUTHORIZATION)
        || body
            .get("resource")
            .map_or(!refresh, |r| r != &auth.0.config.resource)
        || body.get("scope").is_some_and(|s| s != SCOPE)
        || body.contains_key("client_secret")
    {
        return Ok(invalid("invalid_request"));
    }
    let mut store = auth
        .0
        .store
        .lock()
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    let client_id = body.get("client_id").ok_or(StatusCode::BAD_REQUEST)?;
    if store.clients.check(client_id, None).is_err() {
        return Ok(invalid("invalid_client"));
    }
    if body
        .get("code")
        .and_then(|code| store.codes.0.get(&digest(code)))
        .is_some_and(|grant| &grant.client_id != client_id)
    {
        return Ok(invalid("invalid_grant"));
    }
    store.tokens.prune();
    // oxide-auth's public-client refresh path does not bind the body client_id.
    if refresh
        && body
            .get("refresh_token")
            .and_then(|token| store.tokens.recover_refresh(token).ok().flatten())
            .is_some_and(|grant| &grant.client_id != client_id)
    {
        return Ok(invalid("invalid_grant"));
    }
    if refresh
        && body
            .get("refresh_token")
            .is_some_and(|token| store.tokens.replayed(client_id, token))
    {
        return Ok(invalid("invalid_grant"));
    }
    let request = OAuthRequest {
        urlbody: body,
        ..Default::default()
    };
    let endpoint = store.endpoint();
    let response = if refresh {
        RefreshFlow::prepare(endpoint).and_then(|mut flow| flow.execute(request))
    } else {
        AccessTokenFlow::prepare(Extended::extend_with(endpoint, pkce()))
            .and_then(|mut flow| flow.execute(request))
    }
    .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    Ok(oauth_response(response, &auth.0.issuer))
}

async fn require_token(
    State(auth): State<LocalOAuth>,
    mut request: Request,
    next: Next,
) -> HttpResult {
    let valid = crate::transport::bearer_credential(request.headers()).is_some_and(|token| {
        let Ok(store) = auth.0.store.lock() else {
            return false;
        };
        store
            .tokens
            .recover_token(token)
            .ok()
            .flatten()
            .is_some_and(|grant| grant.until > Utc::now())
    });
    if !valid {
        return Ok((StatusCode::UNAUTHORIZED, [(header::WWW_AUTHENTICATE,
            format!("Bearer resource_metadata=\"{}/.well-known/oauth-protected-resource/mcp\", scope=\"{SCOPE}\"", auth.0.issuer))]).into_response());
    }
    request.headers_mut().remove(header::AUTHORIZATION);
    Ok(next.run(request).await)
}

#[cfg(all(test, unix))]
mod tests;
