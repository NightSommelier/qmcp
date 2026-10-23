use std::{
    path::PathBuf,
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use futures_util::{SinkExt, StreamExt};
use qmacp::protocol::{JsonRpcMessage, schema::v1 as acp};
use qmcp::{BridgeConfig, QmcpServer, policy::PermissionPolicy, transport};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines},
    net::{TcpListener, TcpStream},
    process::{Child, ChildStdin, ChildStdout},
    task::{JoinHandle, JoinSet},
};
use tokio_tungstenite::{WebSocketStream, accept_async, tungstenite::Message};

struct MockAcp {
    url: String,
    requests: Arc<Mutex<Vec<Value>>>,
    active: Arc<AtomicUsize>,
    task: JoinHandle<()>,
}

impl MockAcp {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/acp/ws", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let active = Arc::new(AtomicUsize::new(0));
        let recorded = requests.clone();
        let connections = active.clone();
        let task = tokio::spawn(async move {
            let mut tasks = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let (stream, _) = accepted.unwrap();
                        let recorded = recorded.clone();
                        let active = connections.clone();
                        tasks.spawn(async move {
                            active.fetch_add(1, Ordering::SeqCst);
                            let _guard = ConnectionGuard(active);
                            let socket = accept_async(stream).await.unwrap();
                            serve_acp(socket, recorded).await;
                        });
                    }
                    Some(result) = tasks.join_next(), if !tasks.is_empty() => { result.unwrap(); }
                }
            }
        });
        Self {
            url,
            requests,
            active,
            task,
        }
    }

    fn recorded(&self) -> Vec<Value> {
        self.requests.lock().unwrap().clone()
    }

    async fn wait_disconnected(&self) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while self.active.load(Ordering::SeqCst) != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("ACP connections must be released after a tool call");
    }
}

impl Drop for MockAcp {
    fn drop(&mut self) {
        self.task.abort();
    }
}
struct ConnectionGuard(Arc<AtomicUsize>);
impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

async fn send(socket: &mut WebSocketStream<TcpStream>, value: Value) {
    socket
        .send(Message::Text(value.to_string().into()))
        .await
        .unwrap();
}

async fn serve_acp(mut socket: WebSocketStream<TcpStream>, requests: Arc<Mutex<Vec<Value>>>) {
    while let Some(Ok(message)) = socket.next().await {
        let Message::Text(text) = message else {
            continue;
        };
        let request: Value = serde_json::from_str(&text).unwrap();
        requests.lock().unwrap().push(request.clone());
        let Some(method) = request["method"].as_str() else {
            continue;
        };
        let id = request["id"].clone();
        if id.is_null() {
            continue;
        }
        let params = &request["params"];
        let result = match method {
            "initialize" => {
                json!({"protocolVersion": 1, "agentInfo": {"name": "fixture", "version": "1"}, "agentCapabilities": {"loadSession": true, "sessionCapabilities": {"list": {}, "resume": {}, "close": {}}}})
            }
            "_querymt/capabilities" => {
                json!({"features": {"session_lifecycle": true, "session_pins": true}, "methods": ["querymt/session/getLifecycle", "querymt/session/setLifecycle", "querymt/session/setPinned"]})
            }
            "_querymt/profiles" => json!({"profiles": [{"id": "fixture-profile"}]}),
            "session/new" => json!({"sessionId": "fixture-session"}),
            "session/resume" | "session/load" => {
                json!({"configOptions": [{"id": "model", "name": "Model", "type": "select", "currentValue": "fixture-model", "options": [{"value": "fixture-model", "name": "Fixture"}]}]})
            }
            "session/list" => {
                if params["cursor"] == "page-2" {
                    json!({"sessions": [{"sessionId": "second-session", "cwd": "/workspace", "title": "Second"}]})
                } else {
                    json!({"sessions": [{"sessionId": "fixture-session", "cwd": "/workspace", "title": "Fixture"}], "nextCursor": "page-2"})
                }
            }
            "_querymt/session/runtimeState" => json!({"phase": "idle", "steerable": false}),
            "_querymt/session/getLifecycle" => json!({"resolved": false, "archived": false}),
            "_querymt/session/setLifecycle" | "_querymt/session/setPinned" => params.clone(),
            "_querymt/session/queue"
            | "_querymt/session/steer"
            | "_querymt/session/discardQueuedInput" => json!({"accepted": true}),
            "session/prompt" => {
                let text = params["prompt"][0]["text"].as_str().unwrap();
                if text == "wait" {
                    continue;
                }
                let permission = acp::RequestPermissionRequest::new(
                    "fixture-session",
                    acp::ToolCallUpdate::new("tool-1", acp::ToolCallUpdateFields::new()),
                    vec![acp::PermissionOption::new(
                        "allow",
                        "Allow once",
                        acp::PermissionOptionKind::AllowOnce,
                    )],
                )
                .to_untyped_message()
                .unwrap();
                send(&mut socket, json!({"jsonrpc": "2.0", "id": "permission-1", "method": permission.method, "params": permission.params})).await;
                let reply = socket.next().await.unwrap().unwrap().into_text().unwrap();
                let reply: Value = serde_json::from_str(&reply).unwrap();
                assert_eq!(reply["result"]["outcome"]["optionId"], "allow");
                requests.lock().unwrap().push(reply);
                send(&mut socket, json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "fixture-session", "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": format!("reply:{text}")}}}})).await;
                json!({"stopReason": "end_turn"})
            }
            _ => {
                send(&mut socket, json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32000, "message": "fixture RPC failure"}})).await;
                continue;
            }
        };
        send(
            &mut socket,
            json!({"jsonrpc": "2.0", "id": id, "result": result}),
        )
        .await;
    }
}

