//! Test harness for integration testing Adapter.
//!
//! Provides an API for testing without dealing with websocket setup, channels, and async coordination directly.

use agent_client_protocol::{
    self as acp, AgentResponse, ClientRequest, InitializeRequest, InitializeResponse,
    JsonRpcMessage, NewSessionRequest, NewSessionResponse, ProtocolVersion, Request, RequestId,
    Response, SessionId,
};
use futures::FutureExt;
use jcp::{Adapter, AgentOutgoingMessage, ClientOutgoingMessage, Config, Transport};
use serde::de::DeserializeOwned;
use serde_json::Value as JsonValue;
use std::io;
use tokio::sync::mpsc;

/// Test harness for the Adapter.
///
/// Provides an API for sending messages from the client side,
/// receiving them on the agent side, and vice versa.
///
/// The harness drives the adapter synchronously, eliminating
/// the need for timeouts which simplifies tests.
///
/// The main methods to drive conversations between client and agent are:
///
/// - [`TestHarness::client_recv()`]/[`TestHarness::client_send()`]
/// - [`TestHarness::agent_recv()`]/[`TestHarness::agent_reply()`]
///
/// Each method is send a JSON RPC message to the Adapter and makes sure that message is delivered
/// to the counterparty. For example, when [`TestHarness::client_send()`] successfully returns
/// it means that the message or it's derivatives (because Adapter can change/generate new messages)
/// are available for reading using [`TestHarness::agent_recv()`].
pub struct TestHarness {
    /// The adapter instance
    adapter: Adapter<ChannelTransport, ChannelTransport>,
    /// Transport endpoint for the client side (simulates IDE)
    client: ChannelTransport,
    /// Transport endpoint for the agent side (simulates JCP)
    agent: ChannelTransport,
    /// Next request ID for client requests
    next_request_id: u32,
    next_session_id: u32,
}

/// Making sure future completes immedateley on a first poll.
/// It is appropriate in the test context, because we use local mpsc-channels
macro_rules! now_or_panic {
    ($e:expr) => {
        $e.now_or_never()
            .expect("Future should be completed immediately")
    };
}

impl TestHarness {
    /// Bootstrap a new test harness with the given config.
    pub fn new(config: Config) -> Self {
        let (downlink_adapter, downlink_test) = ChannelTransport::pair(10);
        let (uplink_adapter, uplink_test) = ChannelTransport::pair(10);

        let adapter = Adapter::new(Ok(config), downlink_adapter, uplink_adapter);

        Self {
            adapter,
            client: downlink_test,
            agent: uplink_test,
            next_request_id: 1,
            next_session_id: 1,
        }
    }

    /// Send a request from the client to the adapter.
    ///
    /// This simulates a client (IDE) sending a JSON-RPC request via stdin.
    pub fn client_send(&mut self, request: ClientRequest) -> RequestId {
        let id = RequestId::Number(self.next_request_id as i64);
        self.next_request_id += 1;

        let msg = JsonRpcMessage::wrap(ClientOutgoingMessage::Request(Request {
            id: id.clone(),
            method: request.method().to_string().into(),
            params: Some(request),
        }));

        let value = serde_json::to_value(&msg).unwrap();
        let _ = now_or_panic!(self.client.send(value));

        self.deliver_transport_messages();

        id
    }

    /// Sends a request from client side and then reply from the agent side
    ///
    /// All pending messages will be removed from both transports, so that system will be in a ready state
    /// for new interactions to test
    pub fn client_request_and_response(&mut self, request: ClientRequest, response: AgentResponse) {
        let request_id = self.client_send(request);
        self.agent_reply(request_id, response);

        // Removing all transport messages from both sides
        while self.try_client_recv().is_some() {}
        while self.try_agent_recv().is_some() {}
    }

    pub fn initialize(&mut self) {
        self.client_request_and_response(
            ClientRequest::InitializeRequest(InitializeRequest::new(1.into())),
            AgentResponse::InitializeResponse(InitializeResponse::new(ProtocolVersion::V1)),
        );
    }

    pub fn new_session(&mut self) -> SessionId {
        let session_id = SessionId::new(format!("session-id-{}", self.next_session_id));
        self.next_session_id += 1;
        self.client_request_and_response(
            ClientRequest::NewSessionRequest(NewSessionRequest::new("/test")),
            AgentResponse::NewSessionResponse(NewSessionResponse::new(session_id.clone())),
        );
        session_id
    }

    pub fn try_agent_recv(&mut self) -> Option<JRpcMessage> {
        self.agent.try_recv().map(JRpcMessage)
    }

    /// Reads a pending message on Agent side
    pub fn agent_recv(&mut self) -> JRpcMessage {
        self.try_agent_recv()
            .expect("No pending messages are available on the agent")
    }

    /// Send a response from the Agent back to the Adapter.
    pub fn agent_reply(&mut self, id: RequestId, response: AgentResponse) {
        let msg = JsonRpcMessage::wrap(AgentOutgoingMessage::Response(Response::new(
            id,
            Ok(response),
        )));

        let value = serde_json::to_value(&msg).unwrap();
        let _ = now_or_panic!(self.agent.send(value));

        self.deliver_transport_messages();
    }

