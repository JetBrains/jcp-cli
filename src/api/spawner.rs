//! Agent Spawner REST API (`/agent-spawner`): sessions, tasks, history, debug data, logs, artifacts, agents.

use super::{ApiError, ApiResponse, HttpClient};
use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

/// Status values that end a turn or the session. `watch` stops at these values.
///
/// `USER_INPUT_REQUIRED` is also here: the agent does not continue until the user answers.
pub const FINAL_STATUSES: &[&str] = &[
    "FINISHED",
    "USER_INPUT_REQUIRED",
    "ERROR",
    "CANCELLED",
    "ABORTED",
    "SUSPENDED",
    "ARCHIVED",
];

/// The agent waits for an answer of the user.
pub const USER_INPUT_REQUIRED: &str = "USER_INPUT_REQUIRED";

/// Status values that mean the turn failed.
pub const FAILED_STATUSES: &[&str] = &["ERROR", "ABORTED", "CANCELLED"];

/// The maximum page size of `GET /sessions/{id}/history`.
pub const MAX_HISTORY_PAGE: u64 = 1000;

pub struct SpawnerApi {
    http: HttpClient,
}

impl SpawnerApi {
    pub fn new(http: HttpClient) -> Self {
        Self { http }
    }

    pub fn list_sessions(
        &self,
        offset: u64,
        limit: u64,
    ) -> Result<ApiResponse<SessionsList>, ApiError> {
        self.http.get_json(
            "/v1/sessions",
            &[("offset", offset.to_string()), ("limit", limit.to_string())],
        )
    }

    pub fn search_sessions(
        &self,
        q: &str,
        case_sensitive: bool,
        offset: u64,
        limit: u64,
    ) -> Result<ApiResponse<SessionsList>, ApiError> {
        self.http.get_json(
            "/v1/sessions/search",
            &[
                ("q", q.to_string()),
                ("caseSensitive", case_sensitive.to_string()),
                ("offset", offset.to_string()),
                ("limit", limit.to_string()),
            ],
        )
    }

    pub fn get_session(&self, id: &str) -> Result<ApiResponse<Session>, ApiError> {
        self.http.get_json(&format!("/sessions/{id}"), &[])
    }

    /// Returns the status. The spec gives `text/plain`, so the value can come with or without JSON quotes.
    pub fn get_status(&self, id: &str) -> Result<String, ApiError> {
        let text = self.http.get_text(&format!("/sessions/{id}/status"), &[])?;
        Ok(text.trim().trim_matches('"').to_string())
    }

    pub fn rename(&self, id: &str, name: &str) -> Result<(), ApiError> {
        self.http.send_no_content(
            Method::PATCH,
            &format!("/sessions/{id}/name"),
            Some(&RenameSessionRequest {
                name: name.to_string(),
            }),
        )
    }

    pub fn resume(&self, id: &str, prompt: Option<&str>) -> Result<(), ApiError> {
        let body = ResumeSessionRequest {
            launch_config: prompt.map(|p| LaunchConfig {
                prompt: p.to_string(),
                session_config_options: None,
            }),
        };
        self.http
            .send_no_content(Method::POST, &format!("/sessions/{id}/resume"), Some(&body))
    }

    pub fn archive(&self, id: &str) -> Result<(), ApiError> {
        self.http
            .send_no_content(Method::PUT, &format!("/sessions/{id}/archive"), None::<&()>)
    }

    pub fn delete(&self, id: &str) -> Result<(), ApiError> {
        self.http
            .send_no_content(Method::DELETE, &format!("/sessions/{id}"), None::<&()>)
    }

    pub fn artifacts(&self, id: &str) -> Result<ApiResponse<Vec<ArtifactInfo>>, ApiError> {
        self.http
            .get_json(&format!("/sessions/{id}/artifacts"), &[])
    }

    pub fn artifact_download(&self, id: &str, name: &str) -> Result<ArtifactDownload, ApiError> {
        self.http
            .get_json(
                &format!("/sessions/{id}/artifacts/download"),
                &[("name", name.to_string())],
            )
            .map(|r| r.value)
    }

    /// Returns one page of history. The server sorts the page from the newest item to the oldest.
    pub fn history(
        &self,
        id: &str,
        offset: u64,
        limit: u64,
    ) -> Result<ApiResponse<SessionHistorySlice>, ApiError> {
        self.http.get_json(
            &format!("/sessions/{id}/history"),
            &[("offset", offset.to_string()), ("limit", limit.to_string())],
        )
    }

    pub fn debug(&self, id: &str) -> Result<ApiResponse<SessionDebugData>, ApiError> {
        self.http.get_json(&format!("/debug/{id}"), &[])
    }

