//! `jcp env`: environment configurations from the Air backend.

use super::{
    CliError,
    format::{or_dash, table},
    repo::{RepoArg, normalize_repo_url},
    session::is_uuid,
    usage,
};
use crate::api::{
    ApiError,
    env_configs::{EnvConfig, EnvConfigDetails, EnvConfigsApi},
};
use serde_json::Value;
use std::io::Write;

/// Selects one environment by ID or by name.
///
/// An exact ID match wins. Otherwise the name must match exactly one environment.
pub fn select_env<'a>(configs: &'a [EnvConfig], query: &str) -> Result<&'a EnvConfig, CliError> {
    if let Some(config) = configs.iter().find(|c| c.id.eq_ignore_ascii_case(query)) {
        return Ok(config);
    }
    let matches: Vec<&EnvConfig> = configs.iter().filter(|c| c.name == query).collect();
    match matches.as_slice() {
        [one] => Ok(one),
        [] => Err(usage(format!(
            "No environment has the ID or the name `{query}`. Run `jcp env list` to see the environments."
        ))),
        many => {
            let rows: Vec<Vec<String>> = many
                .iter()
                .map(|c| vec![c.id.clone(), c.name.clone(), c.full_name()])
                .collect();
            Err(usage(format!(
                "{} environments have the name `{query}`. Use the ID:\n{}",
                many.len(),
                table(&["ID", "NAME", "REPOSITORY"], &rows).trim_end()
            )))
        }
    }
}

/// Gets the details of one environment by ID or by name.
pub fn resolve(api: &EnvConfigsApi, query: &str) -> Result<(String, EnvConfigDetails), CliError> {
    let id = if is_uuid(query) {
        query.to_string()
    } else {
        let list = api.list(true, None)?.value;
        select_env(&list, query)?.id.clone()
    };
    let response = api.get(&id)?;
    Ok((response.raw, response.value))
}

fn repository_column(config: &EnvConfig) -> String {
    let extra = config
        .additional_repositories
        .as_ref()
        .map(Vec::len)
        .unwrap_or(0);
    if extra > 0 {
        format!("{} +{extra}", config.full_name())
    } else {
        config.full_name()
    }
}

/// Keeps the environments of the repository. A full name matches on all hosts.
fn matches_repo(config: &EnvConfig, repo: &RepoArg) -> bool {
    match repo {
        RepoArg::Url { url, .. } => {
            normalize_repo_url(&config.repository_url()) == normalize_repo_url(url)
        }
        RepoArg::FullName(name) => config.full_name().eq_ignore_ascii_case(name),
    }
}

/// Makes the `env list` table.
pub fn env_table(configs: &[&EnvConfig]) -> String {
    let rows: Vec<Vec<String>> = configs
        .iter()
        .map(|c| {
            vec![
                c.id.clone(),
                c.name.clone(),
                repository_column(c),
                if c.shared { "shared" } else { "me" }.to_string(),
                or_dash(c.access_level.as_deref()),
                or_dash(c.last_used_at.as_deref()),
                if c.is_draft { "yes" } else { "no" }.to_string(),
            ]
        })
        .collect();
    table(
        &[
            "ID",
            "NAME",
            "REPOSITORY",
            "OWNER",
            "ACCESS",
            "LAST USED",
            "DRAFT",
        ],
        &rows,
    )
}

pub struct ListArgs<'a> {
    pub repo: Option<&'a str>,
    pub mine: bool,
    pub shared: bool,
    pub project: Option<&'a str>,
    pub drafts: bool,
    pub json: bool,
}

pub fn list(api: &EnvConfigsApi, args: &ListArgs, out: &mut impl Write) -> Result<(), CliError> {
    let repo = args.repo.map(RepoArg::parse).transpose()?;
    let response = api.list(args.drafts, args.project)?;
    let keep = |c: &EnvConfig| {
        (!args.mine || !c.shared)
            && (!args.shared || c.shared)
            && repo.as_ref().is_none_or(|r| matches_repo(c, r))
    };
    if args.json {
        if !args.mine && !args.shared && repo.is_none() {
            writeln!(out, "{}", response.raw)?;
            return Ok(());
        }
        // The server JSON of the kept environments. The typed list has the same order as the server list.
        let raw: Vec<Value> =
            serde_json::from_str(&response.raw).map_err(|source| ApiError::Decode {
                path: "/env-configs".to_string(),
                body: response.raw.chars().take(500).collect(),
                source,
            })?;
        let kept: Vec<Value> = raw
            .into_iter()
            .zip(&response.value)
            .filter(|(_, c)| keep(c))
            .map(|(value, _)| value)
            .collect();
        writeln!(out, "{}", Value::Array(kept))?;
        return Ok(());
    }
    let configs: Vec<&EnvConfig> = response.value.iter().filter(|c| keep(c)).collect();
    if configs.is_empty() {
        writeln!(out, "No environments.")?;
        return Ok(());
    }
    write!(out, "{}", env_table(&configs))?;
    Ok(())
}

pub fn get(
    api: &EnvConfigsApi,
    query: &str,
    json: bool,
    out: &mut impl Write,
) -> Result<(), CliError> {
    let (raw, details) = resolve(api, query)?;
    if json {
        writeln!(out, "{raw}")?;
        return Ok(());
    }
    write!(out, "{}", describe(&details))?;
    Ok(())
}

