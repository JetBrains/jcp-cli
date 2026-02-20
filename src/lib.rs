use agent_client_protocol::{
    self as acp, AgentNotification, AgentSide, CLIENT_METHOD_NAMES, ClientRequest, ClientSide,
    ContentBlock, ContentChunk, JsonRpcMessage, NewSessionRequest, Notification, OutgoingMessage,
    PromptResponse, RawValue, Request, RequestId, Response, SessionId, SessionNotification,
    SessionUpdate, Side, StopReason, TextContent,
};
use async_trait::async_trait;
use futures::{FutureExt, Sink, Stream};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use std::{collections::HashMap, io};
use tokio::{
    fs::File,
    io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, Lines},
};
use tungstenite::{
    Message, Utf8Bytes,
    protocol::{CloseFrame, frame::coding::CloseCode},
};

pub mod auth;
pub mod keychain;

pub type AgentOutgoingMessage = OutgoingMessage<AgentSide, ClientSide>;
pub type ClientOutgoingMessage = OutgoingMessage<ClientSide, AgentSide>;

pub const JSON_RPC_ERROR_INVALID_PARAMS: i32 = -32602;

/// A bidirectional transport for JSON-RPC messages.
///
/// This trait abstracts over different transport mechanisms (stdio, websocket, channels)
/// providing a simple async interface for sending and receiving JSON values.
#[async_trait]
pub trait Transport {
    /// Receive the next message from the transport.
    /// Returns `Ok(None)` when the transport is closed.
    ///
    /// All implementations need to by cancel safe
    async fn recv(&mut self) -> io::Result<Option<JsonValue>>;

    /// Send a message through the transport.
    async fn send(&mut self, msg: JsonValue) -> io::Result<()>;
}

/// Transport implementation for arbitrary async readers/writers.
///
/// Reads newline-delimited JSON from reader and writes newline-delimited JSON to writer.
pub struct IoTransport {
    lines: Lines<BufReader<Box<dyn AsyncRead + Unpin + Send>>>,
    writer: Box<dyn AsyncWrite + Unpin + Send>,
}

impl IoTransport {
    pub fn new(
        reader: impl AsyncRead + Unpin + Send + 'static,
        writer: impl AsyncWrite + Unpin + Send + 'static,
    ) -> Self {
        let reader: Box<dyn AsyncRead + Unpin + Send> = Box::new(reader);
        Self {
            lines: BufReader::new(reader).lines(),
            writer: Box::new(writer),
        }
    }
}

#[async_trait]
impl Transport for IoTransport {
    async fn recv(&mut self) -> io::Result<Option<JsonValue>> {
        // Lines::next_line() is cancellation safe per tokio documentation
        match self.lines.next_line().await? {
            Some(line) => serde_json::from_str(&line)
                .map_err(to_io_invalid_data_err)
                .map(Some),
            None => Ok(None),
        }
    }

    async fn send(&mut self, msg: JsonValue) -> io::Result<()> {
        let json = serde_json::to_string(&msg)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        self.writer.write_all(json.as_bytes()).await?;
        self.writer.write_all(b"\n").await?;
        self.writer.flush().await
    }
}

/// Transport implementation for WebSocket connections.
pub struct WebSocketTransport {
    rx: Box<dyn Stream<Item = Result<Message, tungstenite::Error>> + Unpin + Send>,
    tx: Box<dyn Sink<Message, Error = tungstenite::Error> + Unpin + Send>,
}

impl WebSocketTransport {
    pub fn new(
        rx: impl Stream<Item = Result<Message, tungstenite::Error>> + Unpin + Send + 'static,
        tx: impl Sink<Message, Error = tungstenite::Error> + Unpin + Send + 'static,
    ) -> Self {
        Self {
            rx: Box::new(rx),
            tx: Box::new(tx),
        }
    }
}

