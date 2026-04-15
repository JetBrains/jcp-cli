use agent_client_protocol::{
    self as acp, Agent, AgentNotification, AgentResponse, AgentSide, Client, ClientRequest,
    ClientSideConnection, ContentBlock, ContentChunk, InitializeRequest, InitializeResponse,
    JsonRpcMessage, NewSessionRequest, NewSessionResponse, Notification, PromptRequest,
    PromptResponse, ProtocolVersion, RequestPermissionRequest, RequestPermissionResponse, Response,
    SessionNotification, SessionUpdate, Side, StopReason, TextContent,
};
use jcp::{
    AS_ACP_URL_ENV_NAME, AgentOutgoingMessage, RawIncomingMessage,
    auth::AccessTokens,
    keychain::{
        AI_PLATFORM_TOKEN_ENV_NAME, JCP_ACCESS_TOKEN_ENV_NAME, file::KEYCHAIN_FILE_ENV_NAME,
    },
};
use std::{
    cell::RefCell,
    io::{Read, Write},
    net::TcpListener,
    path::PathBuf,
    process::{Command, Stdio},
    rc::Rc,
    str::FromStr,
    thread::{self, JoinHandle},
};
use tempfile::tempdir;
use tokio::{
    process::Child,
    task::{AbortHandle, LocalSet, spawn_local},
};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
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

#[tokio::test]
async fn prompt_turn() {
    fn response_fn(input: &[ContentBlock]) -> ContentBlock {
        let response = format!("Reply: {}", extract_text_only(input));
        ContentBlock::Text(TextContent::new(response))
    }

    LocalSet::new()
        .run_until(async {
            let e2e = E2eConfig {
                server_fn: Some(Box::new(response_fn)),
                ..Default::default()
            }
            .bootstrap();

            // Step 1: Initialize handshake
            e2e.initialize_check().await.unwrap();

            // Step 2: Creating a new session
            let response = e2e.new_session_check().await.unwrap();

            // Step 3: prompt turn
            let input_prompt = "Prompt";
            let response = e2e
                .client
                .prompt(PromptRequest::new(
                    response.session_id,
                    vec![input_prompt.into()],
                ))
                .await
                .unwrap();
            assert_eq!(response.stop_reason, StopReason::EndTurn);

            let notifications = e2e.take_notifications().await;
            assert_eq!(
                notifications.len(),
                1,
                "Server should echo back with original prompt"
            );
            let content_blocks = extract_session_message_chunks(&notifications);
            let text = extract_text_only(&content_blocks);
            assert_eq!(text, format!("Reply: {}", input_prompt));
            e2e.teardown().await;
        })
        .await;
}

