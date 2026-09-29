//! Command-line arguments of the `session`, `env`, `repo` and `agents` commands, and their dispatch.

use super::{
    CliError, Services, agents,
    diag::{self, HistoryArgs, LogsArgs, WatchOptions},
    env, repo,
    session::{self, StartArgs, parse_session_ref},
};
use crate::GitCommandTool;
use clap::{Args, Subcommand};
use std::{
    io::{self, IsTerminal},
    path::PathBuf,
    time::Duration,
};

/// Parses a number of seconds that is more than 0. Fractions are permitted.
fn parse_seconds(s: &str) -> Result<Duration, String> {
    let secs: f64 = s
        .parse()
        .map_err(|_| format!("`{s}` is not a number of seconds"))?;
    match Duration::try_from_secs_f64(secs) {
        Ok(d) if !d.is_zero() => Ok(d),
        _ => Err(format!(
            "`{s}` is not a valid number of seconds. Use a value more than 0"
        )),
    }
}

#[derive(Args, Debug)]
pub struct SessionArg {
    /// Session ID or Air task URL
    #[arg(value_name = "SESSION")]
    pub session: String,
}

#[derive(Subcommand, Debug)]
pub enum SessionCommand {
    /// Start a task with one session, and show its progress until the agent stops
    Start {
        /// The prompt for the agent
        prompt: String,
        /// Repository URL or owner/name. The default is the repository of --env, or the `origin` remote
        #[arg(long)]
        repo: Option<String>,
        /// Branch. The default is the current branch (local repository) or the default branch
        #[arg(long)]
        branch: Option<String>,
        /// Environment ID or name (see `jcp env list`)
        #[arg(long)]
        env: Option<String>,
        /// Agent ID (see `jcp agents`)
        #[arg(long)]
        agent: Option<String>,
        /// Model ID
        #[arg(long)]
        model: Option<String>,
        /// Permission mode ID
        #[arg(long)]
        mode: Option<String>,
        /// Reasoning level
        #[arg(long)]
        reasoning: Option<String>,
        /// Start the task and exit. Do not wait for the agent
        #[arg(long)]
        detach: bool,
        /// With --detach: print {"sessionId","taskId","url"}
        #[arg(long)]
        json: bool,
        #[arg(long, hide = true, value_parser = parse_seconds, default_value = "3")]
        interval: Duration,
    },
    /// List sessions, also the sessions that other users shared with you
    List {
        /// Find sessions by name
        #[arg(long)]
        search: Option<String>,
        /// With --search: case-sensitive search
        #[arg(long, requires = "search")]
        case_sensitive: bool,
        #[arg(long, default_value_t = 20)]
        limit: u64,
        #[arg(long, default_value_t = 0)]
        offset: u64,
        #[arg(long)]
        json: bool,
    },
    /// Show the details of a session
    Get {
        #[command(flatten)]
        session: SessionArg,
        #[arg(long)]
        json: bool,
    },
    /// Show the status of a session
    Status {
        #[command(flatten)]
        session: SessionArg,
    },
    /// Wait until the session has a final status or needs input. Exit code 1 for ERROR, ABORTED, CANCELLED and timeout
    Wait {
        #[command(flatten)]
        session: SessionArg,
        /// Maximum time to wait, in seconds
        #[arg(long, value_parser = parse_seconds)]
        timeout: Option<Duration>,
        /// Time between the status checks, in seconds
        #[arg(long, value_parser = parse_seconds, default_value = "3")]
        interval: Duration,
    },
    /// Rename a session
    Rename {
        #[command(flatten)]
        session: SessionArg,
        name: String,
    },
    /// Resume a SUSPENDED session
    Resume {
        #[command(flatten)]
        session: SessionArg,
        /// Send a new prompt when the session resumes
        #[arg(long)]
        prompt: Option<String>,
    },
    /// Stop a session, terminate its environment, and keep the record (ARCHIVED)
    Archive {
        #[command(flatten)]
        session: SessionArg,
    },
    /// Delete a session, its environment, and its artifacts
    Delete {
        #[command(flatten)]
        session: SessionArg,
        /// Do not ask for confirmation
        #[arg(long, short)]
        yes: bool,
    },
    /// List the artifacts of a session
    Artifacts {
        #[command(flatten)]
        session: SessionArg,
        #[arg(long)]
        json: bool,
    },
    /// Download an artifact
    Download {
        #[command(flatten)]
        session: SessionArg,
        /// Artifact name (see `jcp session artifacts`)
        name: String,
        /// Output file. `-` writes to stdout. The default is the file name of the artifact
        #[arg(long, short)]
        output: Option<PathBuf>,
    },
    /// Show one report: session, environment, last history items, and artifacts
    Diag {
        #[command(flatten)]
        session: SessionArg,
        #[arg(long)]
        json: bool,
    },
    /// Show status changes and new conversation items until the session stops
    Watch {
        #[command(flatten)]
        session: SessionArg,
        /// Time between the checks, in seconds
        #[arg(long, value_parser = parse_seconds, default_value = "3")]
        interval: Duration,
    },
    /// Show the conversation of a session
    History {
        #[command(flatten)]
        session: SessionArg,
        /// Show new items until the session stops
        #[arg(long)]
        follow: bool,
        /// Show only the last N items
        #[arg(long)]
        tail: Option<u64>,
        /// Print the raw ACP messages, one on each line
        #[arg(long)]
        json: bool,
        #[arg(long, hide = true, value_parser = parse_seconds, default_value = "3")]
        interval: Duration,
    },
    /// Show the debug data: Orca environment, ports, and log files
    Debug {
        #[command(flatten)]
        session: SessionArg,
        #[arg(long)]
        json: bool,
    },
    /// Download all logs as a ZIP file, list the log files, or show one log file
    Logs {
        #[command(flatten)]
        session: SessionArg,
        /// Output file. `-` writes to stdout. The default for the ZIP file is <SESSION>-logs.zip
        #[arg(long, short)]
        output: Option<PathBuf>,
        /// List the log files
        #[arg(long, conflicts_with_all = ["name", "output", "tail"])]
        list: bool,
        /// Show one log file. `.gz` files are decoded
        #[arg(long)]
        name: Option<String>,
        /// With --name: show only the last N lines
        #[arg(long, requires = "name")]
        tail: Option<usize>,
    },
}

