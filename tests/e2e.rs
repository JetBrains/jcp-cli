use agent_client_protocol::{
    self as acp, AgentNotification, AgentResponse, AgentSide, ClientRequest, ClientSide,
    ContentBlock, ContentChunk, InitializeRequest, InitializeResponse, JsonRpcMessage,
    NewSessionRequest, NewSessionResponse, Notification, PromptRequest, PromptResponse,
    ProtocolVersion, Request, RequestId, Response, SessionNotification, SessionUpdate, Side,
    StopReason, TextContent,
};
use jcp::{AgentOutgoingMessage, ClientOutgoingMessage, RawIncomingMessage};
use serde::de::DeserializeOwned;
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    path::PathBuf,
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    str::FromStr,
    thread::{self, JoinHandle},
};
use tempfile::tempdir;
use tungstenite::{Message, Utf8Bytes, WebSocket};
use url::Url;

#[test]
fn help() {
    let output = Command::new(get_jcp_binary_path())
        .arg("help")
        .output()
        .expect("Failed to run jcp help");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Usage:"), "Expected 'Usage:' in output");
}

#[test]
fn prompt_turn() {
    let mut e2e = E2eHarness::bootstrap(Default::default());

    // Step 1: Initialize handshake
    e2e.initialize_check().unwrap();

    // Step 2: Creating a new session
    let response = e2e.new_session_check().unwrap();

    // Step 3: prompt turn
    let prompt = ContentBlock::Text(TextContent::new("Prompt"));
    let prompt_request = PromptRequest::new(response.session_id, vec![prompt.clone()]);
    let (response, mut notifications) =
        e2e.client_request::<PromptResponse>(ClientRequest::PromptRequest(prompt_request));
    let response = response.unwrap();
    assert_eq!(response.stop_reason, StopReason::EndTurn);
    assert_eq!(
        notifications.len(),
        1,
        "Server should echo back with original prompt"
    );
    match notifications.pop().unwrap() {
        AgentNotification::SessionNotification(n) => match n.update {
            SessionUpdate::AgentMessageChunk(u) => assert_eq!(u.content, prompt),
            u => panic!("Unexpected update: {u:?}"),
        },
        n => panic!("Unexpected notification: {n:?}"),
    };
}

#[test]
fn run_outside_git_directory() {
    let tmp_dir = tempdir().unwrap();
    let mut e2e = E2eHarness::bootstrap(E2eConfig {
        // spawning in empty directory without git
        project_dir: Some(tmp_dir.path().to_path_buf()),
        suppress_stderr: true,
        ..Default::default()
    });

    match e2e.initialize_check() {
        Ok(r) => panic!("JSON RPC error is expected. Got: {r:?}"),
        Err(e) => {
            assert_eq!(e.code, acp::ErrorCode::InvalidParams);
            assert!(
                e.message
                    .contains("Program should be run in git working copy."),
                "Expect git error message, got: {}",
                e.message
            );
        }
    }
}

/// E2E test harness that manages mock server and jcp processes.
///
/// Starts an in-process mock ACP server on a background task
/// and spawns `jcp acp`, providing a typed API for sending client
/// requests and receiving responses.
///
/// Needs to be shut down using [`Self::shutdown()`].
struct E2eHarness {
    jcp: ChildProcess,
    next_request_id: i64,
    server_handle: Option<JoinHandle<()>>,
}

#[derive(Default)]
#[non_exhaustive]
struct E2eConfig {
    project_dir: Option<PathBuf>,
    /// If true, stderr of jcp binary will be sent to /dev/null
    /// Set it if test scenario expects to generate errors/warning is jcp binary
    suppress_stderr: bool,
}

impl E2eHarness {
    /// Start the mock server and jcp process, ready for testing.
    fn bootstrap(config: E2eConfig) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("Unable to bind socket");

        let addr = listener.local_addr().unwrap();
        let url = Url::from_str(&format!("ws://{addr}")).unwrap();

        let server_handle = thread::spawn(move || serve_acp_client(listener));