/// Makes the `env get` text. Secret values stay as the server sends them (masked).
pub fn describe(details: &EnvConfigDetails) -> String {
    let c = &details.config;
    let mut s = String::new();
    let mut line = |name: &str, value: String| s.push_str(&format!("{name:<18}{value}\n"));
    line("ID", c.id.clone());
    line("Name", c.name.clone());
    line("Repository", c.repository_url());
    if let Some(repos) = &c.additional_repositories {
        for r in repos {
            line(
                "  Additional",
                match &r.git_ref {
                    Some(git_ref) => format!("{} ({git_ref})", r.url),
                    None => r.url.clone(),
                },
            );
        }
    }
    line(
        "Owner",
        if c.shared { "shared with you" } else { "me" }.to_string(),
    );
    line("Access", or_dash(c.access_level.as_deref()));
    line("Draft", if c.is_draft { "yes" } else { "no" }.to_string());
    line("Project", or_dash(c.org_project_id.as_deref()));
    line("Network access", or_dash(c.network_access.as_deref()));
    if !c.predefined_domain_lists.is_empty() {
        line("Domain lists", c.predefined_domain_lists.join(", "));
    }
    line(
        "Allowed domains",
        if c.allowed_domains.is_empty() {
            "-".to_string()
        } else {
            c.allowed_domains.join(", ")
        },
    );
    line("Region", or_dash(c.region_key.as_deref()));
    line("Instance type", or_dash(c.instance_type_key.as_deref()));
    line(
        "Ports",
        match &c.exposed_ports {
            Some(ports) => ports
                .iter()
                .map(u64::to_string)
                .collect::<Vec<_>>()
                .join(", "),
            None => "default".to_string(),
        },
    );
    match &c.repository_startup_scripts {
        Some(scripts) if !scripts.is_empty() => {
            for script in scripts {
                line(
                    "Startup script",
                    format!(
                        "{} {}",
                        script.repository_name.as_deref().unwrap_or(&script.url),
                        script
                            .startup_script_path
                            .as_deref()
                            .unwrap_or("(default path)")
                    ),
                );
            }
        }
        _ => line("Startup script", "-".to_string()),
    }
    if let Some(timeout) = c.startup_script_timeout_seconds {
        line("Script timeout", format!("{timeout} s"));
    }
    line("Last used", or_dash(c.last_used_at.as_deref()));
    let names = |vars: &[crate::api::env_configs::EnvVariable]| {
        if vars.is_empty() {
            "-".to_string()
        } else {
            vars.iter()
                .map(|v| v.key.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        }
    };
    line("Variables", names(&details.variables));
    line("Shared secrets", names(&details.shared_secrets));
    line("My secrets", names(&details.personal_secrets));
    line(
        "External secrets",
        if details.external_secrets.is_empty() {
            "-".to_string()
        } else {
            details
                .external_secrets
                .iter()
                .map(|v| v.key.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        },
    );
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(id: &str, name: &str) -> EnvConfig {
        EnvConfig {
            id: id.into(),
            name: name.into(),
            service_host: "github.com".into(),
            organization: "JetBrains".into(),
            repository_name: "x".into(),
            ..Default::default()
        }
    }

    #[test]
    fn selects_env_by_id() {
        let configs = [config("id-1", "A"), config("id-2", "id-1")];
        assert_eq!(select_env(&configs, "id-1").unwrap().id, "id-1");
    }

    #[test]
    fn selects_env_by_unique_name() {
        let configs = [config("id-1", "A"), config("id-2", "B")];
        assert_eq!(select_env(&configs, "B").unwrap().id, "id-2");
    }

    #[test]
    fn no_env_with_name() {
        let configs = [config("id-1", "A")];
        let e = select_env(&configs, "Z").unwrap_err().to_string();
        assert!(e.contains("No environment"), "{e}");
    }

    #[test]
    fn two_envs_with_same_name_list_the_matches() {
        let configs = [
            config("id-1", "A"),
            config("id-2", "A"),
            config("id-3", "B"),
        ];
        let e = select_env(&configs, "A").unwrap_err().to_string();
        assert!(e.contains("2 environments"), "{e}");
        assert!(
            e.contains("id-1") && e.contains("id-2") && !e.contains("id-3"),
            "{e}"
        );
    }

    #[test]
    fn table_shows_additional_repos_owner_and_draft() {
        let mut a = config("id-1", "A");
        a.additional_repositories = Some(vec![crate::api::env_configs::AdditionalRepository {
            url: "https://github.com/a/b".into(),
            git_ref: None,
        }]);
        a.access_level = Some("MANAGE".into());
        let mut b = config("id-2", "B");
        b.shared = true;
        b.is_draft = true;
        let out = env_table(&[&a, &b]);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(
            lines[0],
            "ID    NAME  REPOSITORY      OWNER   ACCESS  LAST USED  DRAFT"
        );
        assert_eq!(
            lines[1],
            "id-1  A     JetBrains/x +1  me      MANAGE  -          no"
        );
        assert_eq!(
            lines[2],
            "id-2  B     JetBrains/x     shared  -       -          yes"
        );
    }

    #[test]
    fn describe_shows_names_but_not_values() {
        let details = EnvConfigDetails {
            config: config("id-1", "A"),
            variables: vec![crate::api::env_configs::EnvVariable {
                key: "FOO".into(),
                value: "bar".into(),
                is_secret: false,
                description: None,
            }],
            shared_secrets: vec![crate::api::env_configs::EnvVariable {
                key: "TOKEN".into(),
                value: "***".into(),
                is_secret: true,
                description: None,
            }],
            ..Default::default()
        };
        let out = describe(&details);
        assert!(out.contains("FOO") && out.contains("TOKEN"), "{out}");
        assert!(!out.contains("bar") && !out.contains("***"), "{out}");
    }
}