#[async_trait]
impl Transport for WebSocketTransport {
    async fn recv(&mut self) -> io::Result<Option<JsonValue>> {
        loop {
            match self.rx.next().await {
                Some(Ok(msg)) => match msg {
                    Message::Text(text) => {
                        return serde_json::from_str(&text)
                            .map_err(to_io_invalid_data_err)
                            .map(Some);
                    }
                    Message::Binary(_) => {
                        eprintln!("Message::Binary is not supported. Skipping");
                        continue;
                    }
                    Message::Ping(bytes) => {
                        self.tx
                            .send(Message::Pong(bytes))
                            .await
                            .map_err(io::Error::other)?;
                        continue;
                    }
                    Message::Pong(_) => continue,
                    Message::Close(close_frame) => {
                        // Replying with the close frame
                        let _ = self.tx.send(Message::Close(None)).await;
                        return match close_frame {
                            Some(CloseFrame {
                                code: CloseCode::Normal,
                                ..
                            }) => Ok(None),
                            close_frame => Err(io::Error::new(
                                io::ErrorKind::UnexpectedEof,
                                format!("Close frame received: {:?}", close_frame),
                            )),
                        };
                    }
                    Message::Frame(_) => {
                        eprintln!("Message::Frame is not supported. Skipping");
                        continue;
                    }
                },
                Some(Err(e)) => return Err(io::Error::other(e)),
                None => return Ok(None),
            }
        }
    }

    async fn send(&mut self, msg: JsonValue) -> io::Result<()> {
        let message = serde_json::to_string(&msg)
            .map_err(to_io_invalid_data_err)
            .map(Utf8Bytes::from)
            .map(Message::Text)?;
        self.tx.send(message).await.map_err(io::Error::other)
    }
}

/// Configuration for the ACP-JCP adapter
#[derive(Clone)]
pub struct Config {
    pub git_url: String,
    pub branch: String,
    pub revision: String,
    pub ai_platform_token: String,
    pub supports_user_git_auth_flow: bool,
}

