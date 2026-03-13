use async_trait::async_trait;
use clap::{Parser, Subcommand};
use dotenv::dotenv;
use jcp::{
    GitTool, WorkingCopyInfo,
    auth::{AccessTokens, get_access_tokens, login},
    keychain::{self, SecretBackend},
};
use std::path::Path;
use std::process::Command;
use std::{env, process};
use tokio::runtime::Runtime;

/// Reads git info by running git commands in the given directory
pub struct GitCommandTool;

#[async_trait]
impl GitTool for GitCommandTool {
    async fn read_working_copy_info(&self, path: &Path) -> Result<WorkingCopyInfo, String> {
        let path = path.to_path_buf();
        tokio::task::spawn_blocking(move || {
            let url = run_git(&path, &["remote", "get-url", "origin"])?;
            let branch = run_git(&path, &["rev-parse", "--abbrev-ref", "HEAD"])?;
            let revision = run_git(&path, &["rev-parse", "HEAD"])?;
            Ok(WorkingCopyInfo {
                url,
                branch,
                revision,
            })
        })
        .await
        .map_err(|e| format!("Git task failed: {e}"))?
    }
}

fn run_git(cwd: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .map_err(|e| format!("Failed to execute git: {}", e))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

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
    use jcp::{Adapter, Config, IoTransport, TrafficLog, WebSocketTransport};
    use tokio::io::{stdin, stdout};
    use tokio_tungstenite::connect_async;
    use tungstenite::client::IntoClientRequest;

    let jcp_url = env::var("JCP_URL")
        .ok()
        .unwrap_or("wss://api.stgn.jetbrains.cloud/agent-spawner/acp".into());

    let tokens = authenticate(keychain);

    let mut request = jcp_url.into_client_request().unwrap();
    request.headers_mut().insert(
        "Authorization",
        format!("Bearer {}", tokens.jcp_access_token)
            .parse()
            .unwrap(),
    );

    let config = Config {
        ai_platform_token: tokens.ai_access_token,
    };

    let runtime = Runtime::new().expect("Failed to create Tokio runtime");
    runtime.block_on(async {
        let traffic_log = TrafficLog::new(env::var("TRAFFIC_LOG").ok()).await.unwrap();

        let (ws_stream, _) = connect_async(request).await.unwrap();
        let (ws_tx, ws_rx) = ws_stream.split();

        let downlink = IoTransport::new(stdin(), stdout());
        let uplink = WebSocketTransport::new(ws_rx, ws_tx);

        let mut adapter = Adapter::new(
            config,
            Box::new(GitCommandTool),
            Box::new(downlink),
            Box::new(uplink),
        );
        adapter.set_traffic_log(traffic_log);
        while adapter
            .handle_next_message()
            .await
            .expect("Unable to handle message")
        {}
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
