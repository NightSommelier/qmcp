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

#[derive(Clone, Copy, Default)]
enum SubmissionBehavior {
    #[default]
    Idle,
    PollError,
    Busy,
    NoReply,
}

impl MockAcp {
    async fn start() -> Self {
        Self::with_submission(SubmissionBehavior::Idle).await
    }

    async fn with_submission(behavior: SubmissionBehavior) -> Self {
        Self::with_handshake_delay(behavior, Duration::ZERO).await
    }

    async fn with_handshake_delay(behavior: SubmissionBehavior, delay: Duration) -> Self {
        Self::with_options(behavior, delay, None).await
    }

    async fn with_listing_pages(pages: usize) -> Self {
        Self::with_options(SubmissionBehavior::Idle, Duration::ZERO, Some(pages)).await
    }

    async fn with_options(
        behavior: SubmissionBehavior,
        delay: Duration,
        listing_pages: Option<usize>,
    ) -> Self {
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
                            tokio::time::sleep(delay).await;
                            let socket = accept_async(stream).await.unwrap();
                            serve_acp(socket, recorded, behavior, listing_pages).await;
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

async fn serve_acp(
    mut socket: WebSocketStream<TcpStream>,
    requests: Arc<Mutex<Vec<Value>>>,
    behavior: SubmissionBehavior,
    listing_pages: Option<usize>,
) {
    let mut submitted = false;
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
                if let Some(total) = listing_pages {
                    let page: usize = params["cursor"]
                        .as_str()
                        .map(|cursor| cursor.strip_prefix("page-").unwrap().parse().unwrap())
                        .unwrap_or(1);
                    assert!(
                        page <= total,
                        "the fixture must not be scanned beyond its final page"
                    );
                    let mut result = json!({"sessions": [
                        {"sessionId": format!("listed-{page}"), "cwd": "/workspace", "title": format!("Listed {page}")},
                        {"sessionId": format!("extra-{page}"), "cwd": "/workspace", "title": format!("Listed extra {page}")},
                    ]});
                    if page < total {
                        result["nextCursor"] = json!(format!("page-{}", page + 1));
                    }
                    result
                } else if params["cursor"] == "page-2" {
                    json!({"sessions": [{"sessionId": "second-session", "cwd": "/workspace", "title": "Second"}]})
                } else {
                    json!({"sessions": [{"sessionId": "fixture-session", "cwd": "/workspace", "title": "Fixture"}], "nextCursor": "page-2"})
                }
            }
            "_querymt/session/runtimeState" => {
                if submitted && matches!(behavior, SubmissionBehavior::PollError) {
                    send(&mut socket, json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32000, "message": "poll failed after submission"}})).await;
                    continue;
                }
                if matches!(behavior, SubmissionBehavior::Idle) {
                    json!({"phase": "idle", "steerable": false})
                } else {
                    json!({"phase": "tools", "steerable": true, "active_run_id": "fixture-run"})
                }
            }
            "_querymt/session/getLifecycle" => json!({"resolved": false, "archived": false}),
            "_querymt/session/setLifecycle" | "_querymt/session/setPinned" => params.clone(),
            "_querymt/session/queue" | "_querymt/session/steer" => {
                submitted = true;
                if matches!(behavior, SubmissionBehavior::NoReply) {
                    continue;
                }
                json!({"accepted": true})
            }
            "_querymt/session/discardQueuedInput" => json!({"accepted": true}),
            "session/set_mode" if params["modeId"] == "flood-config" => {
                for _ in 0..300 {
                    send(&mut socket, json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "fixture-session", "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "event"}}}})).await;
                }
                send(&mut socket, json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32000, "message": "configuration rejected after event flood"}})).await;
                continue;
            }
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
                let reply = if text == "oversized" {
                    "x".repeat(1024 * 1024 + 32)
                } else {
                    format!("reply:{text}")
                };
                send(&mut socket, json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "fixture-session", "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": reply}}}})).await;
                json!({"stopReason": if text == "cancelled" { "cancelled" } else { "end_turn" }})
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
async fn created_session_is_recoverable_after_configuration_failure_and_event_eviction() {
    for http in [false, true] {
        let backend = MockAcp::start().await;
        let mut mcp = Mcp::start(&backend.url, http).await;
        let cwd = std::env::current_dir().unwrap();
        let response = mcp
            .tool("new", json!({"cwd": cwd, "mode": "flood-config"}))
            .await;
        assert_eq!(response["isError"], true);
        let result = &response["structuredContent"];
        assert_eq!(result["code"], 3);
        assert_eq!(result["recovery"]["sessionId"], "fixture-session");
        assert_eq!(result["recovery"]["cwd"], json!(cwd));
        assert_eq!(result["recovery"]["sessionCreated"], true);
        assert!(result["droppedEvents"].as_u64().unwrap() > 0);
        assert_eq!(
            backend
                .recorded()
                .iter()
                .filter(|r| r["method"] == "session/new")
                .count(),
            1
        );
        backend.wait_disconnected().await;
        assert_no_implicit_cancel(&backend);
    }
}

