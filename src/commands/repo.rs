//! `jcp repo`: repositories, branches and VCS providers from repo-connections.

use super::{CliError, format::table, usage};
use crate::api::repos::{ProviderError, ReposApi, Repository};
use std::io::Write;
use url::Url;

/// A repository argument: a URL (HTTPS or SSH) or a full name (`owner/name`).
#[derive(Debug, Clone, PartialEq)]
pub enum RepoArg {
    /// A URL, converted to `https://host/full/name`
    Url {
        url: String,
        host: String,
        full_name: String,
    },
    FullName(String),
}

impl RepoArg {
    pub fn parse(input: &str) -> Result<Self, CliError> {
        let input = input.trim();
        let bad = || {
            usage(format!(
                "`{input}` is not a repository URL or a full name like `owner/name`."
            ))
        };

        // SSH form: git@host:owner/name.git
        if !input.contains("://")
            && let Some((user_host, path)) = input.split_once(':')
        {
            let host = user_host.rsplit('@').next().unwrap_or(user_host);
            return Self::from_parts(host, path).ok_or_else(bad);
        }

        if input.contains("://") {
            let url = Url::parse(input).map_err(|_| bad())?;
            let host = url.host_str().ok_or_else(bad)?;
            return Self::from_parts(host, url.path()).ok_or_else(bad);
        }

        let path = input.trim_matches('/');
        // `github.com/owner/name`: the first part has a dot, so it is a host.
        if let Some((first, rest)) = path.split_once('/')
            && first.contains('.')
        {
            return Self::from_parts(first, rest).ok_or_else(bad);
        }
        let full_name = strip_git_suffix(path);
        let parts: Vec<&str> = full_name.split('/').collect();
        if parts.len() < 2 || parts.iter().any(|p| p.is_empty() || p.contains(' ')) {
            return Err(bad());
        }
        Ok(RepoArg::FullName(full_name.to_string()))
    }

    fn from_parts(host: &str, path: &str) -> Option<Self> {
        let full_name = strip_git_suffix(path.trim_matches('/'));
        let parts: Vec<&str> = full_name.split('/').collect();
        if host.is_empty() || parts.len() < 2 || parts.iter().any(|p| p.is_empty()) {
            return None;
        }
        let host = host.to_lowercase();
        Some(RepoArg::Url {
            url: format!("https://{host}/{full_name}"),
            host,
            full_name: full_name.to_string(),
        })
    }

    pub fn full_name(&self) -> &str {
        match self {
            RepoArg::Url { full_name, .. } => full_name,
            RepoArg::FullName(name) => name,
        }
    }
}

fn strip_git_suffix(s: &str) -> &str {
    s.strip_suffix(".git").unwrap_or(s)
}

/// Makes a URL comparable: no scheme, no `.git`, no trailing slash, lower case.
pub fn normalize_repo_url(url: &str) -> String {
    let url = url.trim().trim_end_matches('/');
    let url = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    strip_git_suffix(url).to_lowercase()
}

/// The repo-connections provider name for a public VCS host.
pub fn provider_for_host(host: &str) -> Option<&'static str> {
    match host.to_lowercase().as_str() {
        "github.com" => Some("github"),
        "gitlab.com" => Some("gitlab"),
        "bitbucket.org" => Some("bitbucket"),
        _ => None,
    }
}

/// Providers that need the `host` query parameter
pub fn provider_needs_host(provider: &str) -> bool {
    matches!(provider, "github-enterprise" | "gitlab-self-managed")
}

/// Finds a repository with repo search. The full name must match exactly (case-insensitive).
pub fn find_repository(
    api: &ReposApi,
    arg: &RepoArg,
    err: &mut impl Write,
) -> Result<Repository, CliError> {
    let full_name = arg.full_name();
    let response = api.search(full_name)?.value;
    print_provider_errors(&response.provider_errors, err)?;
    let found = response.data.into_iter().find(|r| {
        r.full_name.eq_ignore_ascii_case(full_name)
            && match arg {
                RepoArg::Url { url, .. } => {
                    normalize_repo_url(&r.clone_url) == normalize_repo_url(url)
                }
                RepoArg::FullName(_) => true,
            }
    });
    found.ok_or_else(|| {
        usage(format!(
            "Cannot find the repository `{full_name}`. Run `jcp repo list` to see the repositories that you can use, and `jcp repo providers` to see the VCS accounts that are connected to Air."
        ))
    })
}

