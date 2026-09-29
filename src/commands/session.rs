//! `jcp session`: start, list and manage Air Cloud sessions (agent-spawner).

use super::{
    CliError, Clock, Services, agents,
    diag::{self, WatchOptions},
    env::select_env,
    format::{human_size, or_dash, table},
    repo::{RepoArg, find_repository, normalize_repo_url, provider_for_host, provider_needs_host},
    usage,
};
use crate::{
    GitRemoteInfo, GitTool,
    api::{
        ApiError,
        env_configs::EnvConfig,
        spawner::{
            AdditionalRepo, ClientInfo, CreateTaskRequest, FAILED_STATUSES, FINAL_STATUSES,
            LaunchConfig, Session, SessionConfigOptions, SpawnerApi, StartPoint, TaskSessionSpec,
            USER_INPUT_REQUIRED,
        },
    },
};
use serde_json::json;
use std::{
    fs,
    io::{BufRead, Write},
    path::{Path, PathBuf},
    time::Duration,
};
use url::Url;

/// `clientName` of the tasks that jcp starts
pub const CLIENT_NAME: &str = "jcp";

/// Checks the UUID form `8-4-4-4-12` of hex digits.
pub fn is_uuid(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    parts.len() == 5
        && parts
            .iter()
            .zip([8, 4, 4, 4, 12])
            .all(|(p, len)| p.len() == len && p.chars().all(|c| c.is_ascii_hexdigit()))
}

/// Gets the session ID from a session ID or an Air URL (`/task/<id>[/...]` or `/session/<id>[/...]`).
///
/// A task ID is also a session ID: the first session of a task has the ID of the task.
pub fn parse_session_ref(input: &str) -> Result<String, CliError> {
    let input = input.trim();
    if is_uuid(input) {
        return Ok(input.to_string());
    }
    let bad = || {
        usage(format!(
            "`{input}` is not a session ID or an Air task URL (for example, https://air.jetbrains.cloud/org/<org>/task/<id>)."
        ))
    };
    let url = Url::parse(input).map_err(|_| bad())?;
    let segments: Vec<&str> = url.path_segments().map(|s| s.collect()).unwrap_or_default();
    segments
        .windows(2)
        .find(|w| (w[0] == "task" || w[0] == "session") && is_uuid(w[1]))
        .map(|w| w[1].to_string())
        .ok_or_else(bad)
}

// ---------------------------------------------------------------------------------------------
// session start
// ---------------------------------------------------------------------------------------------

/// A repository that `--repo` points to
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedRepo {
    pub url: String,
    pub full_name: String,
    pub provider: Option<String>,
    /// The `host` parameter for providers that need it
    pub host: Option<String>,
}

/// The repository, ref and commit of a new task
#[derive(Debug, Clone, PartialEq)]
pub struct RepoPlan {
    pub url: String,
    pub full_name: String,
    pub provider: Option<String>,
    pub host: Option<String>,
    pub git_ref: Option<String>,
    pub commit_hash: Option<String>,
}

impl RepoPlan {
    /// True when the ref must come from the default branch of the repository
    pub fn needs_default_branch(&self) -> bool {
        self.git_ref.is_none() && self.commit_hash.is_none()
    }
}

