# How To Connect

[Install qmcp](../README.md#install), then choose a setup:

| Setup | Best for | Authentication |
| --- | --- | --- |
| [Local stdio](#local-stdio) | Desktop and CLI MCP clients | Local process permissions |
| [Bearer over HTTPS](#bearer-over-https) | Clients with custom authorization headers | Shared token |
| [Private tunnel](#private-tunnel-chatgpt) | ChatGPT without a public endpoint | OpenAI tunnel access |
| [OAuth over HTTPS](#oauth-over-https-chatgpt) | Public authenticated ChatGPT connections | External OAuth provider |

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

## OAuth over HTTPS (ChatGPT)

Use an external [OAuth provider](https://developers.openai.com/plugins/build/auth)
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

### Separate Nginx server to a Mac over VPN

Topology: ChatGPT -> Nginx (public HTTPS) -> Mac (private/VPN HTTP) ->
local QueryMT.
This is a template, not a working ChatGPT connection yet: an external OAuth
provider is still required, and none is configured for this deployment. TLS and
proxying alone do not enable ChatGPT authentication; a static bearer is not a
substitute. An arbitrary OIDC provider may lack the introspection endpoint or
required claims described above.

Replace every `mcp.example.com` with your public domain and every `10.0.0.42`
with the Mac's actual private/VPN IP; both are placeholders. Point public DNS to
Nginx, not the Mac, and use a valid TLS certificate for that domain on Nginx.

On the Mac, keep QueryMT on `ws://127.0.0.1:3030/acp/ws` as described above.
Prepare `oauth.json` using the provider's actual settings and set `resource` to
`https://mcp.example.com/mcp` (with your domain). Inject the provider-issued
introspection secret as `QMCP_OAUTH_CLIENT_SECRET` through protected service
secret settings. Do not put it in JSON, command arguments, shell history, logs,
or version control; do not print the environment. Leave `QMCP_BEARER_TOKEN`
unset. Once the provider and secret are configured, run on the Mac:

```sh
qmcp --transport http --bind 10.0.0.42:3031 \
  --allowed-host mcp.example.com --oauth-config /absolute/path/to/oauth.json
```

`--bind` takes an IP:port socket address, not a URL or hostname. This binds only
the Mac's VPN/private interface; OAuth is required for this non-loopback bind.
Use the installed qmcp binary's absolute path if it is not on `PATH`.

On the separate Nginx host, adapt this HTTPS server template; certificate paths
are placeholders too. Keep the upstream without a URI suffix so all three paths
and their query strings reach qmcp unchanged:

```nginx
server {
    listen 443 ssl;
    server_name mcp.example.com;
    ssl_certificate /path/to/fullchain.pem;
    ssl_certificate_key /path/to/privkey.pem;

    location ~ ^/(mcp|\.well-known/oauth-protected-resource(/mcp)?)$ {
        proxy_pass http://10.0.0.42:3031;
        proxy_http_version 1.1;
        proxy_set_header Host $host;
        proxy_set_header Authorization $http_authorization;
        proxy_set_header Connection "";
        proxy_buffering off;
        proxy_request_buffering off;
        proxy_cache off;
        proxy_read_timeout 3600s;
        proxy_send_timeout 3600s;
    }

    location / { return 404; }
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
After deployment, both discovery paths should return metadata without a token,
and `/mcp` should reject missing/invalid tokens. Only then follow the OAuth
ChatGPT setup above using `https://mcp.example.com/mcp`, not the private IP.

## Remote Access Checklist

- Use HTTPS: qmcp itself does not terminate TLS. Never send tokens over public plaintext HTTP.
- Keep qmcp's backend port private to the proxy with a firewall/private network.
- Forward Authorization and an allowed Host. Browser Origin headers are rejected.
- Non-loopback binds require Bearer or OAuth auth. Wildcard binds also require
  explicit `--allowed-host` values (repeatable, optionally `host:port`).
- Use `--bind` only when the proxy needs another interface. Keep the default
  loopback listener when the proxy runs on the same host.
- `--permission` controls QueryMT tool approval, not MCP client authentication.
  Credentials authorize powerful agent tools; share them only with trusted users.

Run `qmcp --help` for all options. Set `RUST_LOG=info` for connection diagnostics.