fn extract_session_message_chunks(notifications: &[SessionNotification]) -> Vec<ContentBlock> {
    notifications
        .iter()
        .flat_map(|n| match &n.update {
            SessionUpdate::AgentMessageChunk(chunk) => Some(chunk.content.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn run_outside_git_directory() {
    LocalSet::new()
        .run_until(async {
            let tmp_dir = tempdir().unwrap();
            let e2e = E2eConfig {
                // spawning in empty directory without git
                project_dir: Some(tmp_dir.path().to_path_buf()),
                suppress_stderr: true,
                ..Default::default()
            }
            .bootstrap();

            // Initialize should succeed even outside a git directory
            e2e.initialize_check()
                .await
                .expect("Initialize should succeed");

            // NewSession should fail because cwd is not a git repository
            let result = e2e.client.new_session(NewSessionRequest::new("./")).await;
            match result {
                Ok(r) => panic!("JSON RPC error is expected. Got: {r:?}"),
                Err(e) => {
                    assert_eq!(e.code, acp::ErrorCode::InvalidParams);
                    assert!(
                        e.message.contains("fatal: not a git repository"),
                        "Expected git error message, got: {}",
                        e.message
                    );
                }
            }
            e2e.teardown().await;
        })
        .await;
}

#[tokio::test]
async fn run_without_login() {
    LocalSet::new()
        .run_until(async {
            // Emulating cli without login
            let e2e = E2eConfig {
                keychain_file: Some("./not-existing-keychain-file".into()),
                suppress_stderr: true,
                explicit_access_tokens: None,
                server_fn: None,
                ..Default::default()
            }
            .bootstrap();

            let Err(e) = e2e.initialize_check().await else {
                panic!(
                    "InitializeRequest must fail, because we are not logged in. Instead got successful result"
                );
            };

            let msg = e.to_string();
            assert!(
                msg.contains("`jcp login`"),
                "Expecting message saying that user need to do `jcp login` first. Got: {msg}"
            );

            e2e.teardown().await;
        })
        .await;
}

/// Mock ACP client implementation for tests.
///
/// Collects session notifications for later inspection.
struct TestClient {
    notifications: Rc<RefCell<Vec<SessionNotification>>>,
}

#[async_trait::async_trait(?Send)]
impl Client for TestClient {
    async fn request_permission(
        &self,
        _args: RequestPermissionRequest,
    ) -> acp::Result<RequestPermissionResponse> {
        Err(acp::Error::method_not_found())
    }

    async fn session_notification(&self, args: SessionNotification) -> acp::Result<()> {
        self.notifications.borrow_mut().push(args);
        Ok(())
    }
}

/// E2E test harness that manages mock server and jcp processes.
///
/// Starts an in-process mock ACP server on a background task
/// and spawns `jcp acp`, using [`ClientSideConnection`] to communicate
/// with jcp over its stdin/stdout.
struct E2eHarness {
    client: ClientSideConnection,
    notifications: Rc<RefCell<Vec<SessionNotification>>>,
    child: Child,
    server_handle: Option<JoinHandle<()>>,
    /// Abort handle used to basically close stdin of an adapter process, so that it can
    /// terminated gracefully
    adapter_abort_handle: AbortHandle,
}

type PromptFn = Box<dyn Fn(&[ContentBlock]) -> ContentBlock + Send>;

#[non_exhaustive]
struct E2eConfig {
    project_dir: Option<PathBuf>,
    /// If true, stderr of jcp binary will be sent to /dev/null
    /// Set it if test scenario expects to generate errors/warning is jcp binary
    suppress_stderr: bool,
    /// Function that handles ACP prompt. If None do not start a server.
    /// Some tests that checks fully local beheviour do not have to start server at all
    server_fn: Option<PromptFn>,
    keychain_file: Option<PathBuf>,
    explicit_access_tokens: Option<AccessTokens>,
}

impl E2eConfig {
    /// Start the mock server and jcp process, ready for testing.
    fn bootstrap(self) -> E2eHarness {
        let listener = TcpListener::bind("127.0.0.1:0").expect("Unable to bind socket");

        let addr = listener.local_addr().unwrap();
        let url = Url::from_str(&format!("ws://{addr}")).unwrap();

        // Only start server if prompt function has been given
        let server_handle = self
            .server_fn
            .map(|f| thread::spawn(move || serve_acp_client(listener, f)));

        let mut cmd = tokio::process::Command::new(get_jcp_binary_path());
        cmd.args(["acp"])
            .env(AS_ACP_URL_ENV_NAME, url.as_str())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped());

        if let Some(keychain_file) = self.keychain_file {
            cmd.env(KEYCHAIN_FILE_ENV_NAME, keychain_file.to_str().unwrap());
        }
        if let Some(access_tokens) = self.explicit_access_tokens {
            cmd.env(JCP_ACCESS_TOKEN_ENV_NAME, access_tokens.jcp_access_token);
            if let Some(ai_token) = access_tokens.ai_access_token {
                cmd.env(AI_PLATFORM_TOKEN_ENV_NAME, ai_token);
            }
        }

        if let Some(project_dir) = self.project_dir {
            cmd.current_dir(project_dir);
        }
        if self.suppress_stderr {
            cmd.stderr(Stdio::null());
        }

        let mut child = cmd.spawn().expect("Failed to spawn child process");
        let stdin = child.stdin.take().unwrap().compat_write();
        let stdout = child.stdout.take().unwrap().compat();

        let notifications = Rc::new(RefCell::new(Vec::new()));
        let client = TestClient {
            notifications: Rc::clone(&notifications),
        };

        let (client, io_task) = ClientSideConnection::new(client, stdin, stdout, |f| {
            spawn_local(f);
        });
        let io_task = spawn_local(async { io_task.await.unwrap() });

        E2eHarness {
            client,
            notifications,
            child,
            server_handle,
            adapter_abort_handle: io_task.abort_handle(),
        }
    }
}

impl Default for E2eConfig {
    fn default() -> Self {
        Self {
            project_dir: None,
            suppress_stderr: false,
            keychain_file: None,
            server_fn: Some(Box::new(echo_back)),
            explicit_access_tokens: Some(AccessTokens {
                jcp_access_token: "test-token".into(),
                ai_access_token: None,
            }),
        }
    }
}

impl E2eHarness {
    /// Does initialization and basic checks
    async fn initialize_check(&self) -> Result<InitializeResponse, acp::Error> {
        let response = self
            .client
            .initialize(InitializeRequest::new(ProtocolVersion::V1))
            .await?;
        assert_eq!(response.protocol_version, ProtocolVersion::V1);
        Ok(response)
    }

    async fn new_session_check(&self) -> Result<NewSessionResponse, acp::Error> {
        let response = self
            .client
            .new_session(NewSessionRequest::new("./"))
            .await?;
        assert!(!response.session_id.0.is_empty());
        Ok(response)
    }

    /// Drains all collected session notifications.
    async fn take_notifications(&self) -> Vec<SessionNotification> {
        self.notifications.borrow_mut().drain(..).collect()
    }

    async fn teardown(mut self) {
        drop(self.client);
        self.adapter_abort_handle.abort();
        self.child.wait().await.ok();
        if let Some(server_join_handle) = self.server_handle.take() {
            server_join_handle.join().unwrap();
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
fn serve_acp_client(listener: TcpListener, prompt_fn: PromptFn) {
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
            Err(tungstenite::Error::ConnectionClosed) => break,
            Err(e) => panic!("{e}"),
        };

        match msg {
            Message::Text(text) => {
                let raw: RawIncomingMessage<'_> = serde_json::from_str(&text).unwrap();
                let Some((method, id)) = raw.method.zip(raw.id) else {
                    continue;
                };

                let request = AgentSide::decode_request(method, raw.params).unwrap();
                let response = match request {
                    ClientRequest::InitializeRequest(req) => AgentResponse::InitializeResponse(
                        InitializeResponse::new(req.protocol_version),
                    ),
                    ClientRequest::NewSessionRequest(_) => {
                        AgentResponse::NewSessionResponse(NewSessionResponse::new(session_id))
                    }
                    ClientRequest::PromptRequest(r) => {
                        let reply = prompt_fn(&r.prompt);
                        let update = SessionUpdate::AgentMessageChunk(ContentChunk::new(reply));
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
                        AgentResponse::PromptResponse(PromptResponse::new(StopReason::EndTurn))
                    }
                    _ => continue,
                };
                send_jrpc(
                    &mut ws,
                    AgentOutgoingMessage::Response(Response::new(id, Ok(response))),
                );
            }
            Message::Close(_) => break,
            _ => continue,
        }
    }
}

fn get_jcp_binary_path() -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_BIN_EXE_jcp"));
    if cfg!(windows) {
        path.set_extension("exe");
    }
    path
}

fn echo_back(input: &[ContentBlock]) -> ContentBlock {
    ContentBlock::Text(TextContent::new(extract_text_only(input)))
}

/// Extract text content from an input and ignore everything else
fn extract_text_only(input: &[ContentBlock]) -> String {
    input
        .iter()
        .flat_map(|b| match b {
            ContentBlock::Text(text_content) => Some(text_content.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}
