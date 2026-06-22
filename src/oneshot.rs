use agent_client_protocol::{
    self as acp, AcpAgent, Agent, ConnectionTo, ErrorCode,
    schema::{
        ContentBlock, InitializeRequest, NewSessionRequest, PromptRequest, ProtocolVersion,
        RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
        SessionNotification, SessionUpdate, StopReason, TextContent,
    },
};
use std::{
    env::current_dir,
    io::{Write, stdout},
    sync::{Arc, Mutex},
};
use terminal_size::terminal_size;

pub async fn run(cmd: &[&str], prompt: &str) -> Result<(), acp::Error> {
    let agent = AcpAgent::from_args(cmd)?;

    let terminal_width = terminal_size().map(|(width, _)| width.0).unwrap_or(120) as usize;

    let printer = Arc::new(Mutex::new(ConversationPrinter::new(
        terminal_width,
        stdout(),
    )));

    // Clones for different closures. Need to share it
    let p1 = Arc::clone(&printer);
    let p2 = Arc::clone(&printer);

    // Run the client — AcpAgent implements ConnectTo, so it serves as the transport
    acp::Client
        .builder()
        .on_receive_notification(
            async move |notification: SessionNotification, _cx| {
                let mut printer = p1.lock().unwrap();
                match &notification.update {
                    SessionUpdate::AgentMessageChunk(chunk) => {
                        if let ContentBlock::Text(text_block) = &chunk.content {
                            printer.print(ChunkType::Agent, &text_block.text);
                        }
                    }
                    SessionUpdate::AgentThoughtChunk(chunk) => {
                        if let ContentBlock::Text(text_block) = &chunk.content {
                            printer.print(ChunkType::Thought, &text_block.text);
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

            {
                let mut printer = p2.lock().unwrap();
                printer.print(ChunkType::User, prompt);
            }

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

/// Simple text wrapper that accept Message type: agent, user, though (see: [`ChunkType`]).
///
/// Writes a text like that to a provided [`Write`]:
/// ```ignore
///   user   | Please generate lorem ipsum
///
///   agent  | Lorem Ipsum is simply dummy text of the printing
///          | and typesetting industry. Lorem Ipsum has been the
///          | industry's standard dummy text ever since 1966, when
///          | designers at Letraset and James Mosley, the librarian
///          | at St Bride Printing Library in London.
/// ```
///
/// Handles change in message types (indicated with header), terminal width and newlines
struct ConversationPrinter<W: Write> {
    writer: W,
    terminal_width: usize,
    already_printed: usize,
    last_type: Option<ChunkType>,
}

impl<W: Write> ConversationPrinter<W> {
    pub fn new(terminal_width: usize, writer: W) -> Self {
        // compensating for the header width
        let header_size = Self::format_header("").chars().count();
        let terminal_width = terminal_width.saturating_sub(header_size);

        Self {
            terminal_width,
            writer,
            already_printed: 0,
            last_type: None,
        }
    }

    pub fn print(&mut self, ty: ChunkType, s: &str) {
        if self.last_type != Some(ty) {
            self.already_printed = 0;
            self.print_header(&ty);
        };

        let mut lines = s.lines();
        self.print_line(lines.next().unwrap());
        for chunk in lines {
            self.print_empty_header();
            self.already_printed = 0;
            self.print_line(chunk);
        }

        self.last_type = Some(ty);
    }

    fn print_line(&mut self, mut line: &str) {
        while !line.is_empty() {
            if self.already_printed >= self.terminal_width {
                self.print_empty_header();
                self.already_printed = 0;
            }

            let length_to_print = line
                .chars()
                .count()
                .min(self.terminal_width - self.already_printed);
            let line_tail = line.chars().take(length_to_print).collect::<String>();
            let _ = self.writer.write(line_tail.as_bytes());
            let _ = self.writer.flush();
            self.already_printed += length_to_print;
            line = &line[line_tail.len()..];
        }
    }

    fn print_header(&mut self, ty: &ChunkType) {
        let _ = self.writer.write(b"\n\n");
        let _ = self
            .writer
            .write(Self::format_header(ty.as_str()).as_bytes());
    }

    fn print_empty_header(&mut self) {
        let _ = self.writer.write(b"\n");
        let _ = self.writer.write(Self::format_header("").as_bytes());
    }

    fn format_header(name: &str) -> String {
        format!(" {:>10} ▎", name)
    }
}

#[derive(PartialEq, Debug, Copy, Clone)]
enum ChunkType {
    User,
    Agent,
    Thought,
}

impl ChunkType {
    fn as_str(&self) -> &str {
        match self {
            ChunkType::User => "user",
            ChunkType::Agent => "agent",
            ChunkType::Thought => "thought",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Renders given events to a string returning Vec of lines with every line trimmed for convenience of testing
    fn render(width: usize, chunks: &[(ChunkType, &str)]) -> Vec<String> {
        let mut printer = ConversationPrinter::new(width, Cursor::new(Vec::new()));
        for (ty, s) in chunks {
            printer.print(*ty, s);
        }
        String::from_utf8(printer.writer.into_inner())
            .unwrap()
            .lines()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn prints_header_with_right_aligned_type() {
        let out = render(120, &[(ChunkType::User, "hello")]);
        assert_eq!(out, vec!["user ▎hello"]);
    }

    #[test]
    fn prints_new_header_only_when_type_changes() {
        let out = render(
            120,
            &[
                (ChunkType::Agent, "foo"),
                (ChunkType::Agent, " bar"),
                (ChunkType::Thought, "baz"),
            ],
        );
        // Same type is printed without a new header, type change inserts one
        assert_eq!(out, vec!["agent ▎foo bar", "thought ▎baz"]);
    }

    #[test]
    fn wraps_lines_at_terminal_width() {
        let out = render(16, &[(ChunkType::Agent, "abcdef")]);
        assert_eq!(out, vec!["agent ▎abc", "▎def"]);
    }

    #[test]
    fn wraps_lines_correctly_when_chunk_type_has_changed() {
        let out = render(
            16,
            &[(ChunkType::Agent, "abc"), (ChunkType::Thought, "def")],
        );
        assert_eq!(out, vec!["agent ▎abc", "thought ▎def"]);
    }

    #[test]
    fn splits_input_on_newlines() {
        let out = render(120, &[(ChunkType::User, "first\nsecond")]);
        assert_eq!(out, vec!["user ▎first", "▎second"]);
    }
}
