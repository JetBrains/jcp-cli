mod harness;

use acp::{
    AgentNotification, AgentResponse, ClientNotification, ClientRequest, ContentBlock,
    InitializeRequest, InitializeResponse, JsonRpcMessage, Meta, NewSessionRequest, PromptRequest,
    PromptResponse, ProtocolVersion, Response, SessionNotification, StopReason, TextContent,
};
use agent_client_protocol::{self as acp, CLIENT_METHOD_NAMES, SessionUpdate};
use harness::TestHarness;
use jcp::{Config, EndTurnMeta, GitRemoteInfo, NewSessionMeta};
use serde_json::Value;

const TEST_GIT_URL: &str = "https://github.com/test/repo.git";
const TEST_BRANCH: &str = "main";
const TEST_REVISION: &str = "abc123";
const TEST_TOKEN: &str = "test-token";

fn test_config() -> Config {
    Config {
        git_url: TEST_GIT_URL.into(),
        branch: TEST_BRANCH.into(),
        revision: TEST_REVISION.into(),
        ai_platform_token: TEST_TOKEN.into(),
        supports_user_git_auth_flow: false,
    }
}

#[tokio::test]
async fn test_adapter_forwards_initialize_request_to_server() {
    let mut harness = TestHarness::new(test_config());

    // Client sends initialize request
    let request = ClientRequest::InitializeRequest(InitializeRequest::new(1.into()));
    let request_id = harness.client_send(request);

    // Server receives the forwarded request (no timeout needed)
    let (recv_id, recv_request) = harness.server_recv_request();
    assert_eq!(recv_id, request_id);
    assert!(matches!(recv_request, ClientRequest::InitializeRequest(_)));

    let initialize_response = InitializeResponse::new(ProtocolVersion::V1);
    // Server sends response
    let response = AgentResponse::InitializeResponse(initialize_response.clone());
    harness.server_reply(recv_id, response);

    // Client receives the response (no timeout needed)
    let result = harness.client_recv::<InitializeResponse>();
    let Response::Result { id, result } = result else {
        panic!("expected InitializeResponse, got {:?}", result);
    };

    assert_eq!(id, request_id);
    assert_eq!(result, initialize_response);
}

#[tokio::test]
async fn test_adapter_injects_meta_into_new_session_request() {
    let config = test_config();
    let expected_meta = config.new_session_meta();
    let mut harness = TestHarness::new(config);

    // Client sends newSession request (without meta)
    harness.client_send(ClientRequest::NewSessionRequest(NewSessionRequest::new(
        "/test",
    )));

    // Server receives the request with injected meta (no timeout needed)
    let (_, received) = harness.server_recv_request();
    let ClientRequest::NewSessionRequest(r) = received else {
        panic!("expected NewSessionRequest, got {:?}", received);
    };

    // Verify the meta was injected by deserializing it
    let meta = r
        .meta
        .map(|m| serde_json::from_value::<NewSessionMeta>(serde_json::Value::Object(m)))
        .transpose()
        .expect("meta should be valid");

    assert_eq!(meta, Some(expected_meta));
}

#[tokio::test]
async fn adapter_need_to_inject_chunk_with_git_info() {
    let mut harness = TestHarness::new(test_config());

    harness.initialize();
    let session_id = harness.new_session();

    let request_id = harness.client_send(ClientRequest::PromptRequest(PromptRequest::new(
        session_id,
        vec![ContentBlock::Text(TextContent::new("Test prompt"))],
    )));

    let branch_name = "main";
    let git_url = "http://github.com/user/repo";
    let meta = EndTurnMeta {
        target: GitRemoteInfo {
            branch: branch_name.into(),
            url: git_url.into(),
            revision: "".into(),
        },
    };

    harness.server_reply(
        request_id,
        AgentResponse::PromptResponse(prompt_response_with_git_meta(meta)),
    );

    let (method_name, notification) = harness
        .client_recv2()
        .unwrap()
        .into_notification::<SessionNotification>()
        .unwrap();
    assert_eq!(method_name, CLIENT_METHOD_NAMES.session_update);
    if let SessionUpdate::AgentMessageChunk(chunk) = &notification.update
        && let ContentBlock::Text(content) = &chunk.content
    {
        assert!(
            content.text.contains(git_url),
            "Message should contain '{git_url}', got: {}",
            content.text
        );
        assert!(
            content.text.contains(branch_name),
            "Message should contain '{branch_name}', got: {}",
            content.text
        );
    } else {
        panic!("Exected agent message text chunk, got: {notification:?}")
    }

    let (_, response) = harness
        .client_recv2()
        .unwrap()
        .into_response::<PromptResponse>()
        .unwrap();

    println!("Agent response: {response:?}");
}

fn prompt_response_with_git_meta(meta: EndTurnMeta) -> PromptResponse {
    let Value::Object(json_meta) = serde_json::to_value(meta).unwrap() else {
        panic!("Unexpected json type")
    };
    PromptResponse::new(StopReason::EndTurn).meta(json_meta)
}

mod harness_tests {
    use super::*;
    use crate::harness::JRpcMessage;
    use acp::RequestId;
    use serde_json::json;

    #[test]
    fn into_notification() {
        let msg = JRpcMessage(json! { {"jsonrpc": "2.0", "method": "foo", "params": [0, 1]} });
        let (method, param) = msg.into_notification::<Vec<u32>>().unwrap();
        assert_eq!(method, "foo");
        assert_eq!(param, vec![0, 1]);
    }

    #[test]
    #[should_panic = "id field is present"]
    fn into_notification_for_request() {
        let msg = JRpcMessage(json! { {"jsonrpc": "2.0", "id": 5, "method": "foo", "result": 3} });
        msg.into_notification::<u32>().unwrap();
    }

    #[test]
    fn into_request() {
        let msg =
            JRpcMessage(json! { {"jsonrpc": "2.0", "id": 5, "method": "foo", "params": [0, 1]} });
        let (method, id, params) = msg.into_request::<Vec<u32>>().unwrap();
        assert_eq!(method, "foo");
        assert_eq!(params, vec![0, 1]);
        assert_eq!(id, RequestId::Number(5));
    }

    #[test]
    fn into_response_ok() {
        let msg = JRpcMessage(json! { {"jsonrpc": "2.0", "id": 5, "result": 42} });
        let result = msg.into_response::<u32>().unwrap();
        assert_eq!(result, (RequestId::Number(5), Ok(42)));
    }

    #[test]
    fn into_response_err() {
        let msg = JRpcMessage(
            json! { {"jsonrpc": "2.0", "id": 5, "error": {"code": -32600, "message": "Invalid Request"}} },
        );
        let result = msg.into_response::<u32>().unwrap();
        assert_eq!(
            result,
            (
                RequestId::Number(5),
                Err(acp::Error::new(-32600, "Invalid Request"))
            )
        );
    }
}