/// Selects the repository: `--repo`, then the repository of `--env`, then the local `origin`.
///
/// The ref is `--branch`, then the local branch (local repository only). A local detached HEAD gives the commit.
pub fn plan_repository(
    repo: Option<&ResolvedRepo>,
    env: Option<&EnvConfig>,
    local: Option<&GitRemoteInfo>,
    branch: Option<&str>,
) -> Result<RepoPlan, CliError> {
    let branch = branch.map(str::to_string);
    if let (Some(repo), Some(env)) = (repo, env)
        && normalize_repo_url(&repo.url) != normalize_repo_url(&env.repository_url())
    {
        return Err(usage(format!(
            "The environment `{}` is for the repository {}, but --repo is {}. Use the same repository, or remove --repo.",
            env.name,
            env.repository_url(),
            repo.url
        )));
    }
    if let Some(repo) = repo {
        return Ok(RepoPlan {
            url: repo.url.clone(),
            full_name: repo.full_name.clone(),
            provider: repo.provider.clone(),
            host: repo.host.clone(),
            git_ref: branch,
            commit_hash: None,
        });
    }
    if let Some(env) = env {
        let provider = env
            .provider
            .clone()
            .or_else(|| provider_for_host(&env.service_host).map(str::to_string));
        let host = provider
            .as_deref()
            .filter(|p| provider_needs_host(p))
            .map(|_| env.service_host.clone());
        return Ok(RepoPlan {
            url: env.repository_url(),
            full_name: env.full_name(),
            provider,
            host,
            git_ref: branch,
            commit_hash: None,
        });
    }
    if let Some(local) = local {
        let RepoArg::Url {
            url,
            host,
            full_name,
        } = RepoArg::parse(&local.url)?
        else {
            return Err(usage(format!(
                "The `origin` remote `{}` is not a URL. Use --repo.",
                local.url
            )));
        };
        let detached = local.branch == "HEAD";
        let (git_ref, commit_hash) = match branch {
            Some(branch) => (Some(branch), None),
            None if detached => (None, Some(local.revision.clone())),
            None => (Some(local.branch.clone()), None),
        };
        return Ok(RepoPlan {
            url,
            full_name,
            provider: provider_for_host(&host).map(str::to_string),
            host: None,
            git_ref,
            commit_hash,
        });
    }
    Err(usage(
        "No repository. Use --repo or --env, or run the command in a git repository with an `origin` remote.",
    ))
}

/// Makes the plan of an extra repository of the environment. The session clones its default branch.
fn plan_additional_repository(url: &str) -> Result<RepoPlan, CliError> {
    let RepoArg::Url {
        url,
        host,
        full_name,
    } = RepoArg::parse(url)?
    else {
        return Err(usage(format!(
            "The additional repository `{url}` is not a URL."
        )));
    };
    Ok(RepoPlan {
        url,
        full_name,
        provider: provider_for_host(&host).map(str::to_string),
        host: None,
        git_ref: None,
        commit_hash: None,
    })
}

/// repo-connections gives 404 "No token found for provider X" when the user did not connect that VCS account.
fn is_missing_vcs_token(error: &CliError) -> bool {
    matches!(error, CliError::Api(ApiError::NotFound { body, .. }) if body.contains("No token found"))
}

/// Gets the default branch of the repository from repo-connections. The server needs a ref for each task.
///
/// When the provider is not known from the host (for example, Space or a self-hosted server), repo search gives it.
fn default_branch(
    services: &Services,
    plan: &RepoPlan,
    err: &mut impl Write,
) -> Result<String, CliError> {
    let api = services.repos()?;
    let (provider, full_name, host) = match &plan.provider {
        Some(provider) => (provider.clone(), plan.full_name.clone(), plan.host.clone()),
        None => {
            let found = find_repository(&api, &RepoArg::parse(&plan.url)?, err)?;
            let host = found
                .host
                .clone()
                .filter(|_| provider_needs_host(&found.provider));
            (found.provider, found.full_name, host)
        }
    };
    api.details(&provider, &full_name, host.as_deref())?
        .value
        .default_branch
        .ok_or_else(|| usage("The repository has no default branch."))
}

/// Tells the user to connect the VCS account when repo-connections has no token for it. For other errors, `other`
/// gives the message.
fn default_branch_error(
    services: &Services,
    plan: &RepoPlan,
    error: CliError,
    other: impl FnOnce(CliError) -> String,
) -> CliError {
    if is_missing_vcs_token(&error) {
        let provider = plan.provider.as_deref().unwrap_or("VCS");
        let place = services
            .integrations_url()
            .map_or_else(|| "in Air".to_string(), |url| format!("at {url}"));
        usage(format!(
            "Your {provider} account is not connected to Air. Connect it {place}, then run the command again."
        ))
    } else {
        usage(other(error))
    }
}

/// Gets the start points of the extra repositories of the environment, in the order of the environment.
/// The server keeps no branch for them, so each one starts from its default branch.
fn additional_repos(
    services: &Services,
    env: &EnvConfig,
    err: &mut impl Write,
) -> Result<Vec<AdditionalRepo>, CliError> {
    let mut repos = Vec::new();
    for repository in env.additional_repositories.iter().flatten() {
        let branch = match &repository.git_ref {
            Some(branch) => branch.clone(),
            None => {
                let plan = plan_additional_repository(&repository.url)?;
                default_branch(services, &plan, err).map_err(|e| {
                    default_branch_error(services, &plan, e, |e| {
                        format!(
                            "Cannot get the default branch of the additional repository {}: {e}",
                            repository.url
                        )
                    })
                })?
            }
        };
        repos.push(AdditionalRepo {
            repository_url: repository.url.clone(),
            start_point: StartPoint { branch },
        });
    }
    Ok(repos)
}

