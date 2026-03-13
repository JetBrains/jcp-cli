use clap::{Parser, Subcommand};
use dotenv::dotenv;
use jcp::{
    GitCommandTool,
    auth::{AccessTokens, get_access_tokens, login},
    keychain::{self, SecretBackend},
};
use std::{env, process};
use tokio::runtime::Runtime;

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
    use futures_util::StreamExt;
    use jcp::{Adapter, IoTransport, TrafficLog, WebSocketTransport};
    use tokio::io::{stdin, stdout};
    use tokio_tungstenite::connect_async;
    use tungstenite::client::IntoClientRequest;

    let jcp_url = env::var("JCP_URL").ok().unwrap_or(DEFAULT_JCP_URL.into());

    let tokens = authenticate(keychain);

    let mut request = jcp_url.into_client_request().unwrap();
    request.headers_mut().insert(
        "Authorization",
        format!("Bearer {}", tokens.jcp_access_token)
            .parse()
            .unwrap(),
    );

    let runtime = Runtime::new().expect("Failed to create Tokio runtime");
    runtime.block_on(async {
        let traffic_log = TrafficLog::new(env::var("TRAFFIC_LOG").ok()).await.unwrap();

        let (ws_stream, _) = connect_async(request).await.unwrap();
        let (ws_tx, ws_rx) = ws_stream.split();

        let downlink = IoTransport::new(stdin(), stdout());
        let uplink = WebSocketTransport::new(ws_rx, ws_tx);

        let mut adapter = Adapter::new(
            Box::new(downlink),
            Box::new(uplink),
            Box::new(GitCommandTool),
            tokens.ai_access_token,
        );
        adapter.set_traffic_log(traffic_log);
        adapter.run().await.expect("Unable to handle message");
    });
}

/// Retrieves access tokens
///
/// If both `AI_PLATFORM_TOKEN` and `JCP_ACCESS_TOKEN` are present, then they are used.
/// If not, refresh token is retrieved from a keychain and after that fresh access tokens are requested.
/// `AI_PLATFORM_TOKEN` and `JCP_ACCESS_TOKEN` env variables still allows to override respective tokens.
fn authenticate(keychain: &dyn SecretBackend) -> AccessTokens {
    let jb_ai = env::var("AI_PLATFORM_TOKEN").ok();
    let jcp = env::var("JCP_ACCESS_TOKEN").ok();

    if let Some((jb_ai_access_token, jcp_access_token)) = jb_ai.as_ref().zip(jcp.as_ref()) {
        AccessTokens {
            jcp_access_token: jcp_access_token.to_string(),
            ai_access_token: jb_ai_access_token.to_string(),
        }
    } else {
        // Try to get refresh token from keychain and upgrade it
        let Some(refresh_token) = keychain.get_refresh_token().unwrap() else {
            eprintln!("No refresh token found");
            eprintln!("Please run `acp-jcp login` to authenticate.");
            process::exit(1);
        };
        match get_access_tokens(&refresh_token) {
            Ok(tokens) => AccessTokens {
                jcp_access_token: jcp.unwrap_or(tokens.jcp_access_token),
                ai_access_token: jb_ai.unwrap_or(tokens.ai_access_token),
            },
            Err(e) => {
                eprintln!("Failed to get access token: {}", e);
                eprintln!("Please run `acp-jcp login` to re-authenticate.");
                process::exit(1);
            }
        }
    }
}
