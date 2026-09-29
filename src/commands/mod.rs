//! Handlers of the `session`, `env`, `repo` and `agents` commands.
//!
//! Each handler takes the API clients (through [`Services`]) and a writer for stdout, so that tests can
//! run the handlers without a terminal.

use crate::{
    EnvConfig,
    api::{ApiError, HttpClient, env_configs::EnvConfigsApi, repos::ReposApi, spawner::SpawnerApi},
    auth::{
        self, AIR_BACKEND_AUDIENCE, AuthError, AuthSession, JCP_AS_AUDIENCE,
        REPO_CONNECTIONS_AUDIENCE,
    },
    keychain::SecretBackend,
};
use std::{
    cell::RefCell,
    io, thread,
    time::{Duration, SystemTime},
};
use thiserror::Error;

pub mod agents;
pub mod cli;
pub mod diag;
pub mod env;
pub mod format;
pub mod history;
pub mod repo;
pub mod session;

pub use cli::{
    EnvCommand, RepoCommand, SessionCommand, run_agents, run_env, run_repo, run_session,
};

/// The name of the environment variable with the organization ID. Use it together with `JCP_ACCESS_TOKEN`.
pub const JCP_ORG_ID_ENV_NAME: &str = "JCP_ORG_ID";

#[derive(Error, Debug)]
pub enum CliError {
    #[error("{0}")]
    Api(#[from] ApiError),

    #[error("You are not logged in. Run `jcp login`.")]
    NotLoggedIn,

    #[error("Your login cannot get a token for {service}. Details: {source}")]
    AudienceSwitch {
        service: String,
        #[source]
        source: AuthError,
    },

    #[error("Cannot refresh your login. Log in again with `jcp login`. Details: {0}")]
    Auth(AuthError),

    #[error("{0}")]
    Usage(String),

    /// The command must ask the user, but stdin is not a terminal
    #[error("{0}")]
    NeedsConfirmation(String),

    #[error("The session stopped with status {0}.")]
    SessionFailed(String),

    #[error("IO error: {0}")]
    Io(#[from] io::Error),
}

impl CliError {
    pub fn exit_code(&self) -> i32 {
        match self {
            CliError::NeedsConfirmation(_) => 2,
            _ => 1,
        }
    }
}

/// Gives access tokens for service audiences.
pub trait Credentials {
    fn token_for(&mut self, audience: &str) -> Result<String, CliError>;

    /// Organization ID. It is necessary only for Air web URLs.
    fn org_id(&mut self) -> Result<Option<String>, CliError>;
}

/// One token for all audiences, from the `JCP_ACCESS_TOKEN` environment variable.
pub struct StaticCredentials {
    pub token: String,
    pub org_id: Option<String>,
}

impl Credentials for StaticCredentials {
    fn token_for(&mut self, _audience: &str) -> Result<String, CliError> {
        Ok(self.token.clone())
    }

    fn org_id(&mut self) -> Result<Option<String>, CliError> {
        Ok(self.org_id.clone())
    }
}

/// Tokens from the refresh token in the keychain. The login refreshes one time, on the first request.
pub struct LoginCredentials {
    keychain: Box<dyn SecretBackend>,
    env_config: EnvConfig,
    session: Option<AuthSession>,
}

impl LoginCredentials {
    pub fn new(keychain: Box<dyn SecretBackend>, env_config: EnvConfig) -> Self {
        Self {
            keychain,
            env_config,
            session: None,
        }
    }

    fn session(&mut self) -> Result<&mut AuthSession, CliError> {
        if self.session.is_none() {
            let refresh_token = self
                .keychain
                .get_refresh_token()?
                .ok_or(CliError::NotLoggedIn)?;
            let session =
                auth::open_session(&refresh_token, &self.env_config).map_err(CliError::Auth)?;
            self.session = Some(session);
        }
        Ok(self.session.as_mut().expect("session is set"))
    }
}

impl Credentials for LoginCredentials {
    fn token_for(&mut self, audience: &str) -> Result<String, CliError> {
        self.session()?
            .token_for(audience)
            .map_err(|source| match source {
                AuthError::AudienceSwitch(_) => CliError::AudienceSwitch {
                    service: audience.to_string(),
                    source,
                },
                e => CliError::Auth(e),
            })
    }

    fn org_id(&mut self) -> Result<Option<String>, CliError> {
        Ok(Some(self.session()?.org_id().to_string()))
    }
}

/// Time source and sleep. Tests replace it to run polling commands fast.
pub trait Clock {
    fn now(&self) -> SystemTime;
    fn sleep(&self, duration: Duration);
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }

    fn sleep(&self, duration: Duration) {
        thread::sleep(duration)
    }
}

/// The services and the settings that the command handlers use.
pub struct Services {
    pub env_config: EnvConfig,
    /// The `orca-cli --stack` value for hints
    pub orca_stack: String,
    pub clock: Box<dyn Clock>,
    credentials: RefCell<Box<dyn Credentials>>,
}

impl Services {
    pub fn new(
        env_config: EnvConfig,
        orca_stack: impl Into<String>,
        credentials: Box<dyn Credentials>,
        clock: Box<dyn Clock>,
    ) -> Self {
        Self {
            env_config,
            orca_stack: orca_stack.into(),
            clock,
            credentials: RefCell::new(credentials),
        }
    }

    fn client(&self, base_url: String, audience: &str) -> Result<HttpClient, CliError> {
        let token = self.credentials.borrow_mut().token_for(audience)?;
        Ok(HttpClient::new(base_url, token))
    }

    pub fn spawner(&self) -> Result<SpawnerApi, CliError> {
        let client = self.client(self.env_config.agent_spawner_url(), JCP_AS_AUDIENCE)?;
        Ok(SpawnerApi::new(client))
    }

    pub fn env_configs(&self) -> Result<EnvConfigsApi, CliError> {
        let client = self.client(self.env_config.air_backend_url(), AIR_BACKEND_AUDIENCE)?;
        Ok(EnvConfigsApi::new(client))
    }

    pub fn repos(&self) -> Result<ReposApi, CliError> {
        let client = self.client(
            self.env_config.repo_connections_url(),
            REPO_CONNECTIONS_AUDIENCE,
        )?;
        Ok(ReposApi::new(client))
    }

    /// The Air page where the user connects VCS accounts. It is `None` when the organization ID is not known.
    pub fn integrations_url(&self) -> Option<String> {
        let org_id = self.credentials.borrow_mut().org_id().ok().flatten()?;
        Some(self.env_config.integrations_url(&org_id))
    }

    /// The Air web URL of a session. It is `None` when the organization ID is not known.
    pub fn session_web_url(&self, session_id: &str, task_id: Option<&str>) -> Option<String> {
        let org_id = self.credentials.borrow_mut().org_id().ok().flatten()?;
        Some(match task_id {
            Some(task_id) => self.env_config.task_url(&org_id, task_id),
            None => self.env_config.session_url(&org_id, session_id),
        })
    }
}

/// Makes a [`CliError::Usage`] error.
pub(crate) fn usage(message: impl Into<String>) -> CliError {
    CliError::Usage(message.into())
}