/// Makes the `POST /tasks` body.
pub fn build_task_request(
    prompt: &str,
    plan: &RepoPlan,
    agent_id: &str,
    options: SessionConfigOptions,
    env_config_id: Option<&str>,
    additional_repos: Vec<AdditionalRepo>,
) -> CreateTaskRequest {
    let options = (options != SessionConfigOptions::default()).then_some(options);
    CreateTaskRequest {
        repository_url: plan.url.clone(),
        git_ref: plan.git_ref.clone(),
        commit_hash: plan.commit_hash.clone(),
        client_info: ClientInfo {
            client_name: CLIENT_NAME.to_string(),
            client_version: env!("CARGO_PKG_VERSION").to_string(),
        },
        sessions: vec![TaskSessionSpec {
            agent_id: agent_id.to_string(),
            launch_config: LaunchConfig {
                prompt: prompt.to_string(),
                session_config_options: options,
            },
            env_config_id: env_config_id.map(str::to_string),
        }],
        additional_repos,
    }
}

pub struct StartArgs<'a> {
    pub prompt: &'a str,
    pub repo: Option<&'a str>,
    pub branch: Option<&'a str>,
    pub env: Option<&'a str>,
    pub agent: Option<&'a str>,
    pub model: Option<&'a str>,
    pub mode: Option<&'a str>,
    pub reasoning: Option<&'a str>,
    pub detach: bool,
    pub json: bool,
    pub interval: Duration,
}

