use jwt::Token;
use oauth2::{
    AuthUrl, AuthorizationCode, ClientId, CsrfToken, PkceCodeChallenge, RedirectUrl, Scope,
    TokenResponse, TokenUrl, basic::BasicClient,
};
use reqwest::{blocking::Client, header::ACCEPT, redirect::Policy};
use serde::{Deserialize, Serialize};
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

const LICENSE_API_URL: &str = "https://active.jetprofile-aip.intellij.net";

const JB_AI_API_URL: &str = "https://api.stgn.jetbrains.ai";

/// Agent Spawner audience for upgrading OAuth access token
const JCP_AS_AUDIENCE: &str = "jcp-agent-spawner";

/// The expected callback path for OAuth redirect
const CALLBACK_PATH: &str = "/space/auth";

/// Contains access tokens to JCP and JetBrains AI Platform
pub struct AccessTokens {
    pub jcp_access_token: String,
    pub ai_access_token: String,
}

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
        .add_scope(Scope::new("jba:r_profile".to_string()))
        .add_scope(Scope::new("jba:r_ide_auth".to_string()))
        .add_scope(Scope::new("jba:r_assets".to_string()))
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
/// 1. Uses the JCP refresh token to get a fresh JCP access token and ID token
/// 2. Fetches organization info from JCP
/// 3. Switches the token audience to get a JCP-scoped token
/// 4. reads the first AI license from JB AI platform and retrieves JB AI token
///
/// Use this with a refresh token obtained from [`login()`].
pub fn get_access_tokens(refresh_token: &str) -> Result<AccessTokens, AuthError> {
    let http_client = create_http_client()?;

    // Refresh to get a new access and ID tokens
    let tokens = retrieve_jcp_access_and_id_tokens(&http_client, refresh_token)?;

    // Get organization info
    let org_info = get_org_info(&http_client, &tokens.access_token)?;

    let id_token = tokens.id_token.ok_or(AuthError::MissingIdToken)?;

    // Switch token audience for JCP access
    let jcp_access_token =
        retrieve_jcp_scoped_access_token(&http_client, refresh_token, &org_info)?;
    let ai_access_token = retrieve_ai_access_token(&http_client, &tokens.access_token, &id_token)?;

    Ok(AccessTokens {
        jcp_access_token,
        ai_access_token,
    })
}

/// Creates an HTTP client configured for OAuth operations.
fn create_http_client() -> Result<Client, AuthError> {
    Ok(Client::builder()
        .redirect(Policy::none()) // Disable redirects to prevent SSRF
        .build()?)
}

/// Refreshes an access token using a refresh token.
fn retrieve_jcp_access_and_id_tokens(
    http_client: &Client,
    refresh_token: &str,
) -> Result<OAuthTokenResponse, AuthError> {
    let token_url = format!("{}/oauth2/token", OAUTH_BASE_URL);

    let response = http_client
        .post(&token_url)
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", CLIENT_ID),
        ])
        .send()?
        .error_for_status()
        .map_err(AuthError::TokenRefresh)?
        .json()?;

    Ok(response)
}

