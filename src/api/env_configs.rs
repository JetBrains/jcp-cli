//! Air backend REST API (`/air-backend`): environment configurations.

use super::{ApiError, ApiResponse, HttpClient, path_segment};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

pub struct EnvConfigsApi {
    http: HttpClient,
}

impl EnvConfigsApi {
    pub fn new(http: HttpClient) -> Self {
        Self { http }
    }

    pub fn list(
        &self,
        include_drafts: bool,
        org_project_id: Option<&str>,
    ) -> Result<ApiResponse<Vec<EnvConfig>>, ApiError> {
        let mut query = vec![("includeDrafts", include_drafts.to_string())];
        if let Some(project) = org_project_id {
            query.push(("orgProjectId", project.to_string()));
        }
        self.http.get_json("/env-configs", &query)
    }

    pub fn get(&self, id: &str) -> Result<ApiResponse<EnvConfigDetails>, ApiError> {
        self.http
            .get_json(&format!("/env-configs/{}", path_segment(id)), &[])
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct EnvConfigOwner {
    #[serde(
        rename = "type",
        default,
        deserialize_with = "crate::api::null_as_default"
    )]
    pub owner_type: String,
    #[serde(
        rename = "id",
        default,
        deserialize_with = "crate::api::null_as_default"
    )]
    pub id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct AdditionalRepository {
    #[serde(rename = "url")]
    pub url: String,
    #[serde(rename = "ref", default)]
    pub git_ref: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RepositoryStartupScript {
    #[serde(
        rename = "url",
        default,
        deserialize_with = "crate::api::null_as_default"
    )]
    pub url: String,
    #[serde(rename = "repositoryName", default)]
    pub repository_name: Option<String>,
    #[serde(rename = "startupScriptPath", default)]
    pub startup_script_path: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct EnvConfig {
    #[serde(rename = "id")]
    pub id: String,
    #[serde(
        rename = "name",
        default,
        deserialize_with = "crate::api::null_as_default"
    )]
    pub name: String,
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
    #[serde(rename = "provider", default)]
    pub provider: Option<String>,
    #[serde(rename = "orgProjectId", default)]
    pub org_project_id: Option<String>,
    #[serde(rename = "additionalRepositories", default)]
    pub additional_repositories: Option<Vec<AdditionalRepository>>,
    /// UNRESTRICTED, RESTRICTED or DISABLED
    #[serde(rename = "networkAccess", default)]
    pub network_access: Option<String>,
    #[serde(
        rename = "predefinedDomainLists",
        default,
        deserialize_with = "crate::api::null_as_default"
    )]
    pub predefined_domain_lists: Vec<String>,
    #[serde(
        rename = "allowedDomains",
        default,
        deserialize_with = "crate::api::null_as_default"
    )]
    pub allowed_domains: Vec<String>,
    #[serde(rename = "regionKey", default)]
    pub region_key: Option<String>,
    #[serde(rename = "instanceTypeKey", default)]
    pub instance_type_key: Option<String>,
    #[serde(rename = "exposedPorts", default)]
    pub exposed_ports: Option<Vec<u64>>,
    #[serde(rename = "startupScriptTimeoutSeconds", default)]
    pub startup_script_timeout_seconds: Option<u64>,
    #[serde(rename = "repositoryStartupScripts", default)]
    pub repository_startup_scripts: Option<Vec<RepositoryStartupScript>>,
    #[serde(
        rename = "isDraft",
        default,
        deserialize_with = "crate::api::null_as_default"
    )]
    pub is_draft: bool,
    #[serde(rename = "owner", default)]
    pub owner: Option<EnvConfigOwner>,
    /// USE or MANAGE
    #[serde(rename = "accessLevel", default)]
    pub access_level: Option<String>,
    /// True when the current user is not the owner
    #[serde(
        rename = "shared",
        default,
        deserialize_with = "crate::api::null_as_default"
    )]
    pub shared: bool,
    #[serde(rename = "lastUsedAt", default)]
    pub last_used_at: Option<String>,
}

impl EnvConfig {
    /// `organization/name`
    pub fn full_name(&self) -> String {
        format!("{}/{}", self.organization, self.repository_name)
    }

    /// The repository URL, as the Air web UI makes it
    pub fn repository_url(&self) -> String {
        format!(
            "https://{}/{}/{}",
            self.service_host, self.organization, self.repository_name
        )
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct EnvVariable {
    #[serde(rename = "key")]
    pub key: String,
    /// Secret values come masked from the server
    #[serde(
        rename = "value",
        default,
        deserialize_with = "crate::api::null_as_default"
    )]
    pub value: String,
    #[serde(
        rename = "isSecret",
        default,
        deserialize_with = "crate::api::null_as_default"
    )]
    pub is_secret: bool,
    #[serde(rename = "description", default)]
    pub description: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ExternalSecret {
    #[serde(rename = "key")]
    pub key: String,
    #[serde(rename = "externalSecretReference", default)]
    pub reference: Option<JsonValue>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct EnvConfigDetails {
    #[serde(flatten)]
    pub config: EnvConfig,
    #[serde(
        rename = "variables",
        default,
        deserialize_with = "crate::api::null_as_default"
    )]
    pub variables: Vec<EnvVariable>,
    #[serde(
        rename = "sharedSecrets",
        default,
        deserialize_with = "crate::api::null_as_default"
    )]
    pub shared_secrets: Vec<EnvVariable>,
    #[serde(
        rename = "personalSecrets",
        default,
        deserialize_with = "crate::api::null_as_default"
    )]
    pub personal_secrets: Vec<EnvVariable>,
    #[serde(
        rename = "externalSecrets",
        default,
        deserialize_with = "crate::api::null_as_default"
    )]
    pub external_secrets: Vec<ExternalSecret>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn env_config_without_optional_fields_and_with_unknown_fields() {
        let config: EnvConfig = serde_json::from_value(json!({
            "id": "e1", "name": "n", "serviceHost": "github.com", "organization": "JetBrains",
            "repositoryName": "x", "networkAccess": "UNRESTRICTED", "predefinedDomainLists": [],
            "allowedDomains": [], "isDraft": false, "owner": {"type": "USER", "id": "u"},
            "accessLevel": "MANAGE", "shared": false, "hasBeenShared": false, "sharedWithUsers": [],
            "futureField": 1
        }))
        .unwrap();
        assert_eq!(config.repository_url(), "https://github.com/JetBrains/x");
        assert!(config.additional_repositories.is_none());
        assert!(config.last_used_at.is_none());
    }

    #[test]
    fn env_config_details_reads_nested_and_flat_fields() {
        let details: EnvConfigDetails = serde_json::from_value(json!({
            "id": "e1", "name": "n", "serviceHost": "github.com", "organization": "o", "repositoryName": "r",
            "variables": [{"key": "A", "value": "1"}],
            "sharedSecrets": [{"key": "S", "value": "***", "isSecret": true}]
        }))
        .unwrap();
        assert_eq!(details.config.id, "e1");
        assert_eq!(details.variables[0].key, "A");
        assert_eq!(details.shared_secrets[0].value, "***");
    }
}