pub fn start(
    services: &Services,
    git: &dyn GitTool,
    cwd: &Path,
    args: &StartArgs,
    out: &mut impl Write,
    err: &mut impl Write,
) -> Result<(), CliError> {
    let env = match args.env {
        Some(query) => {
            let api = services.env_configs()?;
            Some(if is_uuid(query) {
                api.get(query)?.value.config
            } else {
                let list = api.list(false, None)?.value;
                select_env(&list, query)?.clone()
            })
        }
        None => None,
    };

    let repo = match args.repo.map(RepoArg::parse).transpose()? {
        Some(RepoArg::Url {
            url,
            host,
            full_name,
        }) if provider_for_host(&host).is_some() => Some(ResolvedRepo {
            url,
            provider: provider_for_host(&host).map(str::to_string),
            host: None,
            full_name,
        }),
        Some(arg) => {
            let found = find_repository(&services.repos()?, &arg, err)?;
            Some(ResolvedRepo {
                url: match &arg {
                    RepoArg::Url { url, .. } => url.clone(),
                    RepoArg::FullName(_) => found.clone_url.clone(),
                },
                full_name: found.full_name.clone(),
                host: found
                    .host
                    .clone()
                    .filter(|_| provider_needs_host(&found.provider)),
                provider: Some(found.provider),
            })
        }
        None => None,
    };

    let local = if repo.is_none() && env.is_none() {
        Some(git.read_remote_info(cwd).map_err(|e| {
            usage(format!(
                "Cannot read the `origin` remote of the current directory: {e}. Use --repo or --env."
            ))
        })?)
    } else {
        None
    };

    let mut plan = plan_repository(repo.as_ref(), env.as_ref(), local.as_ref(), args.branch)?;

    let spawner = services.spawner()?;
    let agent_list = spawner.agents()?.value.agents;
    let choice = agents::choose(
        &agent_list,
        args.agent,
        args.model,
        args.mode,
        args.reasoning,
    )?;

    if plan.needs_default_branch() {
        let branch = default_branch(services, &plan, err).map_err(|e| {
            default_branch_error(services, &plan, e, |e| {
                format!(
                    "Cannot get the default branch of {}: {e} Use --branch.",
                    plan.full_name
                )
            })
        })?;
        plan.git_ref = Some(branch);
    }

    let additional = match &env {
        Some(env) => additional_repos(services, env, err)?,
        None => Vec::new(),
    };

    let request = build_task_request(
        args.prompt,
        &plan,
        &choice.agent_id,
        choice.options,
        env.as_ref().map(|e| e.id.as_str()),
        additional,
    );
    let task = spawner.create_task(&request)?.value;
    let session_id = task
        .sessions
        .first()
        .map(|s| s.session_id.clone())
        .unwrap_or_else(|| task.id.clone());
    let url = services.session_web_url(&session_id, Some(&task.id));

    writeln!(err, "Session: {session_id}")?;
    match &url {
        Some(url) => writeln!(err, "URL: {url}")?,
        None => writeln!(err, "URL: unknown (set JCP_ORG_ID to show it)")?,
    }
    for repo in &request.additional_repos {
        writeln!(
            err,
            "Also clones: {} ({})",
            repo.repository_url, repo.start_point.branch
        )?;
    }

    if args.detach {
        if args.json {
            let body = json!({"sessionId": session_id, "taskId": task.id, "url": url});
            writeln!(out, "{body}")?;
        } else {
            writeln!(out, "{session_id}")?;
        }
        return Ok(());
    }

    let status = diag::watch(
        &spawner,
        services.clock.as_ref(),
        &session_id,
        &WatchOptions {
            interval: args.interval,
            show_status: true,
            initial_tail: None,
            json: false,
        },
        out,
    )?;
    if FAILED_STATUSES.contains(&status.as_str()) {
        return Err(CliError::SessionFailed(status));
    }
    if status == USER_INPUT_REQUIRED {
        writeln!(
            err,
            "The agent waits for your answer. Answer in the web UI: {}",
            url.as_deref().unwrap_or("unknown URL")
        )?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Other session commands
// ---------------------------------------------------------------------------------------------

pub struct ListArgs<'a> {
    pub search: Option<&'a str>,
    pub case_sensitive: bool,
    pub limit: u64,
    pub offset: u64,
    pub json: bool,
}

pub fn list(
    api: &SpawnerApi,
    args: &ListArgs,
    out: &mut impl Write,
    err: &mut impl Write,
) -> Result<(), CliError> {
    let response = match args.search {
        Some(q) => api.search_sessions(q, args.case_sensitive, args.offset, args.limit)?,
        None => api.list_sessions(args.offset, args.limit)?,
    };
    if args.json {
        writeln!(out, "{}", response.raw)?;
        return Ok(());
    }
    let sessions = &response.value.sessions;
    if sessions.is_empty() {
        writeln!(out, "No sessions.")?;
    } else {
        write!(out, "{}", sessions_table(sessions))?;
    }
    if let Some(p) = &response.value.pagination {
        let shown_to = args.offset + sessions.len() as u64;
        if !sessions.is_empty() && shown_to < p.total_items {
            writeln!(
                err,
                "Shown {}-{shown_to} of {}. Use --offset {shown_to} to see more.",
                args.offset + 1,
                p.total_items
            )?;
        }
    }
    Ok(())
}

pub fn sessions_table(sessions: &[Session]) -> String {
    let rows: Vec<Vec<String>> = sessions
        .iter()
        .map(|s| {
            vec![
                s.session_id.clone(),
                s.status.clone(),
                or_dash(s.updated_at.as_deref()),
                s.repository_state.full_name(),
                s.session_name.clone(),
            ]
        })
        .collect();
    table(&["ID", "STATUS", "UPDATED", "REPOSITORY", "NAME"], &rows)
}

/// Makes the `session get` text.
pub fn describe_session(s: &Session, url: Option<&str>) -> String {
    let r = &s.repository_state;
    let mut out = String::new();
    let mut line = |name: &str, value: String| out.push_str(&format!("{name:<16}{value}\n"));
    line("ID", s.session_id.clone());
    line("Name", s.session_name.clone());
    line("Status", s.status.clone());
    if let Some(comment) = s.status_comment.as_deref().filter(|c| !c.is_empty()) {
        line("Status comment", comment.to_string());
    }
    line("Updated", or_dash(s.updated_at.as_deref()));
    line(
        "Repository",
        format!(
            "{}/{}/{}",
            r.service_host, r.organization, r.repository_name
        ),
    );
    line("Ref", or_dash(r.git_ref.as_deref()));
    line("Commit", or_dash(r.commit_hash.as_deref()));
    line("Agent", or_dash(s.agent_type.as_deref()));
    line("Launched from", or_dash(s.launched_from.as_deref()));
    line("Results branch", or_dash(s.results_branch_name.as_deref()));
    line("Results commit", or_dash(s.results_commit_hash.as_deref()));
    line("Task", or_dash(s.task_id.as_deref()));
    line("URL", or_dash(url));
    out
}

pub fn get(
    services: &Services,
    api: &SpawnerApi,
    id: &str,
    json: bool,
    out: &mut impl Write,
) -> Result<(), CliError> {
    let response = api.get_session(id)?;
    if json {
        writeln!(out, "{}", response.raw)?;
        return Ok(());
    }
    let s = &response.value;
    let url = services.session_web_url(&s.session_id, s.task_id.as_deref());
    write!(out, "{}", describe_session(s, url.as_deref()))?;
    Ok(())
}

pub fn status(api: &SpawnerApi, id: &str, out: &mut impl Write) -> Result<(), CliError> {
    writeln!(out, "{}", api.get_status(id)?)?;
    Ok(())
}

/// Polls the status until it is a final status. Exits with an error on a failed status or on timeout.
pub fn wait(
    api: &SpawnerApi,
    clock: &dyn Clock,
    id: &str,
    timeout: Option<Duration>,
    interval: Duration,
    out: &mut impl Write,
) -> Result<(), CliError> {
    let started = clock.now();
    loop {
        let status = api.get_status(id)?;
        if FINAL_STATUSES.contains(&status.as_str()) {
            writeln!(out, "{status}")?;
            if FAILED_STATUSES.contains(&status.as_str()) {
                return Err(CliError::SessionFailed(status));
            }
            return Ok(());
        }
        let elapsed = clock.now().duration_since(started).unwrap_or_default();
        if let Some(timeout) = timeout
            && elapsed >= timeout
        {
            return Err(usage(format!(
                "Timeout after {} s. The status is {status}.",
                timeout.as_secs_f64()
            )));
        }
        clock.sleep(interval);
    }
}

pub fn rename(
    api: &SpawnerApi,
    id: &str,
    name: &str,
    err: &mut impl Write,
) -> Result<(), CliError> {
    api.rename(id, name)?;
    writeln!(err, "Renamed the session {id}.")?;
    Ok(())
}

pub fn resume(
    api: &SpawnerApi,
    id: &str,
    prompt: Option<&str>,
    err: &mut impl Write,
) -> Result<(), CliError> {
    api.resume(id, prompt)?;
    writeln!(err, "Resumed the session {id}.")?;
    Ok(())
}

pub fn archive(api: &SpawnerApi, id: &str, err: &mut impl Write) -> Result<(), CliError> {
    api.archive(id)?;
    writeln!(err, "Archived the session {id}.")?;
    Ok(())
}

/// Deletes a session. Without `yes`, it asks on a terminal, and stops with exit code 2 when there is no terminal.
pub fn delete(
    api: &SpawnerApi,
    id: &str,
    yes: bool,
    is_terminal: bool,
    input: &mut impl BufRead,
    err: &mut impl Write,
) -> Result<(), CliError> {
    if !yes {
        if !is_terminal {
            return Err(CliError::NeedsConfirmation(format!(
                "Deleting the session {id} cannot be undone. Add --yes to delete it without a question."
            )));
        }
        write!(
            err,
            "Delete the session {id}, its environment and its artifacts? [y/N] "
        )?;
        err.flush()?;
        let mut answer = String::new();
        input.read_line(&mut answer)?;
        if !matches!(answer.trim().to_lowercase().as_str(), "y" | "yes") {
            return Err(usage("Not deleted."));
        }
    }
    api.delete(id)?;
    writeln!(err, "Deleted the session {id}.")?;
    Ok(())
}

pub fn artifacts(
    api: &SpawnerApi,
    id: &str,
    json: bool,
    out: &mut impl Write,
) -> Result<(), CliError> {
    let response = api.artifacts(id)?;
    if json {
        writeln!(out, "{}", response.raw)?;
        return Ok(());
    }
    if response.value.is_empty() {
        writeln!(out, "No artifacts.")?;
        return Ok(());
    }
    write!(out, "{}", artifacts_table(&response.value))?;
    Ok(())
}

pub fn artifacts_table(artifacts: &[crate::api::spawner::ArtifactInfo]) -> String {
    let rows: Vec<Vec<String>> = artifacts
        .iter()
        .map(|a| vec![a.name.clone(), human_size(a.size_bytes)])
        .collect();
    table(&["NAME", "SIZE"], &rows)
}

/// Downloads an artifact. `output` `-` writes to stdout. The default file is the last part of the name.
pub fn download(
    api: &SpawnerApi,
    id: &str,
    name: &str,
    output: Option<&Path>,
    out: &mut impl Write,
    err: &mut impl Write,
) -> Result<(), CliError> {
    let presigned = api.artifact_download(id, name)?;
    let bytes = api.download(&presigned.url)?;
    let default_name = name.rsplit('/').find(|p| !p.is_empty()).unwrap_or(name);
    let path = output
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(default_name));
    write_output(&path, &bytes, out, err)
}

