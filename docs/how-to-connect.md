# How To Connect

[Install qmcp](../README.md#install), then choose a setup:

| Setup | Best for | Authentication |
| --- | --- | --- |
| [Local stdio](#local-stdio) | Desktop and CLI MCP clients | Local process permissions |
| [Embedded OAuth on macOS](#embedded-oauth-on-macos-chatgpt) | Single-owner public ChatGPT connections | Built-in owner login + consent; no external provider |
| [Bearer over HTTPS](#bearer-over-https) | Clients with custom authorization headers | Shared token |
| [Private tunnel](#private-tunnel-chatgpt) | ChatGPT without a public endpoint | OpenAI tunnel access |
| [External OAuth provider](#external-oauth-provider-optional) | Deployments with an existing identity provider | Optional external introspection |

Bearer and OAuth are HTTP authentication modes. The tunnel privately connects
ChatGPT to qmcp over stdio; it is not another MCP transport.

## Start QueryMT

All setups need a configured QueryMT instance with any required provider credentials:

```sh
qmtcode --acp-ws
```

qmcp connects to `ws://127.0.0.1:3030/acp/ws` by default. Change it with `--url`
or `QMCP_ACP_URL`. Prefer `wss://` for remote ACP connections; plaintext
non-loopback ACP requires `--allow-insecure`. qmcp does not launch QueryMT.

## Local stdio

Configure your MCP client to launch qmcp:

```json
{
  "mcpServers": {
    "qmcp": {
      "command": "qmcp",
      "args": ["--url", "ws://127.0.0.1:3030/acp/ws"]
    }
  }
}
```

Use an absolute binary path if your client cannot find `qmcp` on `PATH`.
Stdin/stdout carry MCP; logs go to stderr. Leave HTTP auth options and their
secret environment variables unset for stdio.

For local HTTP instead, run `qmcp --transport http` and connect with Streamable
HTTP to `http://127.0.0.1:3031/mcp`. Without authentication, only loopback binds
are allowed. Other local processes can access that endpoint.

## Bearer over HTTPS

Supply a long random token through your service's `QMCP_BEARER_TOKEN` environment
variable (preferred over the `--bearer-token` argument), then run:

```sh
qmcp --transport http --allowed-host mcp.example.com
```

Place an HTTPS reverse proxy in front of `127.0.0.1:3031`. Connect the MCP client
to `https://mcp.example.com/mcp` with this header on **every** request:

```http
Authorization: Bearer <your-token>
```

This works with clients that support custom headers. ChatGPT's public MCP setup
does not accept a manually supplied static API key; use OAuth or a private tunnel.
A shared token grants the full configured tool surface, not per-user permissions.

## Private Tunnel (ChatGPT)

Use [OpenAI Secure MCP Tunnel](https://developers.openai.com/api/docs/guides/secure-mcp-tunnels)
to keep qmcp private. Your account/workspace must allow tunnels and custom MCP servers.

1. Install `tunnel-client` using the linked guide. Create a tunnel in
   [Platform settings](https://platform.openai.com/settings/organization/tunnels),
   link your ChatGPT workspace, and supply its runtime key as `CONTROL_PLANE_API_KEY`.
2. Set `TUNNEL_ID`, replace the qmcp binary path, and run alongside QueryMT:

   ```sh
   tunnel-client init --sample sample_mcp_stdio_local --profile qmcp \
     --tunnel-id "$TUNNEL_ID" --mcp-command "/absolute/path/to/qmcp"
   tunnel-client doctor --profile qmcp --explain
   tunnel-client run --profile qmcp
   ```

3. In [ChatGPT Plugins](https://chatgpt.com/plugins), choose **+ > Add custom MCP server**,
   name it `qmcp`, select **Connection: Tunnel**, and choose your tunnel. Select
   **No authentication** for this private stdio connection and create/install it.
   Tunnel access controls the connection; leave `QMCP_BEARER_TOKEN` and
   `QMCP_OAUTH_CLIENT_SECRET` unset in the qmcp process's environment.
4. Select the plugin with `@` in a chat and ask: **"Use qmcp to list QueryMT profiles."**
   Keep QueryMT and `tunnel-client` running. No public qmcp endpoint is needed.

## Embedded OAuth on macOS (ChatGPT)

qmcp includes a single-owner OAuth authorization server. No Docker, external
identity provider, or separate login service is needed. Start QueryMT locally,
then put qmcp behind a trusted HTTPS reverse proxy. qmcp does not terminate TLS.
The resource must be a canonical HTTPS URL ending in exactly `/mcp`, without
userinfo, query, or fragment. Its origin is the issuer (no trailing slash).

### Generate the owner credential

Generate a new secret, not a human-chosen password. This example creates a
private file without printing its contents, and refuses to overwrite an existing
file. Run it only when preparing a deployment, not as part of a source review:

```sh
umask 077
mkdir -p "$HOME/.config/qmcp"
chmod 700 "$HOME/.config/qmcp"
(set -C; openssl rand -hex 32 > "$HOME/.config/qmcp/owner-secret")
chmod 600 "$HOME/.config/qmcp/owner-secret"
```

Keep the generated value in a trusted password manager for browser login. Never
put it in command arguments, URLs, environment variables, logs, source control,
or a PR. qmcp never creates or prints an owner credential. It accepts 32..64
random bytes encoded as 64..128 lowercase hex characters, optionally followed
by a newline. Length/encoding and obvious-weakness checks cannot prove entropy;
**only use a cryptographically generated secret**, not a phrase or password.
The absolute file path must name a regular file owned by qmcp's user, mode
`0600`, with no final-component symlink or hard links. Keep its parent directory
private and free of untrusted symlinks/ACLs. Secret-file protection currently
requires Unix (including macOS).

### Run and connect

Leave `QMCP_BEARER_TOKEN` and `QMCP_OAUTH_CLIENT_SECRET` unset, then run after
reviewing your deployment configuration:

```sh
qmcp --transport http --permission cancel \
  --oauth-local-resource https://mcp.example.com/mcp \
  --oauth-owner-secret-file "$HOME/.config/qmcp/owner-secret"
```

The default bind is `127.0.0.1:3031`. For a proxy on another host, see the
[Nginx/VPN example](#separate-nginx-server-to-a-mac-over-vpn). Embedded OAuth
accepts only the canonical public resource Host, which is also added to MCP's
Host allowlist; additional host aliases do not broaden that outer guard.
Local OAuth, external OAuth (`--oauth-config`), and static Bearer are mutually
exclusive. HTTP authentication options are rejected in stdio mode; an owner
secret flag without a local resource is rejected too.

Forward **all** these paths through the HTTPS proxy, preserving Host,
Authorization, Origin, and query strings:

- `/mcp`
- `/.well-known/oauth-authorization-server`
- `/.well-known/oauth-protected-resource` and its `/mcp` variant
- `/oauth/register`, `/oauth/authorize`, `/oauth/token`

In ChatGPT, add a custom MCP server with `https://mcp.example.com/mcp` and select
**OAuth**. Use dynamic client registration (DCR); do not enter the owner secret
as a client ID/secret. During sign-in, verify the public domain, the unverified
client name, and its exact callback URL. Enter the generated owner secret on
qmcp's **Owner login + authorize** page and explicitly choose **Log in and
Approve**, or choose **Deny**. Registration alone grants no access. Workspace
settings must allow custom MCP servers. The flow is covered by local tests;
live ChatGPT interoperability still needs deployment validation.

### Security and operational limits

- Only public DCR clients, authorization-code grants, and PKCE `S256` are
  supported. No implicit grant, refresh tokens, CIMD URL fetching, or client
  secret authentication. Registration never fetches a supplied URL.
- The exact registered redirect, public resource, client, and `qmcp:access`
  scope are bound to authorization and code exchange. HTTPS callbacks are
  required except literal loopback/localhost HTTP callbacks. Redirects with
  fragments, credentials, or reserved OAuth response query fields are rejected.
  Unknown registration metadata is rejected; up to eight redirects and a
  128-byte client name are accepted within an 8 KiB request body.
- Each five-minute browser transaction requires a random CSRF token and a
  Secure, HttpOnly, SameSite=Lax, path-scoped cookie. Form POSTs require the
  exact issuer Origin. MCP and all other routes reject Origin; never strip it
  at the proxy to bypass validation. Each submitted transaction has one owner
  login attempt. There is no remembered login or silent approval.
- Codes expire after 90 seconds and are consumed atomically, including on a
  failed binding/PKCE check. Access tokens expire after 30 minutes. Only
  SHA-256 digests of owner credentials, cookies, CSRF tokens, codes and access
  tokens are retained in server state. Responses use no-store and restrictive
  CSP, framing and referrer policies.
- All state is **volatile**. Restart invalidates grants, tokens, transactions,
  and DCR client IDs; ChatGPT may require removing and re-adding the server,
  not just signing in again. Client registrations also expire after one hour;
  their expiration may require re-registration/re-adding. There is no database,
  registration management API, per-client revocation, or persistence.
- Capacity is 128 clients, 128 browser transactions, 128 codes and 256 tokens.
  Expired entries are pruned on use. Global fixed-memory limits per minute are
  16 registrations, 60 authorizations, 8 login POSTs and 120 token requests.
  AS bodies have an 8 KiB limit, 16 concurrent readers and a ten-second read
  timeout. Limits deliberately do not trust client-IP/proxy headers. An attacker
  can temporarily exhaust capacity/rate windows; put deployment-specific
  connection/rate limits at the trusted proxy. No automatic eviction of live
  grants is attempted.
- This is a **single-owner backend**, not multi-user isolation. Approval grants
  all tools on the connected QueryMT instance. `--permission cancel` controls
  agent tool approval, not OAuth scope. Protect the owner secret accordingly.
  For rotation or global revocation, replace the protected file and perform a
  reviewed restart. Keep TLS termination, proxy logs (no query/body/header or
  token-response logging for OAuth), firewall and the private upstream secure.

## External OAuth Provider (Optional)

For an existing identity provider, use an external [OAuth provider](https://developers.openai.com/plugins/build/auth)
with authorization-code + PKCE (`S256`) and ChatGPT client registration
(pre-registered, CIMD, or DCR). qmcp provides discovery and token introspection;
the provider handles login, consent, token issuance, and revocation.

Create `oauth.json` with your actual provider values:

```json
{
  "resource": "https://mcp.example.com/mcp",
  "issuer": "https://auth.example.com/",
  "introspection_url": "https://auth.example.com/introspect",
  "client_id": "qmcp-resource-server",
  "scope": "qmcp:access"
}
```

Supply `QMCP_OAUTH_CLIENT_SECRET` through service secret settings, leave
`QMCP_BEARER_TOKEN` unset, and run behind your HTTPS proxy:

```sh
qmcp --transport http --oauth-config oauth.json --allowed-host mcp.example.com
```

The provider must publish OAuth discovery metadata, honor the `resource` parameter,
and return `active`, `iss`, `aud`, `exp`, `scope`, and `token_type: Bearer` from
RFC 7662 introspection authenticated with `client_secret_post`. `aud` must include
`resource` exactly; `iss` must match `issuer`. If `nbf` is present it is enforced.
The introspection credentials belong to qmcp, not ChatGPT's OAuth client.
Forward `/mcp` and both `/.well-known/oauth-protected-resource` and
`/.well-known/oauth-protected-resource/mcp` through the HTTPS proxy.

In ChatGPT, add a custom MCP server with your public `/mcp` URL and select **OAuth**.
Configure the provider's ChatGPT client and exact callback URL shown by ChatGPT,
then create/install the plugin and sign in. Select it with `@` and ask:
**"Use qmcp to list QueryMT profiles."** Workspace permissions apply.

The default `qmcp:access` scope grants all tools on this backend, not isolated
per-user workspaces. Tokens are checked on every request without caching;
provider outages deny access. Live ChatGPT sign-in depends on your provider
configuration and has not been verified by the project's mock-provider tests.

## Separate Nginx server to a Mac over VPN

Topology: ChatGPT -> Nginx (public HTTPS) -> Mac (private/VPN HTTP) ->
local QueryMT.
This is a deployment template, not a claim of a verified live connection.
Use embedded OAuth on the Mac; an external provider is optional. TLS and
proxying alone do not enable ChatGPT authentication; a static bearer is not a
substitute. If you choose external OAuth instead, an arbitrary OIDC provider
may lack the introspection endpoint or claims described in the optional section.

Replace every `mcp.example.com` with your public domain and every `10.0.0.42`
with the Mac's actual private/VPN IP; both are placeholders. Point public DNS to
Nginx, not the Mac, and use a valid TLS certificate for that domain on Nginx.

On the Mac, keep QueryMT on `ws://127.0.0.1:3030/acp/ws` as described above.
Generate the protected owner secret as described above. Leave
`QMCP_BEARER_TOKEN` and `QMCP_OAUTH_CLIENT_SECRET` unset. After reviewing the
configuration, run on the Mac (all addresses and paths here are examples):

```sh
qmcp --transport http --bind 10.0.0.42:3031 \
  --allowed-host mcp.example.com --permission cancel \
  --oauth-local-resource https://mcp.example.com/mcp \
  --oauth-owner-secret-file /absolute/private/path/to/owner-secret
```

`--bind` takes an IP:port socket address, not a URL or hostname. This binds only
the Mac's VPN/private interface; OAuth is required for this non-loopback bind.
Use the installed qmcp binary's absolute path if it is not on `PATH`.

On the separate Nginx host, adapt this HTTPS server template; certificate paths
are placeholders too. Keep the upstream without a URI suffix so MCP, discovery,
and all embedded authorization-server endpoints and queries reach qmcp unchanged:

```nginx
server {
    listen 443 ssl;
    server_name mcp.example.com;
    ssl_certificate /path/to/fullchain.pem;
    ssl_certificate_key /path/to/privkey.pem;

    location / {
        proxy_pass http://10.0.0.42:3031;
        proxy_http_version 1.1;
        proxy_set_header Host $http_host;
        proxy_set_header Authorization $http_authorization;
        proxy_set_header Origin $http_origin;
        access_log off; # OAuth queries contain authorization codes/state.
        proxy_set_header Connection "";
        proxy_buffering off;
        proxy_request_buffering off;
        proxy_cache off;
        proxy_read_timeout 3600s;
        proxy_send_timeout 3600s;
    }

}
```

The Host must match `--allowed-host`; do not replace it with the upstream IP.
Forward Authorization unchanged; allow token-free discovery and preserve
qmcp's `WWW-Authenticate` responses. Do not strip Origin to bypass qmcp's
checks.
Buffering/cache are disabled for Streamable HTTP; the example timeouts are
inactivity limits, not a guarantee of task duration. Align any other proxy or
load-balancer timeouts with long-lived requests.

Allow inbound TCP 3031 on the Mac only from the Nginx host's VPN source IP,
over the VPN interface; deny other sources. Do not publish/port-forward 3031 or
expose QueryMT's ACP port. Plain HTTP here assumes the proxy-to-Mac hop uses the
encrypted VPN, not an untrusted LAN. Verify Nginx can route to that Mac address;
keep the VPN connected, the Mac awake/reachable, and QueryMT and qmcp running.
Validate the adapted Nginx configuration with `nginx -t` before deploying it.
After deployment, the AS metadata and both resource-discovery paths should
return metadata without a token, and `/mcp` should reject missing/invalid tokens.
Only then follow the embedded OAuth ChatGPT setup above using
`https://mcp.example.com/mcp`, not the private IP. Deployment and real sign-in
verification are separate from source/build review; do not restart a live
service merely to run tests.

## Remote Access Checklist

- Use HTTPS: qmcp itself does not terminate TLS. Never send tokens over public plaintext HTTP.
- Keep qmcp's backend port private to the proxy with a firewall/private network.
- Forward Authorization, Host and Origin unchanged. MCP rejects browser Origin;
  only embedded OAuth consent POSTs accept the exact public issuer Origin.
- Non-loopback binds require Bearer or OAuth auth. Wildcard binds require an
  explicit Host allowlist (`--allowed-host`, repeatable, optionally `host:port`);
  embedded OAuth supplies its canonical resource Host automatically.
- Use `--bind` only when the proxy needs another interface. Keep the default
  loopback listener when the proxy runs on the same host.
- `--permission` controls QueryMT tool approval, not MCP client authentication.
  Credentials authorize powerful agent tools; share them only with trusted users.

Run `qmcp --help` for all options. Set `RUST_LOG=info` for connection diagnostics.
