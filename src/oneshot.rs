use agent_client_protocol::{
    self as acp, AcpAgent, Agent, ConnectionTo, ErrorCode,
    schema::{
        ContentBlock, InitializeRequest, NewSessionRequest, PromptRequest, ProtocolVersion,
        RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
        SessionNotification, SessionUpdate, StopReason, TextContent,
    },
};
use std::env::current_dir;

pub async fn run(cmd: &[&str], prompt: &str) -> Result<(), acp::Error> {
    let agent = AcpAgent::from_args(cmd)?;

    // Run the client — AcpAgent implements ConnectTo, so it serves as the transport
    acp::Client
        .builder()
        .on_receive_notification(
            async move |notification: SessionNotification, _cx| {
                match &notification.update {
                    SessionUpdate::AgentMessageChunk(chunk) => {
                        if let ContentBlock::Text(text_content) = &chunk.content {
                            print!("{}", text_content.text);
                        }
                    }
                    SessionUpdate::AgentThoughtChunk(chunk) => {
                        if let ContentBlock::Text(text_content) = &chunk.content {
                            print!("[thinking {}]", text_content.text)
                        }
                    }
                    _ => {}
                }
                Ok(())
            },
            acp::on_receive_notification!(),
        )
        .on_receive_request(
            async move |_: RequestPermissionRequest, responder, _connection| {
                responder.respond(RequestPermissionResponse::new(
                    RequestPermissionOutcome::Cancelled,
                ))
            },
            acp::on_receive_request!(),
        )
        .connect_with(agent, |connection: ConnectionTo<Agent>| async move {
            connection
                .send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;

            let cwd = current_dir().expect("Unable to read current pwd");
            let new_session_response = connection
                .send_request(NewSessionRequest::new(cwd))
                .block_task()
                .await?;
            let session_id = new_session_response.session_id;

            println!(" > {prompt}");
            println!();
            let prompt_response = connection
                .send_request(PromptRequest::new(
                    session_id.clone(),
                    vec![ContentBlock::Text(TextContent::new(prompt.to_owned()))],
                ))
                .block_task()
                .await?;

            if prompt_response.stop_reason != StopReason::EndTurn {
                Err(acp::Error::new(
                    ErrorCode::InternalError.into(),
                    format!("Stop reason: {:?}", prompt_response.stop_reason),
                ))
            } else {
                Ok(())
            }
        })
        .await
}