    /// Returns a ZIP file with all logs of the session.
    pub fn logs_archive(&self, id: &str) -> Result<Vec<u8>, ApiError> {
        self.http.send_bytes(
            Method::POST,
            &format!("/debug/{id}/logs/archive"),
            None::<&()>,
        )
    }

    pub fn agents(&self) -> Result<ApiResponse<AvailableAgentsResponse>, ApiError> {
        self.http.get_json("/agents", &[])
    }

    pub fn create_task(&self, request: &CreateTaskRequest) -> Result<ApiResponse<Task>, ApiError> {
        self.http.send_json(Method::POST, "/tasks", request)
    }

    /// Downloads a presigned URL (log file or artifact) without the Bearer token.
    pub fn download(&self, url: &str) -> Result<Vec<u8>, ApiError> {
        self.http.download(url)
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct RepositoryState {
    #[serde(
        rename = "serviceHost",
        default,
        deserialize_with = "crate::api::null_as_default"
    )]
    pub service_host: String,
    #[serde(
        rename = "organization",
        default,
        deserialize_with = "crate::api::null_as_default"
    )]
    pub organization: String,
    #[serde(
        rename = "repositoryName",
        default,
        deserialize_with = "crate::api::null_as_default"
    )]
    pub repository_name: String,
    #[serde(rename = "ref", default, skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
    #[serde(
        rename = "commitHash",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub commit_hash: Option<String>,
}

