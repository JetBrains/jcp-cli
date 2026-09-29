//! Blocking REST clients for the JCP services that the `session`, `env`, `repo` and `agents` commands use.

use reqwest::{
    Method, StatusCode,
    blocking::{Client, RequestBuilder, Response},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::time::Duration;
use thiserror::Error;

pub mod env_configs;
pub mod repos;
pub mod spawner;

/// Time limit for one request. Log archives can be large, so the limit is high.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);

/// A decoded response body, together with the body text as the server sent it.
///
/// `--json` prints [`Self::raw`].
#[derive(Debug)]
pub struct ApiResponse<T> {
    pub raw: String,
    pub value: T,
}

#[derive(Error, Debug)]
pub enum ApiError {
    #[error("The server refused the access token (401). Log in again with `jcp login`.")]
    Unauthorized,

    #[error("You do not have access (403).{}", forbidden_detail(.0))]
    Forbidden(String),

    #[error("Not found (404): {path}.{}", detail(.body))]
    NotFound { path: String, body: String },

    #[error("Bad request (400): {0}")]
    BadRequest(String),

    #[error("Request {method} {path} failed with status {status}.{}", detail(.body))]
    Status {
        method: String,
        path: String,
        status: u16,
        body: String,
    },

    #[error("Request {method} {path} failed: {source}")]
    Transport {
        method: String,
        path: String,
        #[source]
        source: reqwest::Error,
    },

    #[error("Cannot read the response of {path}: {source}. Body: {body}")]
    Decode {
        path: String,
        body: String,
        #[source]
        source: serde_json::Error,
    },
}

/// The server tells the cause of a 403, for example a missing VCS authorization with a link to fix it.
/// The `allowCloudAgentsRun` hint is only for the AI Management policy error, or when the server tells nothing.
fn forbidden_detail(body: &str) -> String {
    let body = body.trim();
    if body.is_empty() || body.contains("AI Management policy") {
        format!(
            " Make sure that the AI-Management setting `allowCloudAgentsRun` is on for your organization.{}",
            detail(body)
        )
    } else {
        format!(" {body}")
    }
}

fn detail(body: &str) -> String {
    let body = body.trim();
    if body.is_empty() {
        String::new()
    } else {
        format!(" Server message: {body}")
    }
}

/// An HTTP client for one service. It sends the Bearer token with each request.
#[derive(Clone)]
pub struct HttpClient {
    client: Client,
    base_url: String,
    token: String,
}

impl HttpClient {
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            client: create_client(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            token: token.into(),
        }
    }

    pub fn get_json<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<ApiResponse<T>, ApiError> {
        let body = self.send_text(Method::GET, path, query, None::<&()>)?;
        decode(path, body)
    }

    pub fn get_text(&self, path: &str, query: &[(&str, String)]) -> Result<String, ApiError> {
        self.send_text(Method::GET, path, query, None::<&()>)
    }

    pub fn send_json<B: Serialize, T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: &B,
    ) -> Result<ApiResponse<T>, ApiError> {
        let body = self.send_text(method, path, &[], Some(body))?;
        decode(path, body)
    }

    /// Sends a request and ignores the response body.
    pub fn send_no_content<B: Serialize>(
        &self,
        method: Method,
        path: &str,
        body: Option<&B>,
    ) -> Result<(), ApiError> {
        self.send_text(method, path, &[], body).map(|_| ())
    }

    /// Sends a request and returns the response body as bytes.
    pub fn send_bytes<B: Serialize>(
        &self,
        method: Method,
        path: &str,
        body: Option<&B>,
    ) -> Result<Vec<u8>, ApiError> {
        let response = self.send(method.clone(), path, &[], body)?;
        response
            .bytes()
            .map(|b| b.to_vec())
            .map_err(|source| ApiError::Transport {
                method: method.to_string(),
                path: path.to_string(),
                source,
            })
    }

    /// Downloads an absolute URL without the Bearer token. Use it for presigned URLs.
    pub fn download(&self, url: &str) -> Result<Vec<u8>, ApiError> {
        let transport = |source| ApiError::Transport {
            method: "GET".into(),
            path: url.to_string(),
            source,
        };
        let response = self.client.get(url).send().map_err(transport)?;
        let response = check_status(response, "GET", url)?;
        response.bytes().map(|b| b.to_vec()).map_err(transport)
    }

    fn send_text<B: Serialize>(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<&B>,
    ) -> Result<String, ApiError> {
        let response = self.send(method.clone(), path, query, body)?;
        response.text().map_err(|source| ApiError::Transport {
            method: method.to_string(),
            path: path.to_string(),
            source,
        })
    }

    fn send<B: Serialize>(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<&B>,
    ) -> Result<Response, ApiError> {
        let url = format!("{}{}", self.base_url, path);
        let mut request: RequestBuilder = self
            .client
            .request(method.clone(), &url)
            .bearer_auth(&self.token);
        if !query.is_empty() {
            request = request.query(query);
        }
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request.send().map_err(|source| ApiError::Transport {
            method: method.to_string(),
            path: path.to_string(),
            source,
        })?;
        check_status(response, method.as_str(), path)
    }
}

fn create_client() -> Client {
    Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .expect("Unable to create HTTP client")
}

fn check_status(response: Response, method: &str, path: &str) -> Result<Response, ApiError> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let body = response.text().unwrap_or_default();
    Err(match status {
        StatusCode::UNAUTHORIZED => ApiError::Unauthorized,
        StatusCode::FORBIDDEN => ApiError::Forbidden(body),
        StatusCode::NOT_FOUND => ApiError::NotFound {
            path: path.to_string(),
            body,
        },
        StatusCode::BAD_REQUEST => ApiError::BadRequest(body),
        _ => ApiError::Status {
            method: method.to_string(),
            path: path.to_string(),
            status: status.as_u16(),
            body,
        },
    })
}

fn decode<T: DeserializeOwned>(path: &str, raw: String) -> Result<ApiResponse<T>, ApiError> {
    match serde_json::from_str(&raw) {
        Ok(value) => Ok(ApiResponse { raw, value }),
        Err(source) => Err(ApiError::Decode {
            path: path.to_string(),
            body: raw.chars().take(500).collect(),
            source,
        }),
    }
}

/// Reads `null` as the default value. Use it with `#[serde(default)]`: the services send `null` for some
/// empty lists and objects (for example, `providerErrors` and `permissions` of repo-connections).
pub(crate) fn null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Option::unwrap_or_default)
}

/// Encodes one path segment (for example, a provider name or an ID from the user).
pub fn path_segment(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes())
        .collect::<String>()
        .replace('+', "%20")
}