/// Fetches organization info from JCP using the access token.
fn get_org_info(http_client: &Client, access_token: &str) -> Result<OrgInfo, AuthError> {
    let raw_token = http_client
        .get(format!("{}/org/orgsuserinfo", JCP_API_URL))
        .bearer_auth(access_token)
        .header("Accept", "application/jwt")
        .send()?
        .error_for_status()
        .map_err(AuthError::OrgInfoFetch)?
        .text()?;

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

/// Reads JB AI Platform token linked to a AI-enabled license
fn retrieve_ai_access_token(
    http: &Client,
    access_token: &str,
    id_token: &str,
) -> Result<String, AuthError> {
    let license_id = http
        .get(format!("{LICENSE_API_URL}/services/account/assets"))
        .bearer_auth(access_token)
        .header(ACCEPT, "application/json")
        .send()?
        .error_for_status()
        .map_err(AuthError::License)?
        .json::<UserAssets>()?
        .find_matched_licenses()
        // No UI yet, so just choosing first License
        .next()
        .map(|l| l.license_id.clone())
        .ok_or(AuthError::NoValidAiLicenseFound)?;

    let response = http
        .post(format!(
            "{JB_AI_API_URL}/auth/jetbrains-jwt/provide-access/license/v2"
        ))
        .bearer_auth(id_token)
        .header(ACCEPT, "application/json")
        .json(&grazie_license_v2::Request { license_id })
        .send()?
        .error_for_status()
        .map_err(AuthError::AiToken)?
        .json::<grazie_license_v2::Response>()?;

    Ok(response.token)
}

/// Switches the token audience to get a JCP-scoped access token.
///
/// https://youtrack.jetbrains.com/projects/JCP/articles/JCP-A-204/Refresh-Token-Flow#org-access-token
fn retrieve_jcp_scoped_access_token(
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
        .send()?
        .error_for_status()
        .map_err(AuthError::AudienceSwitch)?;

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

#[derive(Error, Debug)]
pub enum AuthError {
    #[error("Failed to start local callback server: {0}")]
    ServerStart(#[source] Box<dyn std::error::Error + Send + Sync>),

    #[error("OAuth server returned an error: {0}")]
    OAuthServer(String),

    #[error("No valid AI License found")]
    NoValidAiLicenseFound,

    #[error("Missing IDToken in JCP OAuth response")]
    MissingIdToken,

    #[error("Failed to exchange authorization code for tokens: {0}")]
    TokenExchange(#[source] Box<dyn std::error::Error + Send + Sync>),

    #[error("Failed to parse JWT token: {0}")]
    JwtError(#[from] jwt::Error),

    #[error("Missing refresh token in OAuth response")]
    MissingRefreshToken,

    #[error("Failed to fetch organization info: {0}")]
    OrgInfoFetch(reqwest::Error),

    #[error("No organization found in user's account")]
    NoOrganization,

    #[error("No workspace found in user's organization")]
    NoWorkspace,

    #[error("Failed to switch token audience: {0}")]
    AudienceSwitch(reqwest::Error),

    #[error("Failed to refresh access token: {0}")]
    TokenRefresh(reqwest::Error),

    #[error("Failed to fetch license: {0}")]
    License(reqwest::Error),

    #[error("Failed to fetch AI token: {0}")]
    AiToken(reqwest::Error),

    #[error("HTTP request failed: {0}")]
    ReqwestRequest(#[from] reqwest::Error),

    #[error("Invalid URL: {0}")]
    InvalidUrl(#[from] oauth2::url::ParseError),
}

#[derive(Deserialize)]
struct OAuthTokenResponse {
    #[serde(rename = "access_token")]
    access_token: String,

    /// id_token is bound to `openid` OAuth-scope and can be missing from token response
    #[serde(rename = "id_token")]
    id_token: Option<String>,
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

#[derive(Deserialize, Debug)]
pub struct UserAssets {
    #[serde(rename = "assets")]
    pub assets: Vec<License>,
}

impl UserAssets {
    pub fn find_matched_licenses(&self) -> impl Iterator<Item = &License> {
        self.assets.iter().filter(|lic| {
            !lic.cancelled
                && !lic.suspended
                && lic.allowances.iter().any(Allowance::is_ai_allowance)
        })
    }
}

/// Matching type for Grazie License API
///
/// https://code.jetbrains.team/p/grazi/repositories/grazie-platform/files/e6822ebbbf1c33110b9754ea9c3be776e5a2b654/api/api-gateway/api-gateway-api/src/commonMain/kotlin/ai/grazie/api/gateway/api/AuthAPI.kt?tab=source&line=157&lines-count=12
mod grazie_license_v2 {
    use super::*;

    #[derive(Serialize)]
    pub(super) struct Request {
        #[serde(rename = "licenseId")]
        pub(super) license_id: String,
    }

    #[derive(Deserialize)]
    pub(super) struct Response {
        #[serde(rename = "token")]
        pub(super) token: String,
    }
}

#[derive(Deserialize, Debug)]
pub struct License {
    #[serde(rename = "code")]
    pub code: String,

    #[serde(rename = "licenseId")]
    pub license_id: String,

    #[serde(rename = "suspended")]
    pub suspended: bool,

    #[serde(rename = "cancelled")]
    pub cancelled: bool,

    #[serde(rename = "allowance")]
    pub allowances: Vec<Allowance>,
}

#[derive(Deserialize, Debug)]
pub struct Allowance {
    #[serde(rename = "code")]
    pub code: String,

    #[serde(rename = "name")]
    pub name: String,
}

impl Allowance {
    pub fn is_ai_allowance(&self) -> bool {
        const ALLOW_ASSETS: [&str; 5] = ["AIP", "AIPU", "AIF", "AIL", "GZL"];
        ALLOW_ASSETS.contains(&self.code.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn check_assets_deserialization() {
        let json = json!(
            {
              "status": "OK",
              "username": "john.doe@jetbrains.com",
              "firstName": "John",
              "lastName": "Doe",
              "email": "john.doe@jetbrains.com",
              "assets": [
                {
                  "code": "AIPU",
                  "name": "JetBrains AI Ultimate",
                  "description": "",
                  "licenseId": "JPRK",
                  "suspended": true,
                  "cancelled": false,
                  "overuseTill": "2026-12-17T00:00:00.000+0300",
                  "version": "2025.2",
                  "expires": "2026-12-16",
                  "recurrent": false,
                  "allowance": [ { "code": "AIPU", "name": "JetBrains AI Ultimate", "paidUpTo": "2026-12-16" } ]
                },
                {
                  "code": "JUNP",
                  "name": "Junie Pro",
                  "description": "",
                  "licenseId": "FGTJ2",
                  "suspended": false,
                  "cancelled": false,
                  "overuseTill": "2026-12-31T00:00:00.000+0300",
                  "expires": "2026-12-31",
                  "recurrent": false,
                  "allowance": [ { "code": "JUNP", "name": "Junie Pro", "paidUpTo": "2026-12-31" } ]
                },
                {
                  "code": "AIRP",
                  "name": "Air Preview",
                  "description": "",
                  "licenseId": "PRVU",
                  "suspended": false,
                  "cancelled": false,
                  "overuseTill": "2026-03-31T00:00:00.000+0300",
                  "expires": "2026-03-31",
                  "recurrent": false,
                  "allowance": [ { "code": "AIRP", "name": "Air Preview", "paidUpTo": "2026-03-31" } ]
                },
                {
                  "code": "RR",
                  "name": "RustRover",
                  "description": "Rust IDE by JetBrains",
                  "licenseId": "FUIMA",
                  "suspended": false,
                  "cancelled": false,
                  "overuseTill": "2026-07-12T00:00:00.000+0300",
                  "version": "2025.3",
                  "expires": "2026-07-05",
                  "recurrent": false,
                  "allowance": [ { "code": "AIF", "name": "JetBrains AI Pro", "paidUpTo": "2026-07-05" } ]
                }
              ]
            }
        );

        let r = serde_json::from_value::<UserAssets>(json).unwrap();
        let license_ids = r
            .find_matched_licenses()
            .map(|l| &l.license_id)
            .collect::<Vec<_>>();
        assert_eq!(license_ids, vec!["FUIMA"]);
    }
}