/// Writes bytes to a file, or to stdout when the path is `-`.
pub fn write_output(
    path: &Path,
    bytes: &[u8],
    out: &mut impl Write,
    err: &mut impl Write,
) -> Result<(), CliError> {
    if path == Path::new("-") {
        out.write_all(bytes)?;
        out.flush()?;
    } else {
        fs::write(path, bytes)?;
        writeln!(
            err,
            "Saved {} ({}).",
            path.display(),
            human_size(bytes.len() as u64)
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "e4c83d51-c095-436f-99ed-4d1f30c8cc0f";

    #[test]
    fn session_ref_from_uuid() {
        assert_eq!(parse_session_ref(ID).unwrap(), ID);
        assert_eq!(parse_session_ref(&format!(" {ID} ")).unwrap(), ID);
    }

    #[test]
    fn session_ref_from_task_urls() {
        let base = "https://air.jetbrains.cloud/org/05cf1a7f-6ab5-713b-abd3-29d0c8a05e2d";
        for url in [
            format!("{base}/task/{ID}"),
            format!("{base}/task/{ID}/editor"),
            format!("{base}/task/{ID}/artifacts?x=1"),
            format!("{base}/session/{ID}/editor"),
        ] {
            assert_eq!(parse_session_ref(&url).unwrap(), ID, "{url}");
        }
    }

    #[test]
    fn session_ref_rejects_bad_input() {
        for bad in [
            "abc",
            "e4c83d51-c095-436f-99ed-4d1f30c8cc0",
            "https://air.jetbrains.cloud/org/05cf1a7f-6ab5-713b-abd3-29d0c8a05e2d",
            "https://air.jetbrains.cloud/task/not-a-uuid",
            "../../etc",
        ] {
            assert!(parse_session_ref(bad).is_err(), "{bad}");
        }
    }

    fn env(id: &str) -> EnvConfig {
        EnvConfig {
            id: id.into(),
            name: "My env".into(),
            service_host: "github.com".into(),
            organization: "JetBrains".into(),
            repository_name: "marinator".into(),
            ..Default::default()
        }
    }

    fn github_repo(full_name: &str) -> ResolvedRepo {
        ResolvedRepo {
            url: format!("https://github.com/{full_name}"),
            full_name: full_name.into(),
            provider: Some("github".into()),
            host: None,
        }
    }

    fn local(branch: &str) -> GitRemoteInfo {
        GitRemoteInfo {
            url: "git@github.com:JetBrains/local.git".into(),
            branch: branch.into(),
            revision: "abc123".into(),
        }
    }

    fn body(plan: &RepoPlan, env_id: Option<&str>) -> serde_json::Value {
        serde_json::to_value(build_task_request(
            "Do it",
            plan,
            "claude",
            SessionConfigOptions::default(),
            env_id,
            Vec::new(),
        ))
        .unwrap()
    }

    fn client_info() -> serde_json::Value {
        json!({"clientName": "jcp", "clientVersion": env!("CARGO_PKG_VERSION")})
    }

    #[test]
    fn task_body_for_local_repository() {
        let plan = plan_repository(None, None, Some(&local("feature")), None).unwrap();
        assert_eq!(
            body(&plan, None),
            json!({
                "repositoryUrl": "https://github.com/JetBrains/local",
                "ref": "feature",
                "clientInfo": client_info(),
                "sessions": [{"agentId": "claude", "launchConfig": {"prompt": "Do it"}}]
            })
        );
    }

    #[test]
    fn task_body_for_detached_head_sends_commit() {
        let plan = plan_repository(None, None, Some(&local("HEAD")), None).unwrap();
        assert_eq!(plan.git_ref, None);
        let body = body(&plan, None);
        assert_eq!(body["commitHash"], "abc123");
        assert!(body.get("ref").is_none());
    }

    #[test]
    fn branch_argument_wins_over_local_branch() {
        let plan = plan_repository(None, None, Some(&local("HEAD")), Some("main")).unwrap();
        assert_eq!(plan.git_ref.as_deref(), Some("main"));
        assert_eq!(plan.commit_hash, None);
    }

    #[test]
    fn task_body_for_repo_argument() {
        let repo = github_repo("JetBrains/marinator");
        let plan = plan_repository(Some(&repo), None, Some(&local("x")), None).unwrap();
        assert!(plan.needs_default_branch());
        assert_eq!(plan.provider.as_deref(), Some("github"));
        let body = body(&plan, None);
        assert_eq!(
            body["repositoryUrl"],
            "https://github.com/JetBrains/marinator"
        );
        assert!(body.get("ref").is_none());
    }

    #[test]
    fn task_body_for_env_has_env_config_id_and_no_fallback() {
        let e = env("env-1");
        let plan = plan_repository(None, Some(&e), Some(&local("x")), None).unwrap();
        assert_eq!(plan.url, "https://github.com/JetBrains/marinator");
        let body = body(&plan, Some("env-1"));
        assert_eq!(body["sessions"][0]["envConfigId"], "env-1");
        assert!(
            body["sessions"][0]
                .get("environmentConfigFallbackMode")
                .is_none()
        );
    }

    #[test]
    fn env_and_same_repo_are_accepted() {
        let e = env("env-1");
        let repo = ResolvedRepo {
            url: "https://github.com/jetbrains/marinator.git".into(),
            ..github_repo("JetBrains/marinator")
        };
        assert!(plan_repository(Some(&repo), Some(&e), None, None).is_ok());
    }

    #[test]
    fn env_and_other_repo_give_error() {
        let e = env("env-1");
        let repo = github_repo("JetBrains/other");
        let error = plan_repository(Some(&repo), Some(&e), None, None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("The environment `My env`"), "{error}");
    }

    #[test]
    fn no_repository_gives_error() {
        assert!(plan_repository(None, None, None, None).is_err());
    }

    #[test]
    fn task_body_has_session_options() {
        let plan = plan_repository(Some(&github_repo("a/b")), None, None, Some("dev")).unwrap();
        let request = build_task_request(
            "p",
            &plan,
            "claude",
            SessionConfigOptions {
                model: Some("m".into()),
                mode: None,
                reasoning_effort: Some("high".into()),
            },
            None,
            Vec::new(),
        );
        assert_eq!(
            serde_json::to_value(request).unwrap()["sessions"][0]["launchConfig"],
            json!({"prompt": "p", "sessionConfigOptions": {"model": "m", "reasoning_effort": "high"}})
        );
    }

    #[test]
    fn sessions_table_columns() {
        let s: Session = serde_json::from_value(json!({
            "sessionId": ID, "sessionName": "Fix bug", "status": "RUNNING", "updatedAt": "2026-01-01T00:00:00Z",
            "repositoryState": {"serviceHost": "github.com", "organization": "a", "repositoryName": "b"}
        }))
        .unwrap();
        let out = sessions_table(&[s]);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(
            lines[0].split_whitespace().collect::<Vec<_>>(),
            ["ID", "STATUS", "UPDATED", "REPOSITORY", "NAME"]
        );
        assert_eq!(
            lines[1].split_whitespace().collect::<Vec<_>>(),
            [ID, "RUNNING", "2026-01-01T00:00:00Z", "a/b", "Fix", "bug"]
        );
    }

    #[test]
    fn artifacts_table_has_sizes() {
        let out = artifacts_table(&[crate::api::spawner::ArtifactInfo {
            name: "report.md".into(),
            size_bytes: 2048,
        }]);
        assert_eq!(out, "NAME       SIZE\nreport.md  2.0 KiB\n");
    }
}