fn assert_no_implicit_cancel(backend: &MockAcp) {
    assert!(
        !backend
            .recorded()
            .iter()
            .any(|r| r["method"] == "session/cancel" || r["method"] == "session/close")
    );
}

#[tokio::test]
async fn submission_identity_survives_poll_failures_and_timeouts_without_input_events() {
    for http in [false, true] {
        for behavior in [
            SubmissionBehavior::PollError,
            SubmissionBehavior::Busy,
            SubmissionBehavior::NoReply,
        ] {
            for delivery in ["queue", "steer"] {
                let backend = MockAcp::with_submission(behavior).await;
                let mut mcp = Mcp::start(&backend.url, http).await;
                let response = mcp.tool("prompt", json!({"session_id": "fixture-session", "text": "next", "delivery": delivery, "timeout": 1})).await;
                assert_eq!(response["isError"], true);
                let result = &response["structuredContent"];
                let expected_code = if matches!(behavior, SubmissionBehavior::PollError) {
                    3
                } else {
                    4
                };
                assert_eq!(result["code"], expected_code);
                let recovery = &result["recovery"];
                assert_eq!(recovery["sessionId"], "fixture-session");
                let submission = &recovery["submission"];
                assert_eq!(submission["delivery"], delivery);
                let requests = backend.recorded();
                let submitted: Vec<_> = requests
                    .iter()
                    .filter(|r| r["method"] == format!("_querymt/session/{delivery}"))
                    .collect();
                assert_eq!(submitted.len(), 1, "no duplicate submission");
                assert_eq!(
                    submission["clientInputId"],
                    submitted[0]["params"]["client_input_id"]
                );
                if matches!(behavior, SubmissionBehavior::NoReply) {
                    assert_eq!(submission["state"], "unconfirmed");
                    assert!(submission.get("acknowledgement").is_none());
                } else {
                    assert_eq!(submission["state"], "acknowledged");
                    assert_eq!(submission["acknowledgement"]["accepted"], true);
                }
                backend.wait_disconnected().await;
                assert_no_implicit_cancel(&backend);
            }
        }
    }
}

#[tokio::test]
async fn batch_keeps_each_steps_truncation_status_after_a_later_short_answer() {
    for http in [false, true] {
        let backend = MockAcp::start().await;
        let mut mcp = Mcp::start(&backend.url, http).await;
        let response = mcp
            .tool(
                "exec",
                json!({"calls": [
                    {"name": "prompt", "arguments": {"new": true, "text": "oversized"}},
                    {"name": "prompt", "arguments": {"new": true, "text": "short"}},
                ]}),
            )
            .await;
        assert_eq!(response["isError"], false);
        let result = &response["structuredContent"];
        assert_eq!(result["textTruncated"], true);
        let first = &result["results"][0]["result"];
        let second = &result["results"][1]["result"];
        assert_eq!(first["ok"], true);
        assert_eq!(first["textTruncated"], true);
        assert_eq!(first["text"].as_str().unwrap().len(), 1024 * 1024);
        assert_eq!(second["ok"], true);
        assert_eq!(second["textTruncated"], false);
        assert_eq!(second["text"], "reply:short");
    }
}