    /// Reads a pending message on Client side
    pub fn client_recv(&mut self) -> JRpcMessage {
        self.try_client_recv()
            .expect("No pending messages are available on the client")
    }

    pub fn try_client_recv(&mut self) -> Option<JRpcMessage> {
        self.client.try_recv().map(JRpcMessage)
    }

    /// Process the all enqueued messages in the adapter.
    ///
    /// After this method was called it is safe to assume that all requests were sent to their
    /// conterparties
    fn deliver_transport_messages(&mut self) {
        now_or_panic!(self.adapter.handle_enqueued_messages()).unwrap()
    }
}

pub struct ChannelTransport {
    rx: mpsc::Receiver<JsonValue>,
    tx: mpsc::Sender<JsonValue>,
}

impl ChannelTransport {
    pub fn new(rx: mpsc::Receiver<JsonValue>, tx: mpsc::Sender<JsonValue>) -> Self {
        Self { rx, tx }
    }

    /// Create a pair of connected transports.
    ///
    /// Returns `(a, b)` where messages sent on `a` are received on `b` and vice versa.
    pub fn pair(buffer: usize) -> (Self, Self) {
        let (tx_a, rx_a) = mpsc::channel(buffer);
        let (tx_b, rx_b) = mpsc::channel(buffer);
        (Self::new(rx_a, tx_b), Self::new(rx_b, tx_a))
    }

    /// Try to receive a message without blocking.
    ///
    /// Returns `Some(msg)` if a message is available, `None` otherwise.
    pub fn try_recv(&mut self) -> Option<JsonValue> {
        self.rx.try_recv().ok()
    }
}

impl Transport for ChannelTransport {
    async fn recv(&mut self) -> io::Result<Option<JsonValue>> {
        Ok(self.rx.recv().await)
    }

    async fn send(&mut self, msg: JsonValue) -> io::Result<()> {
        self.tx.send(msg).await.map_err(io::Error::other)
    }
}

/// This is a simple wrapper around json value that simplifies tests by
/// providing typical conversion and expectaction method for JSON RPC messages.
pub struct JRpcMessage(pub JsonValue);

impl JRpcMessage {
    pub fn expect_notification<T: DeserializeOwned>(self) -> (String, T) {
        assert!(
            self.0["id"] == JsonValue::Null,
            "Expected notification (no id), got request: {}",
            self.0
        );
        (self.read_field("method"), self.read_field("params"))
    }

    pub fn expect_request<T: DeserializeOwned>(self) -> (String, RequestId, T) {
        assert!(
            self.0["id"] != JsonValue::Null,
            "Expected request, but no id is present on request: {}",
            self.0
        );
        let request_id = self.read_field("id");
        let method_name = self.read_field("method");
        let params = self.read_field("params");
        (method_name, request_id, params)
    }

    pub fn expect_response<T: DeserializeOwned>(self) -> (RequestId, Result<T, acp::Error>) {
        let request_id = self.read_field("id");
        let result = if self.0["error"] != JsonValue::Null {
            Err(self.read_field("error"))
        } else {
            Ok(self.read_field("result"))
        };
        (request_id, result)
    }

    fn read_field<T: DeserializeOwned>(&self, field_name: &str) -> T {
        serde_json::from_value(self.0[field_name].clone())
            .unwrap_or_else(|e| panic!("{e}\nJSON: {}", self.0))
    }
}

mod harness_test {
    use crate::harness::JRpcMessage;
    use agent_client_protocol::{self as acp, RequestId};
    use serde_json::json;

    #[test]
    fn jprc_into_notification() {
        let msg = JRpcMessage(json! { {"jsonrpc": "2.0", "method": "foo", "params": [0, 1]} });
        let (method, param) = msg.expect_notification::<Vec<u32>>();
        assert_eq!(method, "foo");
        assert_eq!(param, vec![0, 1]);
    }

    #[test]
    #[should_panic = "Expected notification (no id), got request"]
    fn jprc_into_notification_for_request() {
        let msg = JRpcMessage(json! { {"jsonrpc": "2.0", "id": 5, "method": "foo", "result": 3} });
        msg.expect_notification::<u32>();
    }

    #[test]
    fn jprc_into_request() {
        let msg =
            JRpcMessage(json! { {"jsonrpc": "2.0", "id": 5, "method": "foo", "params": [0, 1]} });
        let (method, id, params) = msg.expect_request::<Vec<u32>>();
        assert_eq!(method, "foo");
        assert_eq!(params, vec![0, 1]);
        assert_eq!(id, RequestId::Number(5));
    }

    #[test]
    fn jprc_into_response_ok() {
        let msg = JRpcMessage(json! { {"jsonrpc": "2.0", "id": 5, "result": 42} });
        let result = msg.expect_response::<u32>();
        assert_eq!(result, (RequestId::Number(5), Ok(42)));
    }

    #[test]
    fn jprc_into_response_err() {
        let msg = JRpcMessage(
            json! { {"jsonrpc": "2.0", "id": 5, "error": {"code": -32600, "message": "Invalid Request"}} },
        );
        let result = msg.expect_response::<u32>();
        assert_eq!(
            result,
            (
                RequestId::Number(5),
                Err(acp::Error::new(-32600, "Invalid Request"))
            )
        );
    }
}
