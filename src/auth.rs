use jwt::Token;
use oauth2::{
    AuthUrl, AuthorizationCode, ClientId, CsrfToken, PkceCodeChallenge, RedirectUrl, Scope,
    TokenResponse, TokenUrl, basic::BasicClient,
};
use reqwest::{blocking::Client, redirect::Policy};
use serde::Deserialize;
use serde_json::Value;
use thiserror::Error;
use tiny_http::{Response, Server};
use url::Url;

/// Base URL for JetBrains OAuth provider
const OAUTH_BASE_URL: &str = "https://public.aip.oauth.intservices.aws.intellij.net";

/// OAuth client ID for AIR
const CLIENT_ID: &str = "air";

/// JetBrains Cloud Platform API base URL
const JCP_API_URL: &str = "https://api.stgn.jetbrainscloud.com";

/// Agent Spawner audience for upgrading OAuth access token
const JCP_AS_AUDIENCE: &str = "jcp-agent-spawner";

/// The expected callback path for OAuth redirect
const CALLBACK_PATH: &str = "/space/auth";

/// Performs OAuth browser login flow and returns a refresh token.
///
/// This function:
/// 1. Opens a browser for user authentication
/// 2. Receives the authorization code via local callback server
/// 3. Exchanges the code for tokens
/// 4. Returns the refresh token for later use
///
/// The refresh token can be stored (e.g., in keychain) and used with
/// `get_access_token()` to obtain access tokens without re-authentication.
pub fn login() -> Result<String, AuthError> {
    let http_client = create_http_client()?;

    // Start local callback server
    let server = Server::http("localhost:0").map_err(AuthError::ServerStart)?;

    let local_addr = server
        .server_addr()
        .to_ip()
        .ok_or_else(|| AuthError::ServerStart("Failed to get server address".into()))?;

    let local_port = local_addr.port();
    let redirect_url = format!("http://localhost:{}{}", local_port, CALLBACK_PATH);

    // Configure OAuth client
    let auth_url = format!("{}/oauth2/auth", OAUTH_BASE_URL);
    let token_url = format!("{}/oauth2/token", OAUTH_BASE_URL);

    let client = BasicClient::new(ClientId::new(CLIENT_ID.to_string()))
        .set_auth_uri(AuthUrl::new(auth_url)?)
        .set_token_uri(TokenUrl::new(token_url)?)
        .set_redirect_uri(RedirectUrl::new(redirect_url)?);

    // Generate PKCE challenge
    let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();

    // Build authorization URL with required scopes
    let (auth_url, csrf_token) = client
        .authorize_url(CsrfToken::new_random)
        .add_scope(Scope::new("offline_access".to_string()))
        .add_scope(Scope::new("openid".to_string()))
        .add_scope(Scope::new("org-service".to_string()))
        .add_scope(Scope::new("jba".to_string()))
        .set_pkce_challenge(pkce_challenge)
        .url();

    eprintln!(
        "Opening browser for authentication... If the browser doesn't open, visit this URL manually:"
    );
    eprintln!("\n  {}\n", auth_url);

    // We told user to open browser in case we failed, so ignore the error
    let _ = open::that(auth_url.to_string());

    // Wait for callback
    let code = read_authorization_code_from_callback(server, csrf_token)?;

    // Exchange code for tokens
    let token_response = client
        .exchange_code(code)
        .set_pkce_verifier(pkce_verifier)
        .request(&http_client)
        .map_err(|e| AuthError::TokenExchange(e.into()))?;

    token_response
        .refresh_token()
        .map(|t| t.secret().to_string())
        .ok_or(AuthError::MissingRefreshToken)
}

/// Converts a refresh token into a JCP access token.
///
/// This function:
/// 1. Uses the refresh token to get a fresh access token
/// 2. Fetches organization info from JCP
/// 3. Switches the token audience to get a JCP-scoped token
///
/// Use this with a refresh token obtained from [`login()`].
pub fn get_access_token(refresh_token: &str) -> Result<String, AuthError> {
    let http_client = create_http_client()?;

    // Refresh to get a new access token
    let access_token = refresh_access_token(&http_client, refresh_token)?;

    // Get organization info
    let org_info = get_org_info(&http_client, &access_token)?;

    // Switch token audience for JCP access
    switch_token_audience(&http_client, refresh_token, &org_info)
}