enum Mcp {
    Stdio {
        child: Box<Child>,
        stdin: ChildStdin,
        stdout: Lines<BufReader<ChildStdout>>,
    },
    Http {
        client: reqwest::Client,
        url: String,
        task: JoinHandle<()>,
    },
}

impl Mcp {
    async fn start(backend: &str, http: bool) -> Self {
        let mut mcp = if http {
            let server = QmcpServer::new(
                BridgeConfig::new(backend, false, PermissionPolicy::AllowOnce).unwrap(),
            )
            .unwrap();
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}/mcp", listener.local_addr().unwrap());
            let task = tokio::spawn(async move {
                axum::serve(listener, transport::http_router(server))
                    .await
                    .unwrap();
            });
            Self::Http {
                client: reqwest::Client::new(),
                url,
                task,
            }
        } else {
            let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_qmcp"))
                .args(["--url", backend])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            let stdin = child.stdin.take().unwrap();
            let stdout = BufReader::new(child.stdout.take().unwrap()).lines();
            Self::Stdio {
                child: Box::new(child),
                stdin,
                stdout,
            }
        };
        let initialized = mcp.rpc("initialize", json!({"protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "fixture-client", "version": "1"}})).await;
        assert_eq!(initialized["serverInfo"]["name"], "qmcp");
        mcp.message(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .await;
        mcp
    }

    async fn message(&mut self, message: Value) -> Option<Value> {
        let expect_response = message.get("id").is_some();
        match self {
            Self::Stdio { stdin, stdout, .. } => {
                stdin
                    .write_all(format!("{message}\n").as_bytes())
                    .await
                    .unwrap();
                stdin.flush().await.unwrap();
                if !expect_response {
                    return None;
                }
                let line = tokio::time::timeout(Duration::from_secs(5), stdout.next_line())
                    .await
                    .unwrap()
                    .unwrap()
                    .expect("MCP stdout ended");
                Some(serde_json::from_str(&line).expect("stdout must contain only MCP JSON"))
            }
            Self::Http { client, url, .. } => {
                let response = client
                    .post(url.as_str())
                    .header("accept", "application/json, text/event-stream")
                    .header("mcp-protocol-version", "2025-11-25")
                    .json(&message)
                    .send()
                    .await
                    .unwrap();
                if !expect_response {
                    assert_eq!(response.status(), 202);
                    return None;
                }
                assert_eq!(response.status(), 200);
                assert_eq!(response.headers()["content-type"], "application/json");
                assert!(!response.headers().contains_key("mcp-session-id"));
                Some(response.json().await.unwrap())
            }
        }
    }

    async fn rpc(&mut self, method: &str, params: Value) -> Value {
        let response = self
            .message(json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}))
            .await
            .unwrap();
        assert!(response.get("error").is_none(), "{response}");
        response["result"].clone()
    }

    async fn tool(&mut self, name: &str, arguments: Value) -> Value {
        self.rpc("tools/call", json!({"name": name, "arguments": arguments}))
            .await
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        match self {
            Self::Stdio { child, .. } => {
                let _ = child.start_kill();
            }
            Self::Http { task, .. } => task.abort(),
        }
    }
}

async fn exercise_bridge(http: bool) {
    let backend = MockAcp::start().await;
    let mut mcp = Mcp::start(&backend.url, http).await;
    let catalog = mcp.rpc("tools/list", json!({})).await;
    assert_eq!(catalog["tools"].as_array().unwrap().len(), 25);
    assert!(
        backend.recorded().is_empty(),
        "discovery must not connect to ACP"
    );

    let caps = mcp.tool("caps", json!({})).await;
    assert_eq!(caps["structuredContent"]["agent"]["name"], "fixture");
    let cwd: PathBuf = std::env::current_dir().unwrap();
    let prompt = mcp
        .tool("prompt", json!({"new": true, "cwd": cwd, "text": "-"}))
        .await;
    assert_eq!(prompt["isError"], false, "{prompt}");
    assert_eq!(prompt["structuredContent"]["sessionId"], "fixture-session");
    assert_eq!(prompt["structuredContent"]["text"], "reply:-");
    assert!(
        prompt["structuredContent"]["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["type"] == "permission")
    );
    let continued = mcp
        .tool(
            "prompt",
            json!({"session_id": "fixture-session", "cwd": cwd, "text": "continue"}),
        )
        .await;
    assert_eq!(continued["structuredContent"]["text"], "reply:continue");

    let listed = mcp
        .tool(
            "sessions",
            json!({"all": true, "resolution": "open", "pinned": false}),
        )
        .await;
    assert_eq!(
        listed["structuredContent"]["sessions"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let pages: Vec<_> = backend
        .recorded()
        .into_iter()
        .filter(|r| r["method"] == "session/list")
        .collect();
    assert_eq!(pages.len(), 2);
    for page in pages {
        assert_eq!(
            page["params"]["_meta"]["querymt"]["sessionFilters"]["pinned"],
            false
        );
    }

    let patched = mcp
        .tool(
            "set_lifecycle",
            json!({"session_id": "fixture-session", "resolved": false}),
        )
        .await;
    assert_eq!(patched["isError"], false, "{patched}");
    let queued = mcp
        .tool(
            "queue",
            json!({"session_id": "fixture-session", "text": "next"}),
        )
        .await;
    assert!(
        queued["structuredContent"]["clientInputId"]
            .as_str()
            .is_some()
    );
    let cancelled = mcp
        .tool("cancel", json!({"session_id": "fixture-session"}))
        .await;
    assert_eq!(
        cancelled["structuredContent"]["cancellationRequested"],
        true
    );
    backend.wait_disconnected().await;
    assert!(
        backend
            .recorded()
            .iter()
            .any(|r| r["method"] == "session/cancel"),
        "cancel must be flushed before the socket is dropped"
    );
}

#[tokio::test]
async fn stdio_bridges_native_acp_operations() {
    exercise_bridge(false).await;
}

#[tokio::test]
async fn streamable_http_bridges_native_acp_operations_without_sse() {
    exercise_bridge(true).await;
}

#[tokio::test]
async fn failures_are_tool_errors_and_timeouts_do_not_cancel_remote_work() {
    let backend = MockAcp::start().await;
    let mut mcp = Mcp::start(&backend.url, true).await;
    let invalid = mcp
        .tool(
            "prompt",
            json!({"new": true, "text": "x", "session_id": "s"}),
        )
        .await;
    assert_eq!(invalid["isError"], true);
    assert!(backend.recorded().is_empty());

    let timeout = mcp
        .tool("prompt", json!({"new": true, "text": "wait", "timeout": 1}))
        .await;
    assert_eq!(timeout["isError"], true, "{timeout}");
    assert_eq!(timeout["structuredContent"]["code"], 4);
    assert!(
        timeout["structuredContent"]["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["sessionId"] == "fixture-session")
    );
    backend.wait_disconnected().await;
    assert!(
        !backend
            .recorded()
            .iter()
            .any(|r| r["method"] == "session/cancel" || r["method"] == "session/close")
    );

    let batch = mcp.tool("exec", json!({"calls": [{"name": "runtime", "arguments": {"session_id": "s"}}, {"name": "set_mode", "arguments": {"session_id": "s", "mode": "invalid"}}, {"name": "close", "arguments": {"session_id": "s"}}]})).await;
    assert_eq!(batch["isError"], true, "{batch}");
    assert_eq!(
        batch["structuredContent"]["results"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(
        !backend
            .recorded()
            .iter()
            .any(|r| r["method"] == "session/close")
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let offline = format!("ws://{}/acp/ws", listener.local_addr().unwrap());
    drop(listener);
    let mut offline_mcp = Mcp::start(&offline, true).await;
    let error = offline_mcp
        .tool("runtime", json!({"session_id": "s"}))
        .await;
    assert_eq!(error["isError"], true);
    assert_eq!(error["structuredContent"]["code"], 2);
}

#[tokio::test]
async fn long_prompt_does_not_block_concurrent_runtime_requests() {
    let backend = MockAcp::start().await;
    let mut mcp = Mcp::start(&backend.url, true).await;
    let Mcp::Http { client, url, .. } = &mcp else {
        unreachable!()
    };
    let client = client.clone();
    let url = url.clone();
    let waiting = tokio::spawn(async move {
        client.post(url).header("accept", "application/json, text/event-stream")
            .header("mcp-protocol-version", "2025-11-25")
            .json(&json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "prompt", "arguments": {"new": true, "text": "wait", "timeout": 1}}}))
            .send().await.unwrap().json::<Value>().await.unwrap()
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        while !backend
            .recorded()
            .iter()
            .any(|r| r["method"] == "session/prompt")
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let runtime = tokio::time::timeout(
        Duration::from_millis(500),
        mcp.tool("runtime", json!({"session_id": "fixture-session"})),
    )
    .await
    .unwrap();
    assert_eq!(runtime["isError"], false);
    let finished = waiting.await.unwrap();
    assert_eq!(finished["result"]["structuredContent"]["code"], 4);
    backend.wait_disconnected().await;
}

#[tokio::test]
async fn http_rejects_event_streams_and_untrusted_hosts_and_origins() {
    let backend = MockAcp::start().await;
    let mcp = Mcp::start(&backend.url, true).await;
    let Mcp::Http { client, url, .. } = &mcp else {
        unreachable!()
    };
    assert_eq!(
        client
            .get(url)
            .header("accept", "text/event-stream")
            .send()
            .await
            .unwrap()
            .status(),
        405
    );
    assert_eq!(
        client
            .get(url.replace("/mcp", "/sse"))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    let request = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {}});
    for (header, value) in [
        ("host", "untrusted.invalid"),
        ("origin", "https://untrusted.invalid"),
    ] {
        let response = client
            .post(url)
            .header("accept", "application/json, text/event-stream")
            .header(header, value)
            .json(&request)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 403);
    }
    assert!(backend.recorded().is_empty());
}
