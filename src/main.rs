use clap::{Parser, Subcommand};
use dotenv::dotenv;
use jcp::{
    auth::{get_access_token, login},
    keychain::{self, SecretBackend},
};
use std::process::Command;
use std::{env, process};
use tokio::runtime::Runtime;

struct GitInfo {
    url: String,
    branch: String,
    revision: String,
}

fn run_git(args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .args(args)
        .output()
        .map_err(|e| format!("Failed to execute git: {}", e))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Retrieves git repository information from the current directory.
/// Returns URL of the remote origin, current branch name, and HEAD commit SHA.
fn get_git_info() -> Result<GitInfo, String> {
    let url = run_git(&["remote", "get-url", "origin"])?;
    let branch = run_git(&["rev-parse", "--abbrev-ref", "HEAD"])?;
    let revision = run_git(&["rev-parse", "HEAD"])?;

    Ok(GitInfo {
        url,
        branch,
        revision,
    })
}

#[derive(Parser)]
#[command(name = "acp-jcp")]
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
            if let Err(e) = keychain.delete_refresh_token() {
                eprintln!("Failed to delete refresh token: {}", e);
                process::exit(1);
            }
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

    let Some(ai_platform_token) = env::var("AI_PLATFORM_TOKEN").ok() else {
        eprintln!("AI_PLATFORM_TOKEN env variable should be configured");
        process::exit(1);
    };
    let jcp_url = env::var("JCP_URL")
        .ok()
        .unwrap_or("wss://api.stgn.jetbrains.cloud/agent-spawner/acp".into());

    let jba_access_token = authenticate(keychain);

    let mut request = match jcp_url.into_client_request() {
        Ok(req) => req,
        Err(e) => {
            eprintln!("Invalid JCP URL: {}", e);
            process::exit(1);
        }
    };
    let auth_header = match format!("Bearer {jba_access_token}").parse() {
        Ok(header) => header,
        Err(e) => {
            eprintln!("Failed to create authorization header: {}", e);
            process::exit(1);
        }
    };
    request.headers_mut().insert("Authorization", auth_header);

    let config = match get_git_info() {
        Ok(git_info) => Ok(Config {
            git_url: git_info.url,
            branch: git_info.branch,
            revision: git_info.revision,
            ai_platform_token,
        }),
        Err(e) => {
            let desc =
                format!("Failed to get git info. Program should be run in git working copy. {e}");
            eprintln!("{desc}");
            Err(desc)
        }
    };

    let runtime = Runtime::new().expect("Failed to create Tokio runtime");
    runtime.block_on(async {
        let traffic_log = match TrafficLog::new(env::var("TRAFFIC_LOG").ok()).await {
            Ok(log) => log,
            Err(e) => {
                eprintln!("Failed to create traffic log: {}", e);
                process::exit(1);
            }
        };

        let (ws_stream, _) = match connect_async(request).await {
            Ok(result) => result,
            Err(e) => {
                eprintln!("Failed to connect to JCP: {}", e);
                process::exit(1);
            }
        };
        let (ws_tx, ws_rx) = ws_stream.split();

        let downlink = IoTransport::new(stdin(), stdout());
        let uplink = WebSocketTransport::new(ws_rx, ws_tx);

        let mut adapter = Adapter::new(config, Box::new(downlink), Box::new(uplink));
        adapter.set_traffic_log(traffic_log);
        while adapter
            .handle_next_message()
            .await
            .expect("Unable to handle message")
        {}
    });
}

fn authenticate(keychain: &dyn SecretBackend) -> String {
    if let Ok(access_key) = env::var("JBA_ACCESS_TOKEN") {
        access_key
    } else {
        // Try to get refresh token from keychain and upgrade it
        let refresh_token = match keychain.get_refresh_token() {
            Ok(Some(token)) => token,
            Ok(None) => {
                eprintln!("No refresh token found");
                eprintln!("Please run `acp-jcp login` to authenticate.");
                process::exit(1);
            }
            Err(e) => {
                eprintln!("Failed to read refresh token from keychain: {}", e);
                process::exit(1);
            }
        };
        match get_access_token(&refresh_token) {
            Ok(token) => token,
            Err(e) => {
                eprintln!("Failed to get access token: {}", e);
                eprintln!("Please run `acp-jcp login` to re-authenticate.");
                process::exit(1);
            }
        }
    }
}