impl Config {
    pub fn new_session_meta(&self) -> NewSessionMeta {
        NewSessionMeta {
            remote: GitRemoteInfo {
                branch: self.branch.clone(),
                url: self.git_url.clone(),
                revision: self.revision.clone(),
            },
            ai_platform_token: self.ai_platform_token.clone(),
            supports_user_git_auth_flow: self.supports_user_git_auth_flow,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct NewSessionMeta {
    #[serde(rename = "remote")]
    pub remote: GitRemoteInfo,

    #[serde(rename = "jbAiToken")]
    pub ai_platform_token: String,

    #[serde(rename = "supportsUserGitAuthFlow")]
    pub supports_user_git_auth_flow: bool,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct EndTurnMeta {
    #[serde(rename = "target")]
    pub target: GitRemoteInfo,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct GitRemoteInfo {
    #[serde(rename = "branch")]
    pub branch: String,

    #[serde(rename = "url")]
    pub url: String,

    #[serde(rename = "revision")]
    pub revision: String,
}

#[derive(Debug, Deserialize)]
pub struct RawIncomingMessage<'a> {
    pub id: Option<acp::RequestId>,
    pub method: Option<&'a str>,
    pub params: Option<&'a RawValue>,
    pub result: Option<&'a RawValue>,
    pub error: Option<acp::Error>,
}

/// Adapter that bridges ACP client and JCP server communication.
///
/// This struct processes messages from both channels using `tokio::select!`,
/// allowing for synchronous test-driving without spawning separate tasks.
pub struct Adapter {
    /// Can be missing, in which case adapter should report error on initialize handlshake
    config: Result<Config, String>,
    client: Box<dyn Transport>,
    agent: Box<dyn Transport>,
    traffic_log: TrafficLog,

    /// Mapping from prompt request id to session id
    ///
    /// We need this to be able to send prompt followups before prompt finished
    prompt_request_mapping: HashMap<RequestId, SessionId>,
}

impl Adapter {
    /// Create a new adapter with the given configuration and transports.
    ///
    /// - `downlink`: transport to the client (IDE)
    /// - `uplink`: transport to the server (JCP)
    pub fn new(
        config: Result<Config, String>,
        client: Box<dyn Transport>,
        agent: Box<dyn Transport>,
    ) -> Self {
        Self {
            config,
            client,
            agent,
            traffic_log: TrafficLog::default(),
            prompt_request_mapping: HashMap::new(),
        }
    }

    pub fn set_traffic_log(&mut self, traffic_log: TrafficLog) {
        self.traffic_log = traffic_log;
    }

    /// Process the next message from either the client or server channel.
    ///
    /// Returns `true` if there are more messages can be handled
    /// Returns `false` when both channels are closed (end of communication).
    pub async fn handle_next_message(&mut self) -> io::Result<bool> {
        use Status::*;
        enum Status {
            AgentTerminated,
            ClientTerminated,
            MessageProcessed,
        }

        let result = tokio::select! {
            // We don't care about message processing order fairness, but random selection
            // makes tests non deterministic, hence biased.
            biased;
            msg = self.client.recv() => {
                if let Some(msg) = msg? {
                    self.handle_client_message(msg).await?;
                    MessageProcessed
                } else {
                    ClientTerminated
                }
            }
            msg = self.agent.recv() => {
                if let Some(msg) = msg? {
                    self.handle_agent_message(msg).await?;
                    MessageProcessed
                } else {
                    AgentTerminated
                }
            }
        };

        // If one of the transports reported EOF, we still need to process another one
        match result {
            MessageProcessed => Ok(true),
            ClientTerminated => {
                if let Some(msg) = self.agent.recv().await? {
                    self.handle_agent_message(msg).await?;
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            AgentTerminated => {
                if let Some(msg) = self.client.recv().await? {
                    self.handle_client_message(msg).await?;
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
        }
    }

    /// Handles all enqueued messages in the transport
    ///
    /// Should be used for tests only, because using this method in a loop will cause CPU spin.
    /// Use [`Self::handle_next_message()`] instead
    pub async fn handle_enqueued_messages(&mut self) -> io::Result<()> {
        while let Some(msg) = self.client.recv().now_or_never().transpose()?.flatten() {
            self.handle_client_message(msg).await?;
        }
        while let Some(msg) = self.agent.recv().now_or_never().transpose()?.flatten() {
            self.handle_agent_message(msg).await?;
        }
        Ok(())
    }

    async fn handle_agent_message(&mut self, msg: JsonValue) -> io::Result<()> {
        let _ = self.traffic_log.write(&msg).await;

        let request_id = serde_json::from_value::<RequestId>(msg["id"].clone()).ok();

        if let Some(request_id) = request_id
            && msg["result"] != JsonValue::Null
            && let Some(session_id) = self.prompt_request_mapping.remove(&request_id)
        {
            // it is a response to a prompt request
            let r = serde_json::from_value::<PromptResponse>(msg["result"].clone())
                .map_err(to_io_invalid_data_err)?;
            if r.stop_reason == StopReason::EndTurn
                && let Some(meta) = r.meta
            {
                let remote_info = serde_json::from_value::<EndTurnMeta>(JsonValue::Object(meta));
                let end_turn_message = remote_info
                    .ok()
                    .map(|r| r.target)
                    .and_then(git_end_turn_message);
                if let Some(message) = end_turn_message {
                    let notification = create_session_update_notification(session_id, message);
                    let value =
                        serde_json::to_value(&notification).map_err(to_io_invalid_data_err)?;
                    self.client.send(value).await?;
                }
            }
        }

        self.client.send(msg).await
    }

    async fn handle_client_message(&mut self, msg: JsonValue) -> io::Result<()> {
        let _ = self.traffic_log.write(&msg).await;

        // This is ugly hack, but we need to serialize here back to string, otherwise
        // we can not use AgentSide::decode_request()
        let msg_str = msg.to_string();
        let rpc_msg: RawIncomingMessage<'_> =
            serde_json::from_str(&msg_str).map_err(to_io_invalid_data_err)?;

        if let Some((method, id)) = rpc_msg.method.zip(rpc_msg.id) {
            let mut request = AgentSide::decode_request(method, rpc_msg.params)
                .map_err(to_io_invalid_data_err)?;

            if let ClientRequest::InitializeRequest(_) = request {
                // On InitializeRequest we need to check all important preciditions and fail
                // query to report a error to user if any.
                if let Err(e) = &self.config {
                    // no git config. Terminating protocol early
                    let msg =
                        JsonRpcMessage::wrap(AgentOutgoingMessage::Response(Response::Error {
                            id: id.clone(),
                            error: acp::Error::new(JSON_RPC_ERROR_INVALID_PARAMS, e),
                        }));
                    let value = serde_json::to_value(&msg).map_err(to_io_invalid_data_err)?;
                    self.client.send(value).await?;

                    return Err(io::Error::other(e.clone()));
                }
            } else if let ClientRequest::NewSessionRequest(r) = &mut request {
                // On a NewSessionRequest we need to inject remote info (git url and branch)
                //
                // Assuming git config present, because we checking it on a init phase
                let meta = self
                    .config
                    .as_ref()
                    .expect("No config found")
                    .new_session_meta();
                inject_new_session_meta(r, &meta)?;
            } else if let ClientRequest::PromptRequest(r) = &request {
                self.prompt_request_mapping
                    .insert(id.clone(), r.session_id.clone());
            }

            // Sending message to the server
            let msg = JsonRpcMessage::wrap(ClientOutgoingMessage::Request(Request {
                id,
                method: method.into(),
                params: Some(request),
            }));

            let value = serde_json::to_value(&msg).map_err(to_io_invalid_data_err)?;
            self.agent.send(value).await?;
        } else {
            // Sending notifications to JCP without modification
            self.agent.send(msg).await?;
        }
        Ok(())
    }

    /// Run the adapter until both channels are closed.
    pub async fn run(&mut self) -> io::Result<()> {
        while self.handle_next_message().await? {}
        Ok(())
    }
}

fn git_end_turn_message(git_info: GitRemoteInfo) -> Option<String> {
    if !git_info.branch.is_empty() {
        Some(format!(
            "Results branch: {} ({})",
            git_info.branch, git_info.url
        ))
    } else if !git_info.revision.is_empty() {
        Some(format!(
            "Results commit: #{} ({})",
            git_info.revision, git_info.url
        ))
    } else {
        None
    }
}

fn create_session_update_notification(
    session_id: SessionId,
    message: impl Into<String>,
) -> JsonRpcMessage<AgentOutgoingMessage> {
    JsonRpcMessage::wrap(AgentOutgoingMessage::Notification(Notification {
        method: CLIENT_METHOD_NAMES.session_update.into(),
        params: Some(AgentNotification::SessionNotification(
            SessionNotification::new(
                session_id,
                SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
                    TextContent::new(message),
                ))),
            ),
        )),
    }))
}

#[derive(Default)]
pub struct TrafficLog {
    file: Option<File>,
}

impl TrafficLog {
    pub async fn new(path: Option<impl AsRef<std::path::Path>>) -> io::Result<Self> {
        let file = if let Some(path) = path {
            Some(File::create(path).await?)
        } else {
            None
        };
        Ok(Self { file })
    }

    pub async fn write(&mut self, msg: &JsonValue) -> io::Result<()> {
        if let Some(file) = &mut self.file {
            let json = serde_json::to_string_pretty(msg).map_err(to_io_invalid_data_err)?;
            file.write_all(json.as_bytes()).await
        } else {
            Ok(())
        }
    }
}

/// JCP needs to know where to clone git repo from and what branch to use
fn inject_new_session_meta(req: &mut NewSessionRequest, meta: &NewSessionMeta) -> io::Result<()> {
    if let JsonValue::Object(json) = serde_json::to_value(meta).map_err(to_io_invalid_data_err)? {
        req.meta = Some(json);
    }
    Ok(())
}

fn to_io_invalid_data_err<E: Into<Box<dyn std::error::Error + Send + Sync>>>(e: E) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::{AgentSide, ClientRequest, LoadSessionRequest, Side};
    use drop_check::{IntersperceExt, cancellations};
    use serde::de::DeserializeOwned;
    use serde_json::Value;
    use std::{fmt::Debug, io::Cursor};

    #[test]
    fn test_new_session_meta_deserialization() {
        check_serialization(
            r#"{
                "remote": {
                    "branch":"main",
                    "url":"https://example.com/repo.git",
                    "revision":"18adf27d36912b2e255c71327146ac21116e232f"
                },
                "jbAiToken": "test_token",
                "supportsUserGitAuthFlow": false
            }"#,
            NewSessionMeta {
                remote: GitRemoteInfo {
                    branch: "main".to_string(),
                    url: "https://example.com/repo.git".to_string(),
                    revision: "18adf27d36912b2e255c71327146ac21116e232f".to_string(),
                },
                ai_platform_token: "test_token".to_string(),
                supports_user_git_auth_flow: false,
            },
        );
    }

    #[test]
    fn test_end_turn_meta_deserialization() {
        check_serialization(
            r#"{
                "target": {
                    "branch":"main",
                    "url":"https://example.com/repo.git",
                    "revision":"18adf27d36912b2e255c71327146ac21116e232f"
                }
            }"#,
            EndTurnMeta {
                target: GitRemoteInfo {
                    branch: "main".to_string(),
                    url: "https://example.com/repo.git".to_string(),
                    revision: "18adf27d36912b2e255c71327146ac21116e232f".to_string(),
                },
            },
        );
    }

    #[test]
    fn json_rpc_request_can_be_deserialized_using_raw_request() {
        let json = r#"{
            "jsonrpc":"2.0",
            "id":0,
            "method":"initialize",
            "params": {
                "protocolVersion":1,
                "clientCapabilities": {
                    "fs": {
                        "readTextFile":true,
                        "writeTextFile":true
                    },
                    "terminal":true,
                    "_meta": {},
                    "clientInfo": {"name":"ide","title":"IDE","version":"0.1"}
                }
            }
        }"#;

        let raw_message: RawIncomingMessage = serde_json::from_str(json).unwrap();
        let request =
            AgentSide::decode_request(raw_message.method.unwrap(), raw_message.params).unwrap();

        if !matches!(request, ClientRequest::InitializeRequest(..)) {
            panic!("Unexpected request: {:?}", request);
        }
    }

    #[test]
    fn check_git_finish_message() {
        let url = "http://github.com/url";
        let branch = "master";
        let revision = "2b7a25df823dc7d8f56f8ce7c2d2dac391cea9c2";

        let msg = git_end_turn_message(GitRemoteInfo {
            branch: branch.to_owned(),
            url: url.to_owned(),
            revision: revision.to_owned(),
        })
        .unwrap();
        assert!(msg.contains(url), "Message: {msg} does not contains {url}",);
        assert!(
            msg.contains(branch),
            "Message: '{msg}' does not contain: {branch}",
        );

        let msg = git_end_turn_message(GitRemoteInfo {
            branch: "".to_owned(),
            url: url.to_owned(),
            revision: revision.to_owned(),
        })
        .unwrap();
        assert!(
            msg.contains(revision),
            "Message: '{msg}' does not contain: {revision}",
        );
    }

    fn check_serialization<T>(json: &str, expected_value: T)
    where
        T: DeserializeOwned + Serialize + PartialEq + Debug,
    {
        let json: Value = serde_json::from_str(json).unwrap();
        let deserialized: T = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(deserialized, expected_value);
        let serialized = serde_json::to_value(deserialized).unwrap();
        assert_eq!(json, serialized);
    }

    #[tokio::test]
    async fn adapter_should_consume_all_messages() {
        const MESSAGE_REPETITIONS: usize = 10;

        let downlink = {
            let request = ClientRequest::LoadSessionRequest(LoadSessionRequest::new("1", "/"));
            let mut message = serde_json::to_string(&request).unwrap();
            message.push('\n');

            IoTransport::new(
                Cursor::new(message.repeat(MESSAGE_REPETITIONS).into_bytes()),
                vec![],
            )
        };

        let uplink = {
            let request = ClientRequest::LoadSessionRequest(LoadSessionRequest::new("1", "/"));
            let mut message = serde_json::to_string(&request).unwrap();
            message.push('\n');

            IoTransport::new(
                Cursor::new(message.repeat(MESSAGE_REPETITIONS).into_bytes()),
                vec![],
            )
        };

        let mut adapter = Adapter::new(
            Err("No config".to_string()),
            Box::new(downlink),
            Box::new(uplink),
        );

        let mut i = 0;
        while adapter.handle_next_message().await.unwrap() {
            i += 1;
        }

        assert_eq!(
            i,
            MESSAGE_REPETITIONS * 2,
            "{MESSAGE_REPETITIONS} should be processed from each of the side (agent, client)"
        );
    }

    #[test]
    fn io_transport_recv_is_cancel_safe() {
        use drop_check::BoxFuture;

        let json_line = b"{\"key\":\"value\"}\n";
        let expected: Value = serde_json::from_slice(json_line).unwrap();

        let init = || {
            let reader = Cursor::new(json_line.to_vec()).intersperse_pending();
            IoTransport::new(reader, vec![])
        };

        fn recv(transport: &mut IoTransport) -> BoxFuture<'_, io::Result<Option<Value>>> {
            Box::pin(transport.recv())
        }

        for (_, result) in cancellations(init, recv) {
            assert_eq!(result.unwrap(), Some(expected.clone()));
        }
    }
}