#[tokio::test]
async fn cancelled_batch_step_is_an_error_and_prevents_later_mutations() {
    for http in [false, true] {
        let backend = MockAcp::start().await;
        let mut mcp = Mcp::start(&backend.url, http).await;
        let response = mcp
            .tool(
                "exec",
                json!({"calls": [
                    {"name": "prompt", "arguments": {"new": true, "text": "cancelled"}},
                    {"name": "close", "arguments": {"session_id": "fixture-session"}},
                ]}),
            )
            .await;
        assert_eq!(response["isError"], true);
        let result = &response["structuredContent"];
        assert_eq!(result["ok"], false);
        assert_eq!(result["stopReason"], "cancelled");
        assert_eq!(result["results"].as_array().unwrap().len(), 1);
        assert_eq!(result["results"][0]["result"]["ok"], false);
        assert_eq!(result["results"][0]["result"]["stopReason"], "cancelled");
        assert_no_implicit_cancel(&backend);
    }
}

#[tokio::test]
async fn batch_failure_keeps_prior_results_and_failed_step_recovery() {
    for http in [false, true] {
        let backend = MockAcp::start().await;
        let mut mcp = Mcp::start(&backend.url, http).await;
        let response = mcp
            .tool(
                "exec",
                json!({"calls": [
                    {"name": "runtime", "arguments": {"session_id": "fixture-session"}},
                    {"name": "new", "arguments": {"mode": "invalid"}},
                    {"name": "close", "arguments": {"session_id": "fixture-session"}},
                ]}),
            )
            .await;
        assert_eq!(response["isError"], true);
        let result = &response["structuredContent"];
        assert_eq!(result["results"].as_array().unwrap().len(), 1);
        assert_eq!(result["failedCall"], "new");
        assert_eq!(result["failedResult"]["ok"], false);
        assert_eq!(
            result["failedResult"]["recovery"]["sessionId"],
            "fixture-session"
        );
        assert_eq!(result["recovery"]["sessionCreated"], true);
        assert_no_implicit_cancel(&backend);
    }
}

#[tokio::test]
async fn operation_deadlines_cover_a_stalled_acp_handshake_on_both_transports() {
    use tokio::io::AsyncReadExt;
    for http in [false, true] {
        for (name, arguments) in [
            (
                "prompt",
                json!({"new": true, "text": "never sent", "timeout": 1}),
            ),
            ("follow", json!({"session_id": "s", "timeout": 1})),
            ("watch", json!({"session_id": "s", "timeout": 1})),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("ws://{}/acp/ws", listener.local_addr().unwrap());
            let peer = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                socket.read_to_end(&mut bytes).await.unwrap();
                assert!(
                    bytes.starts_with(b"GET "),
                    "the ACP handshake must have started"
                );
            });
            let mut mcp = Mcp::start(&url, http).await;
            let started = tokio::time::Instant::now();
            let response = tokio::time::timeout(Duration::from_secs(3), mcp.tool(name, arguments))
                .await
                .expect("a 1s command must not wait for the 10s connect timeout");
            assert_eq!(response["isError"], true);
            assert_eq!(response["structuredContent"]["code"], 4);
            assert!(started.elapsed() < Duration::from_secs(3));
            tokio::time::timeout(Duration::from_secs(1), peer)
                .await
                .expect("the pending ACP socket must be released")
                .unwrap();
        }
    }
}

