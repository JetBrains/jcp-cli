//! Test harness for integration testing the ACP-JCP adapter.
//!
//! Provides an API for testing the adapter without dealing with
//! websocket setup, channels, and async coordination directly.
//!
//! The harness drives the adapter synchronously, eliminating
//! the need for timeouts and making tests deterministic.

use agent_client_protocol::{
    self as acp, AgentResponse, AgentSide, ClientRequest, InitializeRequest, InitializeResponse,
    JsonRpcMessage, NewSessionRequest, NewSessionResponse, ProtocolVersion, Request, RequestId,
    Response, SessionId, Side,
};
use futures::FutureExt;
use jcp::{Adapter, AgentOutgoingMessage, ClientOutgoingMessage, Config, Transport};
use serde::de::DeserializeOwned;
use serde_json::{Value as JsonValue, value::RawValue};
use std::io;
use tokio::sync::mpsc;

/// Test harness for the ACP-JCP adapter.
///
/// Provides an API for sending messages from the client side,
/// receiving them on the server side, and vice versa.
pub struct TestHarness {
    /// The adapter instance
    adapter: Adapter<ChannelTransport, ChannelTransport>,
    /// Transport endpoint for the client side (simulates IDE)
    client: ChannelTransport,
    /// Transport endpoint for the server side (simulates JCP)
    server: ChannelTransport,
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
            server: uplink_test,
            next_request_id: 1,
            next_session_id: 1,
        }
    }

    /// Process the all enqueued messages in the adapter.
    ///
    /// After this method was called it is safe to assume that all requests were sent to their
    /// conterparties
    fn deliver_transport_messages(&mut self) -> io::Result<()> {
        now_or_panic!(self.adapter.handle_enqueued_messages())
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

        self.deliver_transport_messages().unwrap();

        id
    }

    /// Sends a request from client side and then reply from the server side
    pub fn client_request_and_response(&mut self, request: ClientRequest, response: AgentResponse) {
        let request_id = self.client_send(request);
        self.server_reply(request_id, response);

        // Removing all transport messages from both sides
        while self.client_recv_raw().is_some() {}
        while self.server_recv_raw().is_some() {}
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

    /// Send a raw JSON-RPC message from the client.
    ///
    /// Useful for testing edge cases or notifications.
    #[allow(dead_code)]
    pub fn client_send_raw(&mut self, json: &str) {
        let value: JsonValue = serde_json::from_str(json).unwrap();
        let _ = now_or_panic!(self.client.send(value));

        self.deliver_transport_messages().unwrap();
    }

    /// Receive a request that the adapter forwarded to the server.
    pub fn server_recv_raw(&mut self) -> Option<JsonValue> {
        self.server.try_recv()
    }

    /// Receive a request that the adapter forwarded to the server, parsed as ClientRequest.
    pub fn server_recv_request(&mut self) -> (RequestId, ClientRequest) {
        let value = self
            .server_recv_raw()
            .expect("No message is delivered to a server");

        let id = match &value["id"] {
            JsonValue::Number(n) => RequestId::Number(n.as_i64().unwrap()),
            JsonValue::String(s) => RequestId::Str(s.clone()),
            _ => panic!("invalid request id"),
        };

        let method = value["method"].as_str().expect("missing method");
        let params = value.get("params");

        let request = AgentSide::decode_request(
            method,
            params
                .map(|p| RawValue::from_string(p.to_string()).unwrap())
                .as_deref(),
        )
        .expect("failed to decode request");

        (id, request)
    }

    /// Send a response from the server back to the adapter.
    pub fn server_reply(&mut self, id: RequestId, response: AgentResponse) {
        let msg = JsonRpcMessage::wrap(AgentOutgoingMessage::Response(Response::new(
            id,
            Ok(response),
        )));

        let value = serde_json::to_value(&msg).unwrap();
        let _ = now_or_panic!(self.server.send(value));

        self.deliver_transport_messages().unwrap();
    }

    /// Send a raw JSON response from the server.
    #[allow(dead_code)]
    pub fn server_reply_raw(&mut self, json: &str) {
        let value: JsonValue = serde_json::from_str(json).unwrap();
        let _ = now_or_panic!(self.server.send(value));

        self.deliver_transport_messages().unwrap();
    }

    /// Receive a response that the adapter forwarded to the client.
    ///
    /// Returns the parsed response for assertions.
    pub fn client_recv<T: DeserializeOwned>(&mut self) -> Response<T> {
        let value = self
            .client
            .try_recv()
            .expect("no message available for client");

        serde_json::from_value(value).expect("invalid JSON response")
    }

    pub fn client_recv2(&mut self) -> Option<JRpcMessage> {
        self.client.try_recv().map(JRpcMessage)
    }

    /// Receive a response that the adapter forwarded to the client.
    ///
    /// Returns the parsed response for assertions.
    pub fn client_recv_raw(&mut self) -> Option<JsonValue> {
        self.client.try_recv()
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

pub struct JRpcMessage(pub JsonValue);

impl JRpcMessage {
    pub fn into_notification<T: DeserializeOwned>(mut self) -> io::Result<(String, T)> {
        if self.0["id"] != JsonValue::Null {
            Err(io::Error::other(
                "id field is present. This a request, not a notification",
            ))
        } else {
            let method_name = serde_json::from_value::<String>(self.0["method"].take())?;
            let params = serde_json::from_value::<T>(self.0["params"].take())?;

            Ok((method_name, params))
        }
    }

    pub fn into_request<T: DeserializeOwned>(mut self) -> io::Result<(String, RequestId, T)> {
        let request_id = serde_json::from_value(self.0["id"].take())?;
        let method_name = serde_json::from_value(self.0["method"].take())?;
        let params = serde_json::from_value(self.0["params"].take())?;

        Ok((method_name, request_id, params))
    }

    pub fn into_response<T: DeserializeOwned>(
        mut self,
    ) -> io::Result<(RequestId, Result<T, acp::Error>)> {
        println!("{}", self.0);
        let request_id = serde_json::from_value::<RequestId>(self.0["id"].take())?;
        let result = if self.0["error"] != JsonValue::Null {
            Err(serde_json::from_value(self.0["error"].take())?)
        } else {
            // Assuming result is present
            Ok(serde_json::from_value(self.0["result"].take())?)
        };
        Ok((request_id, result))
    }
}