/// Prints a warning for each provider error, with the URL where the user can fix it.
pub fn print_provider_errors(
    errors: &[ProviderError],
    err: &mut impl Write,
) -> Result<(), CliError> {
    for e in errors {
        let mut line = format!("Warning: {}", e.provider);
        if let Some(host) = &e.host {
            line.push_str(&format!(" ({host})"));
        }
        line.push_str(&format!(": {}", e.error.replace('_', " ")));
        if let Some(target) = &e.target
            && let Some(name) = &target.name
        {
            match &target.target_type {
                Some(t) => line.push_str(&format!(" for {} {name}", t.to_lowercase())),
                None => line.push_str(&format!(" for {name}")),
            }
        }
        line.push('.');
        if let Some(url) = &e.manage_url {
            line.push_str(&format!(" Fix it at {url}"));
        }
        writeln!(err, "{line}")?;
    }
    Ok(())
}

fn permissions(repo: &Repository) -> String {
    match &repo.permissions {
        Some(p) => format!(
            "{}/{}",
            if p.read { "yes" } else { "no" },
            if p.write { "yes" } else { "no" }
        ),
        None => "-".to_string(),
    }
}

pub fn list(
    api: &ReposApi,
    search: Option<&str>,
    limit: Option<u64>,
    json: bool,
    out: &mut impl Write,
    err: &mut impl Write,
) -> Result<(), CliError> {
    let response = match search {
        Some(q) => api.search(q)?,
        None => api.list(limit)?,
    };
    if json {
        writeln!(out, "{}", response.raw)?;
        return Ok(());
    }
    let mut data = response.value.data;
    if let Some(limit) = limit {
        data.truncate(limit as usize);
    }
    let rows: Vec<Vec<String>> = data
        .iter()
        .map(|r| {
            vec![
                r.full_name.clone(),
                r.provider.clone(),
                permissions(r),
                r.clone_url.clone(),
            ]
        })
        .collect();
    if rows.is_empty() {
        writeln!(out, "No repositories.")?;
    } else {
        write!(
            out,
            "{}",
            table(&["FULL NAME", "PROVIDER", "READ/WRITE", "CLONE URL"], &rows)
        )?;
    }
    print_provider_errors(&response.value.provider_errors, err)?;
    Ok(())
}

pub fn branches(
    api: &ReposApi,
    repo: &str,
    search: Option<&str>,
    json: bool,
    out: &mut impl Write,
    err: &mut impl Write,
) -> Result<(), CliError> {
    let arg = RepoArg::parse(repo)?;
    let (provider, host) = match &arg {
        RepoArg::Url { host, .. } if provider_for_host(host).is_some() => (
            provider_for_host(host).unwrap_or_default().to_string(),
            None,
        ),
        _ => {
            let found = find_repository(api, &arg, err)?;
            let host = found.host.filter(|_| provider_needs_host(&found.provider));
            (found.provider, host)
        }
    };
    let response = api.branches(&provider, arg.full_name(), host.as_deref(), search)?;
    if json {
        writeln!(out, "{}", response.raw)?;
        return Ok(());
    }
    let rows: Vec<Vec<String>> = response
        .value
        .data
        .iter()
        .map(|b| vec![b.name.clone(), b.commit_sha.clone().unwrap_or_default()])
        .collect();
    if rows.is_empty() {
        writeln!(out, "No branches.")?;
        return Ok(());
    }
    write!(out, "{}", table(&["NAME", "COMMIT"], &rows))?;
    Ok(())
}