#[tokio::test]
async fn connection_time_is_not_added_to_the_command_budget() {
    for http in [false, true] {
        let backend =
            MockAcp::with_handshake_delay(SubmissionBehavior::Idle, Duration::from_millis(800))
                .await;
        let mut mcp = Mcp::start(&backend.url, http).await;
        let response = tokio::time::timeout(
            Duration::from_millis(1600),
            mcp.tool("prompt", json!({"new": true, "text": "wait", "timeout": 1})),
        )
        .await
        .expect("the command deadline must use the budget left after the handshake");
        assert_eq!(response["isError"], true);
        assert_eq!(response["structuredContent"]["code"], 4);
        assert_eq!(
            response["structuredContent"]["recovery"]["sessionId"],
            "fixture-session"
        );
        backend.wait_disconnected().await;
        assert_no_implicit_cancel(&backend);
    }
}

#[tokio::test]
async fn find_discloses_page_cap_without_claiming_an_exhaustive_search() {
    for http in [false, true] {
        for total in [19, 20, 21] {
            let backend = MockAcp::with_listing_pages(total).await;
            let mut mcp = Mcp::start(&backend.url, http).await;
            let response = mcp.tool("find", json!({"query": "not-present", "limit": 50, "resolution": "open", "pinned": false})).await;
            assert_eq!(response["isError"], false);
            let result = &response["structuredContent"];
            assert_eq!(result["sessions"], json!([]));
            assert_eq!(result["scannedPages"], total.min(20));
            assert_eq!(result["scanLimitReached"], total > 20);
            assert_eq!(result["truncated"], total > 20);
            assert_eq!(
                result["nextCursor"],
                if total > 20 {
                    json!("page-21")
                } else {
                    Value::Null
                }
            );
            let requests = backend.recorded();
            let pages: Vec<_> = requests
                .iter()
                .filter(|r| r["method"] == "session/list")
                .collect();
            assert_eq!(pages.len(), total.min(20));
            for page in pages {
                assert_eq!(
                    page["params"]["_meta"]["querymt"]["sessionFilters"]["resolution"],
                    "open"
                );
                assert_eq!(
                    page["params"]["_meta"]["querymt"]["sessionFilters"]["pinned"],
                    false
                );
            }
            if total > 20 {
                let rest = mcp.tool("sessions", json!({"cursor": result["nextCursor"], "resolution": "open", "pinned": false})).await;
                assert_eq!(rest["isError"], false);
                assert_eq!(
                    rest["structuredContent"]["sessions"][0]["sessionId"],
                    "listed-21"
                );
            }
        }
    }
}

#[tokio::test]
async fn find_distinguishes_match_limit_from_scan_limit() {
    for http in [false, true] {
        let backend = MockAcp::with_listing_pages(21).await;
        let mut mcp = Mcp::start(&backend.url, http).await;
        let response = mcp
            .tool("find", json!({"query": "Listed", "limit": 1}))
            .await;
        let result = &response["structuredContent"];
        assert_eq!(result["sessions"].as_array().unwrap().len(), 1);
        assert_eq!(result["scannedPages"], 1);
        assert_eq!(result["truncated"], true);
        assert_eq!(result["scanLimitReached"], false);
        assert_eq!(
            result["nextCursor"],
            Value::Null,
            "a mid-page match limit must not skip unread candidates"
        );
        let response = mcp
            .tool("find", json!({"query": "Listed", "limit": 2}))
            .await;
        let result = &response["structuredContent"];
        assert_eq!(result["truncated"], true);
        assert_eq!(result["scanLimitReached"], false);
        assert_eq!(result["nextCursor"], "page-2");

        let backend = MockAcp::with_listing_pages(1).await;
        let mut mcp = Mcp::start(&backend.url, http).await;
        let response = mcp
            .tool("find", json!({"query": "Listed", "limit": 2}))
            .await;
        let result = &response["structuredContent"];
        assert_eq!(result["truncated"], false);
        assert_eq!(result["scanLimitReached"], false);
        assert_eq!(result["nextCursor"], Value::Null);
    }
}