#[derive(Subcommand, Debug)]
pub enum EnvCommand {
    /// List environments
    List {
        /// Show only the environments of this repository (URL or owner/name)
        #[arg(long)]
        repo: Option<String>,
        /// Show only your own environments
        #[arg(long, conflicts_with = "shared")]
        mine: bool,
        /// Show only the environments that other users shared with you
        #[arg(long)]
        shared: bool,
        /// Show only the environments of this organization project ID
        #[arg(long)]
        project: Option<String>,
        /// Also show drafts
        #[arg(long)]
        drafts: bool,
        #[arg(long)]
        json: bool,
    },
    /// Show an environment
    Get {
        /// Environment ID or name
        #[arg(value_name = "ENV")]
        env: String,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum RepoCommand {
    /// List the repositories that you can use
    List {
        /// Find repositories by name
        #[arg(long)]
        search: Option<String>,
        #[arg(long)]
        limit: Option<u64>,
        #[arg(long)]
        json: bool,
    },
    /// List the branches of a repository
    Branches {
        /// Repository URL or owner/name
        repo: String,
        /// Find branches by name
        #[arg(long)]
        search: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// List the VCS providers that you authorized
    Providers {
        #[arg(long)]
        json: bool,
    },
}

pub fn run_session(command: &SessionCommand, services: &Services) -> Result<(), CliError> {
    let mut out = io::stdout().lock();
    let mut err = io::stderr();
    let id = |arg: &SessionArg| parse_session_ref(&arg.session);
    let clock = services.clock.as_ref();
    match command {
        SessionCommand::Start {
            prompt,
            repo,
            branch,
            env,
            agent,
            model,
            mode,
            reasoning,
            detach,
            json,
            interval,
        } => {
            if *json && !*detach {
                return Err(super::usage("Use --json only with --detach."));
            }
            let cwd = std::env::current_dir()?;
            let args = StartArgs {
                prompt,
                repo: repo.as_deref(),
                branch: branch.as_deref(),
                env: env.as_deref(),
                agent: agent.as_deref(),
                model: model.as_deref(),
                mode: mode.as_deref(),
                reasoning: reasoning.as_deref(),
                detach: *detach,
                json: *json,
                interval: *interval,
            };
            session::start(services, &GitCommandTool, &cwd, &args, &mut out, &mut err)
        }
        SessionCommand::List {
            search,
            case_sensitive,
            limit,
            offset,
            json,
        } => {
            let args = session::ListArgs {
                search: search.as_deref(),
                case_sensitive: *case_sensitive,
                limit: *limit,
                offset: *offset,
                json: *json,
            };
            session::list(&services.spawner()?, &args, &mut out, &mut err)
        }
        SessionCommand::Get { session, json } => session::get(
            services,
            &services.spawner()?,
            &id(session)?,
            *json,
            &mut out,
        ),
        SessionCommand::Status { session } => {
            session::status(&services.spawner()?, &id(session)?, &mut out)
        }
        SessionCommand::Wait {
            session,
            timeout,
            interval,
        } => session::wait(
            &services.spawner()?,
            clock,
            &id(session)?,
            *timeout,
            *interval,
            &mut out,
        ),
        SessionCommand::Rename { session, name } => {
            session::rename(&services.spawner()?, &id(session)?, name, &mut err)
        }
        SessionCommand::Resume { session, prompt } => session::resume(
            &services.spawner()?,
            &id(session)?,
            prompt.as_deref(),
            &mut err,
        ),
        SessionCommand::Archive { session } => {
            session::archive(&services.spawner()?, &id(session)?, &mut err)
        }
        SessionCommand::Delete { session, yes } => {
            let id = id(session)?;
            let is_terminal = io::stdin().is_terminal();
            session::delete(
                &services.spawner()?,
                &id,
                *yes,
                is_terminal,
                &mut io::stdin().lock(),
                &mut err,
            )
        }
        SessionCommand::Artifacts { session, json } => {
            session::artifacts(&services.spawner()?, &id(session)?, *json, &mut out)
        }
        SessionCommand::Download {
            session,
            name,
            output,
        } => session::download(
            &services.spawner()?,
            &id(session)?,
            name,
            output.as_deref(),
            &mut out,
            &mut err,
        ),
        SessionCommand::Diag { session, json } => diag::diag(
            services,
            &services.spawner()?,
            &id(session)?,
            *json,
            &mut out,
        ),
        SessionCommand::Watch { session, interval } => {
            let options = WatchOptions {
                interval: *interval,
                show_status: true,
                initial_tail: Some(10),
                json: false,
            };
            diag::watch(
                &services.spawner()?,
                clock,
                &id(session)?,
                &options,
                &mut out,
            )?;
            Ok(())
        }
        SessionCommand::History {
            session,
            follow,
            tail,
            json,
            interval,
        } => {
            let args = HistoryArgs {
                follow: *follow,
                tail: *tail,
                json: *json,
                interval: *interval,
            };
            diag::history(&services.spawner()?, clock, &id(session)?, &args, &mut out)
        }
        SessionCommand::Debug { session, json } => diag::debug(
            services,
            &services.spawner()?,
            &id(session)?,
            *json,
            &mut out,
        ),
        SessionCommand::Logs {
            session,
            output,
            list,
            name,
            tail,
        } => {
            let args = LogsArgs {
                list: *list,
                name: name.as_deref(),
                tail: *tail,
                output: output.as_deref(),
            };
            diag::logs(
                &services.spawner()?,
                &id(session)?,
                &args,
                &mut out,
                &mut err,
            )
        }
    }
}

pub fn run_env(command: &EnvCommand, services: &Services) -> Result<(), CliError> {
    let mut out = io::stdout().lock();
    match command {
        EnvCommand::List {
            repo,
            mine,
            shared,
            project,
            drafts,
            json,
        } => {
            let args = env::ListArgs {
                repo: repo.as_deref(),
                mine: *mine,
                shared: *shared,
                project: project.as_deref(),
                drafts: *drafts,
                json: *json,
            };
            env::list(&services.env_configs()?, &args, &mut out)
        }
        EnvCommand::Get { env: query, json } => {
            env::get(&services.env_configs()?, query, *json, &mut out)
        }
    }
}

pub fn run_repo(command: &RepoCommand, services: &Services) -> Result<(), CliError> {
    let mut out = io::stdout().lock();
    let mut err = io::stderr();
    match command {
        RepoCommand::List {
            search,
            limit,
            json,
        } => repo::list(
            &services.repos()?,
            search.as_deref(),
            *limit,
            *json,
            &mut out,
            &mut err,
        ),
        RepoCommand::Branches { repo, search, json } => repo::branches(
            &services.repos()?,
            repo,
            search.as_deref(),
            *json,
            &mut out,
            &mut err,
        ),
        RepoCommand::Providers { json } => repo::providers(&services.repos()?, *json, &mut out),
    }
}

pub fn run_agents(json: bool, services: &Services) -> Result<(), CliError> {
    agents::list(&services.spawner()?, json, &mut io::stdout().lock())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[derive(clap::Parser)]
    struct TestCli {
        #[command(subcommand)]
        command: SessionCommand,
    }

    #[test]
    fn arguments_are_valid() {
        TestCli::command().debug_assert();
    }

    #[test]
    fn fractional_seconds() {
        assert_eq!(parse_seconds("0.05").unwrap(), Duration::from_millis(50));
        assert_eq!(parse_seconds("3").unwrap(), Duration::from_secs(3));
        assert!(parse_seconds("-1").is_err());
        assert!(parse_seconds("0").is_err());
        assert!(parse_seconds("x").is_err());
    }
}