impl RepositoryState {
    /// `organization/name`
    pub fn full_name(&self) -> String {
        format!("{}/{}", self.organization, self.repository_name)
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct Session {
    #[serde(rename = "sessionId")]
    pub session_id: String,
    #[serde(
        rename = "sessionName",
        default,
        deserialize_with = "crate::api::null_as_default"
    )]
    pub session_name: String,
    #[serde(rename = "ownerId", default)]
    pub owner_id: Option<String>,
    #[serde(
        rename = "status",
        default,
        deserialize_with = "crate::api::null_as_default"
    )]
    pub status: String,
    #[serde(rename = "statusComment", default)]
    pub status_comment: Option<String>,
    #[serde(
        rename = "repositoryState",
        default,
        deserialize_with = "crate::api::null_as_default"
    )]
    pub repository_state: RepositoryState,
    #[serde(rename = "additionalRepos", default)]
    pub additional_repos: Option<Vec<JsonValue>>,
    #[serde(rename = "updatedAt", default)]
    pub updated_at: Option<String>,
    #[serde(rename = "agentType", default)]
    pub agent_type: Option<String>,
    #[serde(rename = "launchedFrom", default)]
    pub launched_from: Option<String>,
    #[serde(rename = "resultsBranchName", default)]
    pub results_branch_name: Option<String>,
    #[serde(rename = "resultsCommitHash", default)]
    pub results_commit_hash: Option<String>,
    #[serde(rename = "shared", default)]
    pub shared: Option<bool>,
    #[serde(rename = "taskId", default)]
    pub task_id: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct PaginationMetaData {
    #[serde(
        rename = "offset",
        default,
        deserialize_with = "crate::api::null_as_default"
    )]
    pub offset: u64,
    #[serde(
        rename = "limit",
        default,
        deserialize_with = "crate::api::null_as_default"
    )]
    pub limit: u64,
    #[serde(
        rename = "totalItems",
        default,
        deserialize_with = "crate::api::null_as_default"
    )]
    pub total_items: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct SessionsList {
    #[serde(
        rename = "sessions",
        default,
        deserialize_with = "crate::api::null_as_default"
    )]
    pub sessions: Vec<Session>,
    #[serde(rename = "pagination", default)]
    pub pagination: Option<PaginationMetaData>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SessionHistoryMessage {
    /// Chronological index of the message in the session
    #[serde(rename = "index")]
    pub index: u64,
    /// JSON text of a JSON-RPC message
    #[serde(rename = "message")]
    pub message: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct SessionHistorySlice {
    #[serde(
        rename = "messages",
        default,
        deserialize_with = "crate::api::null_as_default"
    )]
    pub messages: Vec<SessionHistoryMessage>,
    #[serde(
        rename = "pagination",
        default,
        deserialize_with = "crate::api::null_as_default"
    )]
    pub pagination: PaginationMetaData,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ArtifactInfo {
    #[serde(rename = "name")]
    pub name: String,
    #[serde(
        rename = "sizeBytes",
        default,
        deserialize_with = "crate::api::null_as_default"
    )]
    pub size_bytes: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ArtifactDownload {
    #[serde(rename = "url")]
    pub url: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct DebugExposedPortData {
    #[serde(rename = "localPort", default)]
    pub local_port: Option<u64>,
    #[serde(rename = "publicUrl", default)]
    pub public_url: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct LogFileInfo {
    /// Absolute presigned URL. Download it without the Bearer token.
    #[serde(
        rename = "downloadUrl",
        default,
        deserialize_with = "crate::api::null_as_default"
    )]
    pub download_url: String,
    #[serde(rename = "name")]
    pub name: String,
    #[serde(rename = "sizeBytes", default)]
    pub size_bytes: Option<u64>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct SessionDebugData {
    #[serde(
        rename = "id",
        default,
        deserialize_with = "crate::api::null_as_default"
    )]
    pub id: String,
    /// Orca environment ID. The server sends `UNDEFINED` when the session has no environment.
    #[serde(rename = "devEnvId", default)]
    pub dev_env_id: Option<String>,
    #[serde(
        rename = "exposedPorts",
        default,
        deserialize_with = "crate::api::null_as_default"
    )]
    pub exposed_ports: Vec<DebugExposedPortData>,
    #[serde(
        rename = "logs",
        default,
        deserialize_with = "crate::api::null_as_default"
    )]
    pub logs: Vec<LogFileInfo>,
    #[serde(rename = "logsException", default)]
    pub logs_exception: Option<String>,
    #[serde(rename = "externalUrl", default)]
    pub external_url: Option<String>,
    #[serde(rename = "getEnvException", default)]
    pub get_env_exception: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ModelInfo {
    #[serde(rename = "id")]
    pub id: String,
    #[serde(
        rename = "name",
        default,
        deserialize_with = "crate::api::null_as_default"
    )]
    pub name: String,
    #[serde(rename = "details", default)]
    pub details: Option<String>,
    #[serde(
        rename = "isDefault",
        default,
        deserialize_with = "crate::api::null_as_default"
    )]
    pub is_default: bool,
    #[serde(rename = "vendor", default)]
    pub vendor: Option<String>,
    /// Values have the form `agent:effort`
    #[serde(
        rename = "thinkingLevels",
        default,
        deserialize_with = "crate::api::null_as_default"
    )]
    pub thinking_levels: Vec<String>,
    #[serde(rename = "supportsFastMode", default)]
    pub supports_fast_mode: Option<bool>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PermissionMode {
    #[serde(rename = "id")]
    pub id: String,
    #[serde(
        rename = "name",
        default,
        deserialize_with = "crate::api::null_as_default"
    )]
    pub name: String,
    #[serde(rename = "description", default)]
    pub description: Option<String>,
    #[serde(rename = "isDefault", default)]
    pub is_default: Option<bool>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct AgentInfo {
    #[serde(rename = "id")]
    pub id: String,
    #[serde(
        rename = "name",
        default,
        deserialize_with = "crate::api::null_as_default"
    )]
    pub name: String,
    #[serde(rename = "isDefault", default)]
    pub is_default: Option<bool>,
    #[serde(rename = "enabled", default = "default_true")]
    pub enabled: bool,
    #[serde(
        rename = "models",
        default,
        deserialize_with = "crate::api::null_as_default"
    )]
    pub models: Vec<ModelInfo>,
    #[serde(
        rename = "permissionModes",
        default,
        deserialize_with = "crate::api::null_as_default"
    )]
    pub permission_modes: Vec<PermissionMode>,
}

fn default_true() -> bool {
    true
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct AvailableAgentsResponse {
    #[serde(
        rename = "agents",
        default,
        deserialize_with = "crate::api::null_as_default"
    )]
    pub agents: Vec<AgentInfo>,
}

#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct ClientInfo {
    #[serde(rename = "clientName")]
    pub client_name: String,
    #[serde(rename = "clientVersion")]
    pub client_version: String,
}

#[derive(Serialize, Debug, Clone, PartialEq, Default)]
pub struct SessionConfigOptions {
    #[serde(rename = "model", skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(rename = "mode", skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(rename = "reasoning_effort", skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
}

#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct LaunchConfig {
    #[serde(rename = "prompt")]
    pub prompt: String,
    #[serde(
        rename = "sessionConfigOptions",
        skip_serializing_if = "Option::is_none"
    )]
    pub session_config_options: Option<SessionConfigOptions>,
}

#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct TaskSessionSpec {
    #[serde(rename = "agentId")]
    pub agent_id: String,
    #[serde(rename = "launchConfig")]
    pub launch_config: LaunchConfig,
    /// When this field is empty, the server uses the personal environment of the repository.
    #[serde(rename = "envConfigId", skip_serializing_if = "Option::is_none")]
    pub env_config_id: Option<String>,
}