#[tokio::test]
async fn stateless_http_cancellation_notification_does_not_cancel_another_post() {
    let backend = MockAcp::with_submission(SubmissionBehavior::Busy).await;
    let mut mcp = Mcp::start(&backend.url, true).await;
    let Mcp::Http { client, url, .. } = &mcp else {
        unreachable!()
    };
    let client = client.clone();
    let url = url.clone();
    let waiting =
        tokio::spawn(async move {
            client.post(url)
            .header("accept", "application/json, text/event-stream")
            .header("mcp-protocol-version", "2025-11-25")
            .json(&json!({"jsonrpc": "2.0", "id": 73, "method": "tools/call", "params": {
                "name": "watch", "arguments": {"session_id": "fixture-session", "timeout": 2},
            }}))
            .send().await.unwrap().json::<Value>().await.unwrap()
        });
    tokio::time::timeout(Duration::from_secs(1), async {
        while !backend
            .recorded()
            .iter()
            .any(|r| r["method"] == "_querymt/session/runtimeState")
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("watch must be running before its cancellation notification is posted");
    mcp.message(
        json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {
            "requestId": 73, "reason": "verify stateless HTTP semantics",
        }}),
    )
    .await;
    assert!(
        !waiting.is_finished(),
        "the notification-only POST must not stop the separate handler"
    );
    let response = tokio::time::timeout(Duration::from_secs(3), waiting)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(response["result"]["isError"], true);
    let result = &response["result"]["structuredContent"];
    assert_eq!(result["code"], 4);
    assert!(
        result["error"].as_str().unwrap().contains("timed out"),
        "the original POST must reach its own deadline, not be cancelled"
    );
    assert!(
        backend
            .recorded()
            .iter()
            .filter(|r| r["method"] == "_querymt/session/runtimeState")
            .count()
            >= 2
    );
    backend.wait_disconnected().await;
    assert_no_implicit_cancel(&backend);
}

// Linux routes the entire 127/8 range to loopback without configuring aliases.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn cli_http_accepts_the_selected_alternate_loopback_host_only() {
    let listener = TcpListener::bind("127.0.0.2:0").await.unwrap();
    let bind = listener.local_addr().unwrap();
    drop(listener);
    let backend = MockAcp::start().await;
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_qmcp"))
        .args([
            "--transport",
            "http",
            "--bind",
            &bind.to_string(),
            "--url",
            &backend.url,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(1))
        .build()
        .unwrap();
    let url = format!("http://{bind}/mcp");
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if client.get(&url).send().await.is_ok() {
                break;
            }
            assert!(
                child.try_wait().unwrap().is_none(),
                "HTTP server exited before listening"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("test HTTP listener must start");
    let request = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
        "protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "bind-fixture", "version": "1"},
    }});
    let response = client
        .post(&url)
        .header("accept", "application/json, text/event-stream")
        .json(&request)
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        200,
        "an explicitly accepted loopback bind must accept its normal Host header"
    );
    assert_eq!(
        response.json::<Value>().await.unwrap()["result"]["serverInfo"]["name"],
        "qmcp"
    );
    for (header, value) in [
        ("host", "untrusted.invalid"),
        ("host", "127.0.0.3"),
        ("host", "0.0.0.0"),
        ("origin", "https://untrusted.invalid"),
        ("origin", "http://127.0.0.2"),
    ] {
        let response = client
            .post(&url)
            .header("accept", "application/json, text/event-stream")
            .header(header, value)
            .json(&request)
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            403,
            "must still reject {header}: {value}"
        );
    }
    assert!(
        backend.recorded().is_empty(),
        "protocol discovery must not connect to ACP"
    );
    child.start_kill().unwrap();
    child.wait().await.unwrap();
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
