use agent_client_protocol::{AgentSide, ClientRequest, ErrorCode, RequestId, Side};
use clap::{Parser, Subcommand};
use dotenv::dotenv;
use futures_util::StreamExt;
use jcp::{
    Adapter, GitCommandTool, IoTransport, RawIncomingMessage, TrafficLog, Transport,
    WebSocketTransport,
    auth::{self, AccessTokens, get_access_tokens, login},
    create_json_rpc_error,
    keychain::{self, SecretBackend},
    request_id, to_io_invalid_data_err,
};
use serde_json::Value as JsonValue;
use std::{env, io, process};
use thiserror::Error;
use tokio::io::{stdin, stdout};
use tokio::runtime::Runtime;
use tokio_tungstenite::connect_async;
use tungstenite::client::IntoClientRequest;

const DEFAULT_JCP_URL: &str = "wss://api.stgn.jetbrains.cloud/agent-spawner/acp";

#[derive(Parser)]
#[command(name = "jcp", version)]
#[command(about = "ACP-JCP adapter for JetBrains Cloud Platform")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Authenticate via browser and store refresh token in keychain
    Login,

    /// Discard local refresh token
    Logout,

    /// Run ACP adapter
    Acp,
}

#[derive(Error, Debug)]
enum Error {
    #[error("Authentication error: {0}")]
    UnableToGetAccessToken(#[from] auth::AuthError),

    #[error("IO error: {0}")]
    Io(#[from] io::Error),

    #[error("No refresh token found")]
    NoRefreshToken,

    #[error("WebSocket failed: {0}")]
    WebSocket(#[from] tungstenite::Error),

    #[error("Invalid ACP message: {0}")]
    InvalidAcpMessage(#[from] agent_client_protocol::Error),
}

fn main() {
    // We don't want to fail if we can't read .env for whatever reason
    let _ = dotenv();

    let cli = Cli::parse();
    let keychain = keychain::active_keychain();

    match cli.command {
        Commands::Login => {
            eprintln!("Starting authentication...");
            match login() {
                Ok(refresh_token) => {
                    if let Err(e) = keychain.store_refresh_token(&refresh_token) {
                        eprintln!("Failed to store refresh token in keychain: {}", e);
                        process::exit(1);
                    }
                    eprintln!("Login successful!");
                }
                Err(e) => {
                    eprintln!("Login failed: {}", e);
                    process::exit(1);
                }
            }
        }
        Commands::Logout => {
            keychain.delete_refresh_token().unwrap();
            eprintln!("Logout successful!");
        }
        Commands::Acp => run_adapter(&*keychain),
    }
}

fn run_adapter(keychain: &dyn SecretBackend) {
    let jcp_url = env::var("JCP_URL").ok().unwrap_or(DEFAULT_JCP_URL.into());

    let runtime = Runtime::new().expect("Failed to create Tokio runtime");
    runtime.block_on(async {
        let traffic_log = TrafficLog::new(env::var("TRAFFIC_LOG").ok()).await;

        let mut client = IoTransport::new(stdin(), stdout());

        // This code is rather tricky.
        //
        // We generally are not intrested in client transport errors, because if client transport failed
        // we don't have any other option but panic. But in case of any other error we need to properly
        // report error as an JSON RPC error to a client, because this is how IDE will know something
        // goes wrong and properly show error message to an end user.

        // Read the first message from the client, which must be an InitializeRequest
        let init_msg = client
            .recv()
            .await
            .expect("Unable to read message")
            .expect("Unexpected EOF");
        let request_id = request_id(&init_msg).unwrap_or(RequestId::Null);

        match handshake_and_authenticate(&mut client, init_msg, keychain, &jcp_url).await {
            Ok((uplink, tokens)) => {
                // Run the adapter for the remainder of the session
                let mut adapter = Adapter::new(
                    Box::new(client),
                    Box::new(uplink),
                    Box::new(GitCommandTool),
                    tokens.ai_access_token,
                );
                match traffic_log {
                    Ok(log) => adapter.set_traffic_log(log),
                    Err(e) => eprintln!("Unable to create traffic log: {e}"),
                }
                adapter.run().await.expect("Unable to handle message");
            }
            Err(e) => {
                // Report the initialization failure to the client as a JSON-RPC error
                // Error reporting is happening via JSON RPC channel, we can not reply on IDE monitoring stderr
                if let Ok(err) =
                    create_json_rpc_error(ErrorCode::InvalidRequest, e.to_string(), request_id)
                {
                    let _ = client.send(err).await;
                }
                panic!("{e}");
            }
        }
    });
}

/// Authenticates and opens a WebSocket connection to the JCP uplink.
///
/// Returns the connected [`WebSocketTransport`], and a resolved [`AccessTokens`] on success,
/// or an error message string that can be forwarded to the client.
async fn handshake_and_authenticate(
    client: &mut dyn Transport,
    initialize_request: JsonValue,
    keychain: &dyn SecretBackend,
    jcp_url: &str,
) -> Result<(WebSocketTransport, AccessTokens), Error> {
    // Checking that the message of a proper type
    let ClientRequest::InitializeRequest(_) = decode_acp_request::<AgentSide>(&initialize_request)?
    else {
        return Err(io::Error::other("InititializeRequest expected").into());
    };

    let tokens = authenticate(keychain)?;

    let mut request = jcp_url
        .into_client_request()
        .map_err(to_io_invalid_data_err)?;
    request.headers_mut().insert(
        "Authorization",
        format!("Bearer {}", tokens.jcp_access_token)
            .parse()
            .map_err(to_io_invalid_data_err)?,
    );

    let (ws_stream, _) = connect_async(request).await?;
    let (ws_tx, ws_rx) = ws_stream.split();

    let mut agent = WebSocketTransport::new(ws_rx, ws_tx);

    // Forward InitializeRequest to the uplink server
    agent.send(initialize_request).await?;

    let init_response = agent.recv().await?.ok_or(io::Error::new(
        io::ErrorKind::UnexpectedEof,
        "Agent reset connection",
    ))?;
    // Forwarding InitializeResponse from an agent to a client. We're assuming this is reply to
    // InitializationRequest, but it might be not a successful result, but a JSON RPC error, hence we do not
    // deserialize it here
    client
        .send(init_response)
        .await
        // If client transport has failed we don't have any other option but panic
        .expect("Unable to send response");

    Ok((agent, tokens))
}

fn decode_acp_request<T: Side>(json_rpc: &JsonValue) -> Result<T::InRequest, Error> {
    let msg_str = json_rpc.to_string();
    let rpc_msg: RawIncomingMessage<'_> = serde_json::from_str(&msg_str).unwrap();
    let method = rpc_msg.method.ok_or(io::Error::new(
        io::ErrorKind::UnexpectedEof,
        "Not method name is provided",
    ))?;
    Ok(T::decode_request(method, rpc_msg.params)?)
}

/// Retrieves access tokens.
///
/// If both `AI_PLATFORM_TOKEN` and `JCP_ACCESS_TOKEN` are present, then they are used.
/// If not, the refresh token is retrieved from the keychain and fresh access tokens are requested.
/// `AI_PLATFORM_TOKEN` and `JCP_ACCESS_TOKEN` env variables still allow overriding respective tokens.
fn authenticate(keychain: &dyn SecretBackend) -> Result<AccessTokens, Error> {
    let jb_ai = env::var("AI_PLATFORM_TOKEN").ok();
    let jcp = env::var("JCP_ACCESS_TOKEN").ok();

    let access_tokens = if let Some((jb_ai_token, jcp_token)) = jb_ai.as_ref().zip(jcp.as_ref()) {
        AccessTokens {
            jcp_access_token: jcp_token.to_string(),
            ai_access_token: jb_ai_token.to_string(),
        }
    } else {
        let refresh_token = keychain.get_refresh_token()?.ok_or(Error::NoRefreshToken)?;
        let access_tokens = get_access_tokens(&refresh_token)?;
        AccessTokens {
            jcp_access_token: jcp.unwrap_or(access_tokens.jcp_access_token),
            ai_access_token: jb_ai.unwrap_or(access_tokens.ai_access_token),
        }
    };
    Ok(access_tokens)
}