#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct CreateTaskRequest {
    #[serde(rename = "repositoryUrl")]
    pub repository_url: String,
    #[serde(rename = "ref", skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
    #[serde(rename = "commitHash", skip_serializing_if = "Option::is_none")]
    pub commit_hash: Option<String>,
    #[serde(rename = "clientInfo")]
    pub client_info: ClientInfo,
    #[serde(rename = "sessions")]
    pub sessions: Vec<TaskSessionSpec>,
    /// Other repositories that the session clones, for example the extra repositories of the environment
    #[serde(rename = "additionalRepos", skip_serializing_if = "Vec::is_empty")]
    pub additional_repos: Vec<AdditionalRepo>,
}

#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct AdditionalRepo {
    #[serde(rename = "repositoryUrl")]
    pub repository_url: String,
    #[serde(rename = "startPoint")]
    pub start_point: StartPoint,
}

/// The branch that the session clones. The server also accepts a commit, but the CLI does not use it.
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct StartPoint {
    #[serde(rename = "branch")]
    pub branch: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Task {
    #[serde(rename = "id")]
    pub id: String,
    #[serde(rename = "name", default)]
    pub name: Option<String>,
    #[serde(
        rename = "sessions",
        default,
        deserialize_with = "crate::api::null_as_default"
    )]
    pub sessions: Vec<Session>,
}

#[derive(Serialize, Debug)]
struct RenameSessionRequest {
    #[serde(rename = "name")]
    name: String,
}

#[derive(Serialize, Debug)]
struct ResumeSessionRequest {
    #[serde(rename = "launchConfig", skip_serializing_if = "Option::is_none")]
    launch_config: Option<LaunchConfig>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn session_without_optional_fields_and_with_unknown_fields() {
        let session: Session = serde_json::from_value(json!({
            "sessionId": "s1",
            "sessionName": "name",
            "ownerId": "o",
            "status": "RUNNING",
            "repositoryState": {"serviceHost": "github.com", "organization": "JetBrains", "repositoryName": "x"},
            "updatedAt": "2026-01-01T00:00:00Z",
            "agentType": "claude",
            "launchedFrom": "jcp",
            "shared": false,
            "someNewField": {"a": 1}
        }))
        .unwrap();
        assert_eq!(session.status, "RUNNING");
        assert_eq!(session.repository_state.full_name(), "JetBrains/x");
        assert!(session.task_id.is_none());
        assert!(session.results_commit_hash.is_none());
    }

    #[test]
    fn session_with_null_results_commit() {
        let session: Session = serde_json::from_value(json!({
            "sessionId": "s1", "status": "FINISHED", "resultsCommitHash": null, "taskId": "t1"
        }))
        .unwrap();
        assert_eq!(session.task_id.as_deref(), Some("t1"));
    }

    #[test]
    fn debug_data_with_missing_optional_fields() {
        let debug: SessionDebugData = serde_json::from_value(json!({
            "id": "s1", "devEnvId": "UNDEFINED", "exposedPorts": [], "logs": [], "externalUrl": "", "extra": 1
        }))
        .unwrap();
        assert!(debug.logs_exception.is_none());
        assert!(debug.get_env_exception.is_none());
    }

    #[test]
    fn agents_with_missing_optional_fields() {
        let agents: AvailableAgentsResponse = serde_json::from_value(json!({
            "agents": [{"id": "claude", "name": "Claude", "enabled": true,
                        "models": [{"id": "m", "name": "M", "isDefault": true, "unknown": 1}]}]
        }))
        .unwrap();
        assert_eq!(agents.agents[0].models[0].id, "m");
        assert!(agents.agents[0].permission_modes.is_empty());
        assert!(agents.agents[0].is_default.is_none());
    }

    #[test]
    fn create_task_request_omits_empty_fields() {
        let request = CreateTaskRequest {
            repository_url: "https://github.com/a/b".into(),
            git_ref: Some("main".into()),
            commit_hash: None,
            client_info: ClientInfo {
                client_name: "jcp".into(),
                client_version: "1".into(),
            },
            sessions: vec![TaskSessionSpec {
                agent_id: "claude".into(),
                launch_config: LaunchConfig {
                    prompt: "p".into(),
                    session_config_options: Some(SessionConfigOptions::default()),
                },
                env_config_id: None,
            }],
            additional_repos: Vec::new(),
        };
        assert_eq!(
            serde_json::to_value(&request).unwrap(),
            json!({
                "repositoryUrl": "https://github.com/a/b",
                "ref": "main",
                "clientInfo": {"clientName": "jcp", "clientVersion": "1"},
                "sessions": [{"agentId": "claude", "launchConfig": {"prompt": "p", "sessionConfigOptions": {}}}]
            })
        );
    }
}
