use agent_client_protocol::{
    self as acp, AcpAgent, Agent, AgentNotification, AgentResponse, Client, ConnectionTo,
    schema::{
        ContentBlock, ContentChunk, InitializeRequest, InitializeResponse, JsonRpcMessage,
        NewSessionRequest, NewSessionResponse, Notification, PromptRequest, PromptResponse,
        ProtocolVersion, Response, SessionNotification, SessionUpdate, StopReason, TextContent,
    },
};
use jcp::{
    AS_ACP_URL_ENV_NAME,
    auth::AccessTokens,
    decode_acp, decode_jrpc,
    keychain::{
        AI_PLATFORM_TOKEN_ENV_NAME, JCP_ACCESS_TOKEN_ENV_NAME, file::KEYCHAIN_FILE_ENV_NAME,
    },
};
use serde::Serialize;
use serde_json::Value as JsonValue;
use std::{
    env,
    io::{Read, Write},
    net::TcpListener,
    path::PathBuf,
    process::Command,
    str::FromStr,
    thread,
};
use tempfile::tempdir;
use tungstenite::{Message, Utf8Bytes, WebSocket, error::ProtocolError};
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
    let config = E2eConfig {
        server_fn: Some(Box::new(response_fn)),
        ..Default::default()
    };

    run_cli(config, async |connection| {
        connection
            .send_request(InitializeRequest::new(ProtocolVersion::V1))
            .block_task()
            .await?;

        // We need to start agent in a git directory, otherwise it will fail reading git remote
        // Using current project dir
        let project_dir = env::current_dir().unwrap();
        let new_session_response = connection
            .send_request(NewSessionRequest::new(project_dir))
            .block_task()
            .await?;

        let prompt_response = connection
            .send_request(PromptRequest::new(
                new_session_response.session_id.clone(),
                vec![ContentBlock::Text(TextContent::new("Prompt"))],
            ))
            .block_task()
            .await?;

        assert_eq!(prompt_response.stop_reason, StopReason::EndTurn);
        Ok(())
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn run_outside_git_directory() {
    let tmp_dir = tempdir().unwrap();
    let path = tmp_dir.path();
    let config = E2eConfig {
        ..Default::default()
    };
    let result = run_cli(config, async |connection| {
        connection
            .send_request(InitializeRequest::new(ProtocolVersion::V1))
            .block_task()
            .await?;
        // creating session in an empty directory without git
        connection
            .send_request(NewSessionRequest::new(path))
            .block_task()
            .await?;
        Ok(())
    })
    .await;

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
}

#[tokio::test]
async fn run_without_login() {
    // Emulating cli without login
    let config = E2eConfig {
        keychain_file: Some("./not-existing-keychain-file".into()),
        explicit_access_tokens: None,
        server_fn: None,
        ..Default::default()
    };
    let result = run_cli(config, async |connection| {
        connection
            .send_request(InitializeRequest::new(ProtocolVersion::V1))
            .block_task()
            .await
    })
    .await;

    match result {
        Ok(_) => panic!("InitializeRequest must fail, because we are not logged in."),
        Err(e) => {
            let msg = e.to_string();
            assert!(
                msg.contains("`jcp login`"),
                "Expecting message saying that user need to do `jcp login` first. Got: {msg}"
            );
        }
    }
}

type PromptFn = Box<dyn Fn(&[ContentBlock]) -> ContentBlock + Send>;

#[non_exhaustive]
struct E2eConfig {
    /// Function that handles ACP prompt. If None do not start a server.
    /// Some tests that checks fully local beheviour do not have to start server at all
    server_fn: Option<PromptFn>,
    keychain_file: Option<PathBuf>,
    explicit_access_tokens: Option<AccessTokens>,
}

/// Start the mock server and jcp process, ready for testing.
async fn run_cli<T>(
    config: E2eConfig,
    main_fn: impl AsyncFnOnce(ConnectionTo<Agent>) -> Result<T, acp::Error>,
) -> Result<T, acp::Error> {
    let listener = TcpListener::bind("127.0.0.1:0").expect("Unable to bind socket");

    let addr = listener.local_addr().unwrap();
    let url = Url::from_str(&format!("ws://{addr}")).unwrap();

    // Only start server if prompt function has been given
    let server_handle = config
        .server_fn
        .map(|f| thread::spawn(move || serve_acp_client(listener, f)));

    let mut args = vec![format!("{}={url}", AS_ACP_URL_ENV_NAME)];

    if let Some(keychain_file) = config.keychain_file {
        args.push(format!(
            "{}={}",
            KEYCHAIN_FILE_ENV_NAME,
            keychain_file.to_str().unwrap()
        ));
    }
    if let Some(access_tokens) = config.explicit_access_tokens {
        args.push(format!(
            "{}={}",
            JCP_ACCESS_TOKEN_ENV_NAME, access_tokens.jcp_access_token
        ));
        if let Some(ai_token) = access_tokens.ai_access_token {
            args.push(format!("{}={ai_token}", AI_PLATFORM_TOKEN_ENV_NAME));
        }
    }

    args.extend([
        get_jcp_binary_path().display().to_string(),
        // Ability to override URL's are only available under --staging flag
        "--staging".to_string(),
        "acp".to_string(),
    ]);

    let agent = AcpAgent::from_args(args).unwrap();

    let result = Client.builder().connect_with(agent, main_fn).await;
    if let Some(server_handle) = server_handle {
        let _ = server_handle.join();
    }
    result
}

impl Default for E2eConfig {
    fn default() -> Self {
        Self {
            keychain_file: None,
            server_fn: Some(Box::new(echo_back)),
            explicit_access_tokens: Some(AccessTokens {
                jcp_access_token: "test-token".into(),
                ai_access_token: None,
            }),
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
    fn send_jrpc<S: Read + Write, M: Serialize>(ws: &mut WebSocket<S>, msg: M) {
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
            Err(tungstenite::Error::Protocol(ProtocolError::ResetWithoutClosingHandshake)) => break,
            Err(e) => panic!("{e}"),
        };

        match msg {
            Message::Text(text) => {
                let json = serde_json::from_str::<JsonValue>(&text).unwrap();
                let jrpc = decode_jrpc(json).unwrap();

                let response = if let Some(rq) = decode_acp::<InitializeRequest>(&jrpc).unwrap() {
                    AgentResponse::InitializeResponse(InitializeResponse::new(rq.protocol_version))
                } else if decode_acp::<NewSessionRequest>(&jrpc).unwrap().is_some() {
                    AgentResponse::NewSessionResponse(NewSessionResponse::new(session_id))
                } else if let Some(rq) = decode_acp::<PromptRequest>(&jrpc).unwrap() {
                    let reply = prompt_fn(&rq.prompt);
                    let update = SessionUpdate::AgentMessageChunk(ContentChunk::new(reply));
                    let notification = AgentNotification::SessionNotification(
                        SessionNotification::new(session_id, update),
                    );
                    send_jrpc(
                        &mut ws,
                        Notification {
                            method: notification.method().into(),
                            params: Some(notification),
                        },
                    );
                    AgentResponse::PromptResponse(PromptResponse::new(StopReason::EndTurn))
                } else {
                    continue;
                };
                send_jrpc(&mut ws, Response::new(jrpc.id, Ok(response)));
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