/// Creates an HTTP client configured for OAuth operations.
fn create_http_client() -> Result<Client, AuthError> {
    Ok(Client::builder()
        .redirect(Policy::none()) // Disable redirects to prevent SSRF
        .build()?)
}

/// Refreshes an access token using a refresh token.
fn refresh_access_token(http_client: &Client, refresh_token: &str) -> Result<String, AuthError> {
    let token_url = format!("{}/oauth2/token", OAUTH_BASE_URL);

    let response = http_client
        .post(&token_url)
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", CLIENT_ID),
        ])
        .send()?;

    let status = response.status().as_u16();
    if status != 200 {
        return Err(AuthError::TokenRefresh {
            status,
            body: response.text().unwrap_or_default(),
        });
    }

    Ok(response.json::<OAuthTokenResponse>()?.access_token)
}

/// Fetches organization info from JCP using the access token.
fn get_org_info(http_client: &Client, access_token: &str) -> Result<OrgInfo, AuthError> {
    let response = http_client
        .get(format!("{}/org/orgsuserinfo", JCP_API_URL))
        .bearer_auth(access_token)
        .header("Accept", "application/jwt")
        .send()?;

    let status = response.status().as_u16();
    if status != 200 {
        return Err(AuthError::OrgInfoFetch {
            status,
            body: response.text().unwrap_or_default(),
        });
    }

    let raw_token = response.text()?;

    // Parse JWT to extract organization ID
    let token: Token<Value, JcpTokenClaims, _> = Token::parse_unverified(&raw_token)?;

    // There is no UI yet, so we just choosing first organisation
    let organization = token
        .claims()
        .orgs
        .first()
        .ok_or(AuthError::NoOrganization)?;
    let org_id = organization.id.clone();
    let workspace_id = organization
        .workspaces
        .first()
        .ok_or(AuthError::NoWorkspace)?
        .id
        .clone();

    Ok(OrgInfo {
        org_id,
        raw_token,
        workspace_id,
    })
}

/// Switches the token audience to get a JCP-scoped access token.
///
/// https://youtrack.jetbrains.com/projects/JCP/articles/JCP-A-204/Refresh-Token-Flow#org-access-token
fn switch_token_audience(
    http_client: &Client,
    refresh_token: &str,
    org_info: &OrgInfo,
) -> Result<String, AuthError> {
    let token_url = format!("{}/oauth2/token", OAUTH_BASE_URL);

    let response = http_client
        .post(&token_url)
        .form(&[
            ("grant_type", "switch_audience"),
            ("refresh_token", refresh_token),
            ("client_id", CLIENT_ID),
            ("audience", JCP_AS_AUDIENCE),
            ("org_id", &org_info.org_id),
            ("orgs_user_info", &org_info.raw_token),
            ("workspace_id", &org_info.workspace_id),
        ])
        .send()?;

    let status = response.status().as_u16();
    if status != 200 {
        return Err(AuthError::AudienceSwitch {
            status,
            body: response.text().unwrap_or_default(),
        });
    }

    Ok(response.json::<OAuthTokenResponse>()?.access_token)
}

