use agent_client_protocol::{
    self as acp, ErrorCode,
    schema::{InitializeRequest, RequestId, rpc},
};
use clap::{Parser, Subcommand};
use dotenv::dotenv;
use jcp::{
    Adapter, EnvConfig, GitCommandTool, IoTransport, TrafficLog, Transport, WebSocketTransport,
    auth::{self, AccessTokens, get_access_token, login},
    decode_acp, decode_jrpc,
    keychain::{self, AI_PLATFORM_TOKEN_ENV_NAME, JCP_ACCESS_TOKEN_ENV_NAME, SecretBackend},
    oneshot, request_id,
};
use serde_json::Value as JsonValue;
use std::{env, io, process};
use thiserror::Error;
use tokio::{
    io::{stdin, stdout},
    runtime::Runtime,
    task::{JoinError, spawn_blocking},
};
use tungstenite::client::IntoClientRequest;

#[derive(Parser)]
#[command(name = "jcp", version)]
#[command(about = "ACP-JCP adapter for JetBrains Cloud Platform")]
struct Cli {
    /// Use staging environment instead of production
    #[arg(long = "staging", hide = true)]
    staging: bool,

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

    #[command(hide = true)]
    /// Oneshot prompt. This is for development/testing purposes only
    OneShot { prompt: String },
}