pub fn providers(api: &ReposApi, json: bool, out: &mut impl Write) -> Result<(), CliError> {
    let response = api.providers()?;
    if json {
        writeln!(out, "{}", response.raw)?;
        return Ok(());
    }
    let rows: Vec<Vec<String>> = response
        .value
        .authorized_providers
        .iter()
        .map(|p| {
            vec![
                p.provider.clone(),
                p.username.clone().unwrap_or_default(),
                p.host.clone().unwrap_or_default(),
                p.manage.clone().unwrap_or_default(),
            ]
        })
        .collect();
    if rows.is_empty() {
        writeln!(out, "No VCS accounts are connected.")?;
        return Ok(());
    }
    write!(
        out,
        "{}",
        table(&["PROVIDER", "USERNAME", "HOST", "MANAGE"], &rows)
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(url: &str, host: &str, full_name: &str) -> RepoArg {
        RepoArg::Url {
            url: url.into(),
            host: host.into(),
            full_name: full_name.into(),
        }
    }

    #[test]
    fn parses_https_url() {
        let expected = url(
            "https://github.com/JetBrains/marinator",
            "github.com",
            "JetBrains/marinator",
        );
        assert_eq!(
            RepoArg::parse("https://github.com/JetBrains/marinator").unwrap(),
            expected
        );
        assert_eq!(
            RepoArg::parse("https://github.com/JetBrains/marinator.git").unwrap(),
            expected
        );
        assert_eq!(
            RepoArg::parse("https://github.com/JetBrains/marinator/").unwrap(),
            expected
        );
        assert_eq!(
            RepoArg::parse("github.com/JetBrains/marinator").unwrap(),
            expected
        );
    }

    #[test]
    fn parses_ssh_url_as_https() {
        let expected = url(
            "https://github.com/JetBrains/marinator",
            "github.com",
            "JetBrains/marinator",
        );
        assert_eq!(
            RepoArg::parse("git@github.com:JetBrains/marinator.git").unwrap(),
            expected
        );
        assert_eq!(
            RepoArg::parse("ssh://git@github.com/JetBrains/marinator.git").unwrap(),
            expected
        );
    }

    #[test]
    fn parses_nested_gitlab_groups() {
        assert_eq!(
            RepoArg::parse("https://gitlab.com/a/b/c").unwrap(),
            url("https://gitlab.com/a/b/c", "gitlab.com", "a/b/c")
        );
    }

    #[test]
    fn parses_full_name() {
        assert_eq!(
            RepoArg::parse("JetBrains/marinator").unwrap(),
            RepoArg::FullName("JetBrains/marinator".into())
        );
    }

    #[test]
    fn rejects_bad_input() {
        assert!(RepoArg::parse("marinator").is_err());
        assert!(RepoArg::parse("https://github.com/only").is_err());
        assert!(RepoArg::parse("a//b").is_err());
        assert!(RepoArg::parse("").is_err());
    }

    #[test]
    fn normalizes_urls() {
        assert_eq!(
            normalize_repo_url("https://GitHub.com/JetBrains/X.git/"),
            normalize_repo_url("https://github.com/jetbrains/x")
        );
    }

    #[test]
    fn provider_of_public_hosts() {
        assert_eq!(provider_for_host("GitHub.com"), Some("github"));
        assert_eq!(provider_for_host("gitlab.com"), Some("gitlab"));
        assert_eq!(provider_for_host("git.example.com"), None);
    }

    #[test]
    fn provider_error_warning_has_manage_url() {
        let mut err = Vec::new();
        print_provider_errors(
            &[ProviderError {
                provider: "github".into(),
                host: None,
                error: "app_not_installed".into(),
                target: Some(crate::api::repos::ProviderErrorTarget {
                    target_type: Some("ORG".into()),
                    name: Some("JetBrains".into()),
                }),
                manage_url: Some("https://github.com/apps/x".into()),
            }],
            &mut err,
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(err).unwrap(),
            "Warning: github: app not installed for org JetBrains. Fix it at https://github.com/apps/x\n"
        );
    }
}