/// Waits for the OAuth callback and extracts the authorization code.
fn read_authorization_code_from_callback(
    server: Server,
    csrf_token: CsrfToken,
) -> Result<AuthorizationCode, AuthError> {
    loop {
        let Ok(request) = server.recv() else {
            continue;
        };

        let url_str = request.url();

        // Parse the URL to extract query parameters
        let full_url = format!("http://localhost{}", url_str);
        let parsed_url = match Url::parse(&full_url) {
            Ok(url) => url,
            Err(e) => {
                let description = format!("Failed to parse callback URL: {}", e);
                let response = Response::from_string(&description).with_status_code(400);
                let _ = request.respond(response);
                continue;
            }
        };

        // Check if this is the expected callback path
        if parsed_url.path() != CALLBACK_PATH {
            let response = Response::from_string("Not Found").with_status_code(404);
            let _ = request.respond(response);
            continue;
        }

        // Extract authorization code
        let Some(code) = extract_query_param(&parsed_url, "code") else {
            // Check if there's an error parameter
            if let Some(error) = extract_query_param(&parsed_url, "error") {
                let error_desc =
                    extract_query_param(&parsed_url, "error_description").unwrap_or_default();

                let error_msg = format!("{} - {}", error, error_desc);
                let response = Response::from_string(&error_msg).with_status_code(400);
                let _ = request.respond(response);
                return Err(AuthError::OAuthServer(error_msg));
            }

            let response = Response::from_string("Bad Request: missing code").with_status_code(400);
            let _ = request.respond(response);
            continue;
        };

        // Extract and verify state parameter (CSRF protection)
        let Some(state) = extract_query_param(&parsed_url, "state") else {
            let response =
                Response::from_string("Bad Request: missing state").with_status_code(400);
            let _ = request.respond(response);
            continue;
        };

        if state != *csrf_token.secret() {
            let response =
                Response::from_string("Bad Request: invalid state").with_status_code(400);
            let _ = request.respond(response);
            continue;
        }

        // Send success response to browser
        let response = Response::from_string(include_str!("auth_success.html")).with_header(
            tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..])
                .unwrap(),
        );
        let _ = request.respond(response);

        return Ok(AuthorizationCode::new(code));
    }
}

/// Extracts a query parameter from a URL.
fn extract_query_param(url: &Url, param_name: &str) -> Option<String> {
    url.query_pairs()
        .find(|(key, _)| key == param_name)
        .map(|(_, value)| value.into_owned())
}

// =============================================================================
// Error Types
// =============================================================================

#[derive(Error, Debug)]
pub enum AuthError {
    #[error("Failed to start local callback server: {0}")]
    ServerStart(#[source] Box<dyn std::error::Error + Send + Sync>),

    #[error("OAuth server returned an error: {0}")]
    OAuthServer(String),

    #[error("Failed to exchange authorization code for tokens: {0}")]
    TokenExchange(#[source] Box<dyn std::error::Error + Send + Sync>),

    #[error("Failed to parse JWT token: {0}")]
    JwtError(#[from] jwt::Error),

    #[error("Missing refresh token in OAuth response")]
    MissingRefreshToken,

    #[error("Failed to fetch organization info: {status} - {body}")]
    OrgInfoFetch { status: u16, body: String },

    #[error("No organization found in user's account")]
    NoOrganization,

    #[error("No workspace found in user's organization")]
    NoWorkspace,

    #[error("Failed to switch token audience: {status} - {body}")]
    AudienceSwitch { status: u16, body: String },

    #[error("Failed to refresh access token: {status} - {body}")]
    TokenRefresh { status: u16, body: String },

    #[error("HTTP request failed: {0}")]
    ReqwestRequest(#[from] reqwest::Error),

    #[error("Invalid URL: {0}")]
    InvalidUrl(#[from] oauth2::url::ParseError),
}

#[derive(Deserialize)]
struct OAuthTokenResponse {
    #[serde(rename = "access_token")]
    access_token: String,
}

#[derive(Deserialize, Debug)]
struct JcpTokenClaims {
    #[serde(rename = "orgMemberships")]
    orgs: Vec<Organization>,
}

#[derive(Deserialize, Debug, Clone)]
struct Organization {
    #[serde(rename = "orgId")]
    id: String,

    #[serde(rename = "workspaces")]
    workspaces: Vec<Workspace>,
}

#[derive(Deserialize, Debug, Clone)]
struct Workspace {
    #[serde(rename = "id")]
    id: String,
}

/// Organization info retrieved from JCP
struct OrgInfo {
    org_id: String,
    workspace_id: String,
    raw_token: String,
}
