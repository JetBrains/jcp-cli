//! repo-connections REST API (`/repo-connections`): repositories, branches, VCS providers.

use super::{ApiError, ApiResponse, HttpClient, path_segment};
use serde::{Deserialize, Serialize};

pub struct ReposApi {
    http: HttpClient,
}

impl ReposApi {
    pub fn new(http: HttpClient) -> Self {
        Self { http }
    }

    pub fn list(&self, limit: Option<u64>) -> Result<ApiResponse<RepositoriesResponse>, ApiError> {
        let query: Vec<(&str, String)> = limit
            .map(|l| ("limit", l.to_string()))
            .into_iter()
            .collect();
        self.http.get_json("/repositories", &query)
    }

    pub fn search(&self, q: &str) -> Result<ApiResponse<RepositoriesResponse>, ApiError> {
        self.http
            .get_json("/repositories/search", &[("q", q.to_string())])
    }

    pub fn details(
        &self,
        provider: &str,
        full_name: &str,
        host: Option<&str>,
    ) -> Result<ApiResponse<Repository>, ApiError> {
        let mut query = vec![("fullName", full_name.to_string())];
        if let Some(host) = host {
            query.push(("host", host.to_string()));
        }
        self.http
            .get_json(&format!("/repositories/{}", path_segment(provider)), &query)
    }

    pub fn branches(
        &self,
        provider: &str,
        full_name: &str,
        host: Option<&str>,
        q: Option<&str>,
    ) -> Result<ApiResponse<BranchesResponse>, ApiError> {
        let mut query = vec![("fullName", full_name.to_string())];
        if let Some(host) = host {
            query.push(("host", host.to_string()));
        }
        if let Some(q) = q {
            query.push(("q", q.to_string()));
        }
        self.http.get_json(
            &format!("/repositories/{}/branches", path_segment(provider)),
            &query,
        )
    }

    pub fn providers(&self) -> Result<ApiResponse<ProvidersResponse>, ApiError> {
        self.http.get_json("/providers", &[])
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
pub struct Permissions {
    #[serde(
        rename = "read",
        default,
        deserialize_with = "crate::api::null_as_default"
    )]
    pub read: bool,
    #[serde(
        rename = "write",
        default,
        deserialize_with = "crate::api::null_as_default"
    )]
    pub write: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
pub struct Repository {
    #[serde(rename = "fullName")]
    pub full_name: String,
    #[serde(
        rename = "cloneUrl",
        default,
        deserialize_with = "crate::api::null_as_default"
    )]
    pub clone_url: String,
    #[serde(rename = "browseUrl", default)]
    pub browse_url: Option<String>,
    #[serde(
        rename = "provider",
        default,
        deserialize_with = "crate::api::null_as_default"
    )]
    pub provider: String,
    #[serde(rename = "host", default)]
    pub host: Option<String>,
    #[serde(rename = "permissions", default)]
    pub permissions: Option<Permissions>,
    /// Only `GET /repositories/{provider}` sends this field
    #[serde(rename = "defaultBranch", default)]
    pub default_branch: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct ProviderErrorTarget {
    #[serde(rename = "type", default)]
    pub target_type: Option<String>,
    #[serde(rename = "name", default)]
    pub name: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct ProviderError {
    #[serde(
        rename = "provider",
        default,
        deserialize_with = "crate::api::null_as_default"
    )]
    pub provider: String,
    #[serde(rename = "host", default)]
    pub host: Option<String>,
    /// app_not_installed, app_suspended or provider_unavailable
    #[serde(
        rename = "error",
        default,
        deserialize_with = "crate::api::null_as_default"
    )]
    pub error: String,
    #[serde(rename = "target", default)]
    pub target: Option<ProviderErrorTarget>,
    #[serde(rename = "manageUrl", default)]
    pub manage_url: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct RepositoriesResponse {
    #[serde(
        rename = "data",
        default,
        deserialize_with = "crate::api::null_as_default"
    )]
    pub data: Vec<Repository>,
    #[serde(
        rename = "providerErrors",
        default,
        deserialize_with = "crate::api::null_as_default"
    )]
    pub provider_errors: Vec<ProviderError>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Branch {
    #[serde(rename = "name")]
    pub name: String,
    #[serde(rename = "commitSha", default)]
    pub commit_sha: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct BranchesResponse {
    #[serde(
        rename = "data",
        default,
        deserialize_with = "crate::api::null_as_default"
    )]
    pub data: Vec<Branch>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct AuthorizedProvider {
    #[serde(rename = "provider")]
    pub provider: String,
    #[serde(rename = "manage", default)]
    pub manage: Option<String>,
    #[serde(rename = "username", default)]
    pub username: Option<String>,
    #[serde(rename = "host", default)]
    pub host: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct ProvidersResponse {
    #[serde(
        rename = "authorizedProviders",
        default,
        deserialize_with = "crate::api::null_as_default"
    )]
    pub authorized_providers: Vec<AuthorizedProvider>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn repositories_with_null_lists_and_objects() {
        // The shape that production repo-connections sends
        let response: RepositoriesResponse = serde_json::from_value(json!({
            "data": [{
                "fullName": "MARINATOR/marinator",
                "cloneUrl": "https://git.jetbrains.team/MARINATOR/marinator",
                "browseUrl": "https://jetbrains.team/p/MARINATOR/repositories/marinator",
                "provider": "space",
                "host": null,
                "permissions": null
            }],
            "providerErrors": null
        }))
        .unwrap();
        assert_eq!(response.data.len(), 1);
        assert!(response.provider_errors.is_empty());
        let empty: RepositoriesResponse =
            serde_json::from_value(json!({"data": null, "providerErrors": null})).unwrap();
        assert!(empty.data.is_empty());
    }

    #[test]
    fn repositories_without_optional_fields_and_with_unknown_fields() {
        let response: RepositoriesResponse = serde_json::from_value(json!({
            "data": [{"fullName": "a/b", "cloneUrl": "https://github.com/a/b", "browseUrl": "x",
                      "provider": "github", "newField": true}]
        }))
        .unwrap();
        assert_eq!(response.data[0].full_name, "a/b");
        assert!(response.data[0].permissions.is_none());
        assert!(response.provider_errors.is_empty());
    }

    #[test]
    fn repository_details_with_null_default_branch() {
        let repo: Repository = serde_json::from_value(json!({
            "fullName": "a/b", "cloneUrl": "c", "browseUrl": "b", "provider": "github", "defaultBranch": null
        }))
        .unwrap();
        assert!(repo.default_branch.is_none());
    }
}