fn main() {
    // We don't want to fail if we can't read .env for whatever reason
    let _ = dotenv();

    let opts = Cli::parse();
    let keychain = keychain::active_keychain();

    let env_config = read_env_config(&opts);

    match &opts.command {
        Commands::Login => {
            eprintln!("Starting authentication...");
            match login(&env_config) {
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
        Commands::Acp => run_adapter(keychain, &env_config),
        Commands::OneShot { prompt } => {
            if let Err(e) = run_one_shot(&opts, prompt) {
                eprintln!("Agent failed: {}", e);
                process::exit(1);
            }
        }
    }
}

fn run_one_shot(opts: &Cli, prompt: &str) -> Result<(), acp::Error> {
    let program_path = env::current_exe().expect("Failed to get executable path");
    let program_path = program_path.to_str().unwrap();

    // To run agent we call ourselves with `acp` subcommand
    let args: &[&str] = if opts.staging {
        &[program_path, "--staging", "acp"]
    } else {
        &[program_path, "acp"]
    };

    let runtime = Runtime::new().expect("Failed to create Tokio runtime");
    runtime.block_on(async { oneshot::run(args, prompt).await })
}

fn run_adapter(keychain: Box<dyn SecretBackend>, env_config: &EnvConfig) {
    let runtime = Runtime::new().expect("Failed to create Tokio runtime");
    runtime.block_on(async {
        let traffic_log = TrafficLog::new(env::var("TRAFFIC_LOG").ok()).await;

        let mut client = IoTransport::new(stdin(), stdout());

        let ctrl_c = tokio::signal::ctrl_c();

        // This code is rather tricky.
        //
        // We generally are not interested in client transport errors, because if client transport failed
        // we don't have any other option but panic. But in case of any other error we need to properly
        // report error as an JSON RPC error to a client, because this is how IDE will know something
        // went wrong and properly show error message to an end user.

        // Read the first message from the client in order to save request_id which we need to
        // properly report errors if they will happen
        let init_msg = client
            .recv()
            .await
            .expect("Unable to read message")
            .expect("Unexpected EOF while reading InitializationRequest");
        let request_id = request_id(&init_msg).unwrap_or(RequestId::Null);

        match handshake_and_authenticate(&mut client, init_msg, keychain, env_config).await {
            Ok((uplink, tokens)) => {
                // Run the adapter for the remainder of the session
                let mut adapter =
                    Adapter::new(Box::new(client), Box::new(uplink), Box::new(GitCommandTool));
                adapter.set_ai_platform_token(tokens.ai_access_token);
                match traffic_log {
                    Ok(log) => adapter.set_traffic_log(log),
                    Err(e) => eprintln!("Unable to create traffic log: {e}"),
                }

                tokio::select! {
                    r = adapter.run() => r.expect("Unable to handle message"),
                    _ = ctrl_c => {  }
                };
                let _ = adapter.shutdown().await;
            }
            Err(e) => {
                // Report the initialization failure back to the client
                // Error reporting is happening via JSON RPC channel, we can't rely on IDE monitoring stderr,
                // but for the sake of convenience we report error to both channels
                match create_json_rpc_error(&e, request_id) {
                    Ok(err) => {
                        let _ = client.send(err).await.ok();
                    }
                    Err(e) => eprintln!("Unable to send JSON RPC error: {e}"),
                }
                panic!("{e}");
            }
        }
    });
}

/// Authenticates and opens a WebSocket connection to the JCP uplink.
///
/// Returns the connected [`WebSocketTransport`], and a resolved [`AccessTokens`] on success,
/// or an error that MUST be forwarded to the client.
///
/// After this method returned both transport considered ready and can be passed to an adapter
async fn handshake_and_authenticate(
    client: &mut dyn Transport,
    initialize_request: JsonValue,
    keychain: Box<dyn SecretBackend>,
    env_config: &EnvConfig,
) -> Result<(WebSocketTransport, AccessTokens), Error> {
    // Checking that this is indeed InitializeRequest
    let Some(_) = decode_jrpc(initialize_request.clone())
        .and_then(|jrpc| decode_acp::<InitializeRequest>(&jrpc))?
    else {
        return Err(io::Error::other("InitializeRequest expected").into());
    };

    // We can't call `authenticate()` synchronously here, because blocking reqwest implementation is
    // using tokio under the hood.
    let e = env_config.clone();
    let tokens = spawn_blocking(move || authenticate(&*keychain, &e)).await??;

    let mut request = env_config
        .agent_spawner_ws_url
        .clone()
        .into_client_request()
        .map_err(|e| Error::InvalidUrl(env_config.agent_spawner_ws_url.clone(), e))?;
    request.headers_mut().insert(
        "Authorization",
        format!("Bearer {}", tokens.jcp_access_token)
            .parse()
            .map_err(|_| {
                // Intentionally masking original error here, to prevent any possible secret leak
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Illegal token value. Only ASCII characters are allowed",
                )
            })?,
    );

    // Establishing WebSocket connection
    let (ws_stream, _) = tokio_tungstenite::connect_async(request).await?;
    let mut agent = WebSocketTransport::new(ws_stream);

    // Forward InitializeRequest to the uplink server
    agent.send(initialize_request).await?;

    let init_response = agent.recv().await?.ok_or(io::Error::new(
        io::ErrorKind::UnexpectedEof,
        "Agent reset connection",
    ))?;
    // Forwarding `InitializeResponse` from an agent to a client. We're assuming this is reply to
    // `InitializeRequest`. It might be not a successful result, but a JSON RPC error, hence we do not
    // deserialize it here
    client
        .send(init_response)
        .await
        // If client transport has failed we don't have any other option but panic
        .expect("Unable to send response");

    Ok((agent, tokens))
}

/// Retrieves access tokens
///
/// If both env-variables with access tokens are present, then they are used (see [`AI_PLATFORM_TOKEN_ENV_NAME`],
/// [`JCP_ACCESS_TOKEN_ENV_NAME`]). If not, the refresh token is retrieved
/// from the keychain and fresh access tokens are requested.
/// Env variables still allow overriding respective tokens.
fn authenticate(
    keychain: &dyn SecretBackend,
    env_config: &EnvConfig,
) -> Result<AccessTokens, Error> {
    let jb_ai = env::var(AI_PLATFORM_TOKEN_ENV_NAME).ok();
    let jcp = env::var(JCP_ACCESS_TOKEN_ENV_NAME).ok();

    let jcp_access_token = if let Some(jcp_token) = jcp {
        jcp_token
    } else {
        let refresh_token = keychain.get_refresh_token()?.ok_or(Error::NoRefreshToken)?;
        get_access_token(&refresh_token, env_config)?
    };
    Ok(AccessTokens {
        jcp_access_token,
        ai_access_token: jb_ai,
    })
}

fn read_env_config(cli: &Cli) -> EnvConfig {
    if cli.staging {
        EnvConfig::staging()
    } else {
        let env_var = env::var("JCP_ENVIRONMENT").ok();
        match env_var.as_deref().unwrap_or_default() {
            "" | "production" => EnvConfig::production(),
            "staging" => EnvConfig::staging(),
            name => {
                eprintln!("Unknown environment name: {name}. Production will be used instead.");
                EnvConfig::production()
            }
        }
    }
}

#[derive(Error, Debug)]
pub enum Error {
    #[error("Authentication error: {0}")]
    UnableToGetAccessToken(#[from] auth::AuthError),

    #[error("IO error: {0}")]
    Io(#[from] io::Error),

    #[error("No refresh token found")]
    NoRefreshToken,

    #[error("WebSocket failed: {0}")]
    WebSocket(#[from] tungstenite::Error),

    #[error("Failed to join on tokio task: {0}")]
    TokioJoinError(#[from] JoinError),

    #[error("Invalid URL: {1}, url: {0}")]
    InvalidUrl(String, tungstenite::Error),

    #[error("Invalid ACP message: {0}")]
    InvalidAcpMessage(#[from] acp::Error),
}

/// Creates a new JSON RPC error reply for a given request id
fn create_json_rpc_error(
    error: &Error,
    original_request_id: RequestId,
) -> serde_json::Result<JsonValue> {
    let message = match error {
        Error::UnableToGetAccessToken(e) => format!(
            "Unable to get Access Tokens. Try relogin with `jcp logout && jcp login`. Details: {e}"
        ),
        Error::NoRefreshToken => "Please login with `jcp login` first".to_string(),
        Error::InvalidUrl(url, _) => format!("Invalid URL given: {url}"),
        e => e.to_string(),
    };
    let error = acp::Error::new(ErrorCode::InvalidRequest.into(), message);
    let message = rpc::Response::<(), _>::new(original_request_id, Err(error));
    serde_json::to_value(rpc::JsonRpcMessage::wrap(message))
}