        let mut cmd = Command::new(get_jcp_binary_path());
        cmd.args(["acp"])
            .env("JCP_URL", url.as_str())
            .env("AI_PLATFORM_TOKEN", "test-token")
            .env("JBA_ACCESS_TOKEN", "test-access-token")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped());

        if let Some(project_dir) = config.project_dir {
            cmd.current_dir(project_dir);
        }
        if config.suppress_stderr {
            cmd.stderr(Stdio::null());
        }

        let mut child = cmd.spawn().expect("Failed to spawn child process");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());

        Self {
            jcp: ChildProcess {
                child,
                stdin,
                stdout,
            },
            next_request_id: 1,
            server_handle: Some(server_handle),
        }
    }

    /// Does initialization and basic checks
    #[track_caller]
    fn initialize_check(&mut self) -> Result<InitializeResponse, acp::Error> {
        let (response, _) = self.client_request::<InitializeResponse>(
            ClientRequest::InitializeRequest(InitializeRequest::new(ProtocolVersion::V1)),
        );
        let response = response?;
        assert_eq!(response.protocol_version, ProtocolVersion::V1);
        Ok(response)
    }

    fn new_session_check(&mut self) -> Result<NewSessionResponse, acp::Error> {
        let (response, _) = self.client_request::<NewSessionResponse>(
            ClientRequest::NewSessionRequest(NewSessionRequest::new("./")),
        );
        let response = response?;
        assert!(!response.session_id.0.is_empty());
        Ok(response)
    }

    /// Send a typed request and receive a typed response as well as all notifications that were sent by an agent
    /// while the request was executed.
    fn client_request<T: DeserializeOwned>(
        &mut self,
        request: ClientRequest,
    ) -> (Result<T, acp::Error>, Vec<AgentNotification>) {
        let request_id = self.next_request_id;
        self.next_request_id += 1;

        let msg = JsonRpcMessage::wrap(ClientOutgoingMessage::Request(Request {
            id: RequestId::Number(request_id),
            method: request.method().to_string().into(),
            params: Some(request),
        }));

        let json = serde_json::to_string(&msg).expect("Failed to serialize request");
        self.jcp.send_line(&json);

        let mut notifications: Vec<AgentNotification> = vec![];

        let response = loop {
            let line = self.jcp.read_line();
            let rpc_message: RawIncomingMessage =
                serde_json::from_str(&line).expect("Failed to parse response JSON");

            match (
                rpc_message.id,
                rpc_message.method,
                rpc_message.params,
                rpc_message.result,
                rpc_message.error,
            ) {
                // Response handling
                (Some(RequestId::Number(id)), None, None, Some(result), None) => {
                    assert_eq!(
                        request_id, id,
                        "Incoming response is expected to have id {id}, got {request_id} instead"
                    );
                    break Ok(serde_json::from_str(result.get())
                        .expect("Failed to deserialize response result"));
                }
                // Notifications handling
                (None, Some(method), params, None, None) => {
                    notifications
                        .push(ClientSide::decode_notification(method, params).expect("Unable"));
                }
                // Error handling
                (Some(RequestId::Number(id)), None, None, None, Some(error)) => {
                    assert_eq!(
                        request_id, id,
                        "Incoming response is expected to have id {id}, got {request_id} instead"
                    );
                    break Err(error);
                }
                _ => panic!("Unexpected payload: {line}"),
            }
        };
        (response, notifications)
    }
}

impl Drop for E2eHarness {
    fn drop(&mut self) {
        if let Some(server_join_handle) = self.server_handle.take() {
            self.jcp.kill();
            server_join_handle.join().ok();
        }
    }
}

/// Serves a mock WS/ACP server.
///
/// Server conforms to following rules:
///
/// 1. supports basic flow (Initialize->New Session->Text Prompt)
/// 2. on all prompts server reply with the same content
/// 3. server is single user. After first user disconnects server exits
fn serve_acp_client(listener: TcpListener) {
    fn send_jrpc<S: Read + Write>(ws: &mut WebSocket<S>, msg: AgentOutgoingMessage) {
        let json = serde_json::to_string(&JsonRpcMessage::wrap(msg)).expect("Failed serializing");
        // We don't really care about sending errors.
        // Most likely it happens because a client disconnected early
        let _ = ws.send(Message::Text(Utf8Bytes::from(json)));
    }

    // We intentionally panic here, because in the test environment it's much more convenient
    // to have an error immediately on stderr. It's not reliable to communicate errors via Result.
    // The test might be stuck somewhere else preventing it for joining on server thread Result.
    let (tcp_stream, _) = listener.accept().expect("Failed on accept()");
    let mut ws = tungstenite::accept(tcp_stream).expect("Failed on websocket handshake");
    let session_id = "SHINY-SESSION-ID";

    loop {
        let msg = match ws.read() {
            Ok(msg) => msg,
            // we have a separate server for each test, so stopping after serving first client,
            Err(tungstenite::Error::ConnectionClosed) => return,
            Err(e) => panic!("{e}"),
        };

        let Message::Text(text) = msg else {
            continue;
        };

        let raw: RawIncomingMessage<'_> = serde_json::from_str(&text).unwrap();
        let Some((method, id)) = raw.method.zip(raw.id) else {
            continue;
        };

        let request = AgentSide::decode_request(method, raw.params).unwrap();
        let response = match request {
            ClientRequest::InitializeRequest(req) => {
                AgentResponse::InitializeResponse(InitializeResponse::new(req.protocol_version))
            }
            ClientRequest::NewSessionRequest(_) => {
                AgentResponse::NewSessionResponse(NewSessionResponse::new(session_id))
            }
            ClientRequest::PromptRequest(r) => {
                if let Some(block) = r.prompt.first() {
                    let update = SessionUpdate::AgentMessageChunk(ContentChunk::new(block.clone()));
                    let notification = AgentNotification::SessionNotification(
                        SessionNotification::new(session_id, update),
                    );
                    send_jrpc(
                        &mut ws,
                        AgentOutgoingMessage::Notification(Notification {
                            method: notification.method().into(),
                            params: Some(notification),
                        }),
                    );
                }
                AgentResponse::PromptResponse(PromptResponse::new(StopReason::EndTurn))
            }
            _ => continue,
        };

        send_jrpc(
            &mut ws,
            AgentOutgoingMessage::Response(Response::new(id, Ok(response))),
        );
    }
}

/// A simple wrapper around a child process with piped stdin/stdout.
struct ChildProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl ChildProcess {
    fn send_line(&mut self, line: &str) {
        // It's important to send newline character, so that transport will trigger on a new message
        writeln!(self.stdin, "{}", line).expect("Failed to write to child stdin");
        self.stdin.flush().expect("Failed to flush child stdin");
    }

    fn read_line(&mut self) -> String {
        let mut line = String::new();
        self.stdout
            .read_line(&mut line)
            .expect("Failed to read from child stdout");
        line
    }

    fn kill(&mut self) {
        self.child.kill().ok();
        self.child.wait().ok();
    }
}

impl Drop for ChildProcess {
    fn drop(&mut self) {
        self.kill();
    }
}

fn get_jcp_binary_path() -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_BIN_EXE_jcp"));
    if cfg!(windows) {
        path.set_extension("exe");
    }
    path
}
