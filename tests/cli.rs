//! Tests of the `session`, `env`, `repo` and `agents` commands. They run the `jcp` binary against a fake HTTP server.

#[path = "harness/fake_http.rs"]
mod fake_http;

use fake_http::{FakeHttp, Reply};
use flate2::{Compression, write::GzEncoder};
use serde_json::{Value, json};
use std::{cell::Cell, io::Write, path::Path, process::Command};
use tempfile::TempDir;

const SID: &str = "e4c83d51-c095-436f-99ed-4d1f30c8cc0f";
const ORG: &str = "05cf1a7f-6ab5-713b-abd3-29d0c8a05e2d";
const AS: &str = "/agent-spawner";
const AB: &str = "/air-backend";
const RC: &str = "/repo-connections";

struct Run {
    code: i32,
    stdout: String,
    stdout_bytes: Vec<u8>,
    stderr: String,
}

struct Env {
    fake: FakeHttp,
    dir: TempDir,
    /// `false` when the test checks the tokens itself
    check_tokens: Cell<bool>,
}

impl Env {
    fn new() -> Self {
        Self {
            fake: FakeHttp::start(),
            dir: TempDir::new().unwrap(),
            check_tokens: Cell::new(true),
        }
    }

    fn route(&self, method: &str, target: &str, reply: Reply) -> &Self {
        self.fake.route(method, target, reply);
        self
    }

    fn route_seq(&self, method: &str, target: &str, replies: Vec<Reply>) -> &Self {
        self.fake.route_seq(method, target, replies);
        self
    }

    fn jcp(&self, args: &[&str]) -> Run {
        let output = Command::new(env!("CARGO_BIN_EXE_jcp"))
            .arg("--staging")
            .args(args)
            .current_dir(self.dir.path())
            .env("JCP_API_URL", self.fake.base_url())
            .env("AIR_WEB_URL", "http://air.test")
            .env("JCP_ACCESS_TOKEN", "test-token")
            .env("JCP_ORG_ID", ORG)
            .env("KEYCHAIN_FILE", self.dir.path().join("no-keychain"))
            .env_remove("JCP_ENVIRONMENT")
            .output()
            .expect("cannot run jcp");
        Run {
            code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stdout_bytes: output.stdout,
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }

    fn paths(&self) -> Vec<String> {
        self.fake
            .requests()
            .iter()
            .map(|r| format!("{} {}", r.method, r.path()))
            .collect()
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.fake.base_url())
    }
}

impl Drop for Env {
    /// Each API request has the Bearer token. Presigned downloads have no token.
    fn drop(&mut self) {
        if std::thread::panicking() || !self.check_tokens.get() {
            return;
        }
        for r in self.fake.requests() {
            if r.path().starts_with("/presigned/") {
                assert_eq!(r.authorization, None, "{} {}", r.method, r.url);
            } else {
                assert_eq!(
                    r.authorization.as_deref(),
                    Some("Bearer test-token"),
                    "{} {}",
                    r.method,
                    r.url
                );
            }
        }
    }
}

fn ok(run: &Run) {
    assert_eq!(
        run.code, 0,
        "stdout:\n{}\nstderr:\n{}",
        run.stdout, run.stderr
    );
}

fn session(status: &str) -> Value {
    json!({
        "sessionId": SID,
        "sessionName": "Fix the bug",
        "status": status,
        "statusComment": if status == "ERROR" { "boom" } else { "" },
        "updatedAt": "2026-09-01T10:00:00Z",
        "repositoryState": {
            "serviceHost": "github.com", "organization": "JetBrains", "repositoryName": "marinator",
            "ref": "main", "commitHash": "abc123"
        },
        "agentType": "claude",
        "launchedFrom": "jcp",
        "taskId": SID,
        "someNewField": 1
    })
}

fn agent_text(text: &str) -> String {
    json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": SID, "update": {
        "sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": text}}}})
    .to_string()
}

fn tool_call(title: &str) -> String {
    json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": SID, "update": {
        "sessionUpdate": "tool_call", "toolCallId": title, "title": title, "status": "completed"}}})
    .to_string()
}

/// A history page. `items` are in chronological order; the page has them from the newest to the oldest.
fn history(items: &[String], total: u64) -> Reply {
    let count = items.len() as u64;
    let messages: Vec<Value> = items
        .iter()
        .enumerate()
        .rev()
        .map(|(i, m)| json!({"index": total - count + i as u64, "message": m}))
        .collect();
    Reply::json(
        json!({"messages": messages, "pagination": {"offset": 0, "limit": 1000, "totalItems": total}}),
    )
}

fn agents() -> Reply {
    Reply::json(json!({"agents": [
        {"id": "claude", "name": "Claude", "isDefault": true, "enabled": true,
         "models": [{"id": "d", "name": "Default", "isDefault": true, "thinkingLevels": ["claude:low", "claude:high"]},
                    {"id": "m", "name": "Model M", "isDefault": false}],
         "permissionModes": [{"id": "agent-full-access", "name": "Full access", "isDefault": true}]},
        {"id": "codex", "name": "Codex", "models": []}
    ]}))
}

fn marinator() -> Value {
    json!({"fullName": "JetBrains/marinator", "cloneUrl": "https://github.com/JetBrains/marinator.git",
           "browseUrl": "https://github.com/JetBrains/marinator", "provider": "github",
           "permissions": {"read": true, "write": false}, "defaultBranch": "main"})
}

fn task() -> Reply {
    Reply::json(json!({"id": SID, "name": "Task", "sessions": [session("LAUNCHING")]}))
}

/// Routes for `session start --repo JetBrains/marinator`
fn start_routes(env: &Env) {
    env.route(
        "GET",
        &format!("{RC}/repositories/search"),
        Reply::json(json!({"data": [marinator()]})),
    )
    .route("GET", &format!("{AS}/agents"), agents())
    .route(
        "GET",
        &format!("{RC}/repositories/github"),
        Reply::json(marinator()),
    )
    .route("POST", &format!("{AS}/tasks"), task());
}

fn env_configs() -> Value {
    json!([
        {"id": "env-1", "name": "My env", "serviceHost": "github.com", "organization": "JetBrains",
         "repositoryName": "marinator", "shared": false, "accessLevel": "OWNER",
         "additionalRepositories": [{"url": "https://github.com/JetBrains/other"}]},
        {"id": "env-2", "name": "Team env", "serviceHost": "github.com", "organization": "JetBrains",
         "repositoryName": "air", "shared": true, "accessLevel": "USE", "isDraft": false}
    ])
}

// ---------------------------------------------------------------------------------------------
// session start
// ---------------------------------------------------------------------------------------------

#[test]
fn start_with_repo_detach_json() {
    let env = Env::new();
    start_routes(&env);
    let run = env.jcp(&[
        "session",
        "start",
        "--repo",
        "JetBrains/marinator",
        "--agent",
        "claude",
        "--model",
        "m",
        "--detach",
        "--json",
        "Do it",
    ]);
    ok(&run);
    assert_eq!(
        env.paths(),
        [
            format!("GET {RC}/repositories/search"),
            format!("GET {AS}/agents"),
            format!("GET {RC}/repositories/github"),
            format!("POST {AS}/tasks"),
        ]
    );
    let requests = env.fake.requests();
    assert_eq!(
        requests[0].url,
        format!("{RC}/repositories/search?q=JetBrains%2Fmarinator")
    );
    assert_eq!(
        requests[2].url,
        format!("{RC}/repositories/github?fullName=JetBrains%2Fmarinator")
    );
    assert_eq!(
        requests[3].body_json(),
        json!({
            "repositoryUrl": "https://github.com/JetBrains/marinator.git",
            "ref": "main",
            "clientInfo": {"clientName": "jcp", "clientVersion": env!("CARGO_PKG_VERSION")},
            "sessions": [{"agentId": "claude", "launchConfig": {"prompt": "Do it", "sessionConfigOptions": {"model": "m"}}}]
        })
    );
    let url = format!("http://air.test/org/{ORG}/task/{SID}/editor");
    let stdout: Value = serde_json::from_str(&run.stdout).unwrap();
    assert_eq!(stdout, json!({"sessionId": SID, "taskId": SID, "url": url}));
    assert!(
        run.stderr.contains(&format!("Session: {SID}")),
        "{}",
        run.stderr
    );
    assert!(
        run.stderr.contains(&format!("URL: {url}")),
        "{}",
        run.stderr
    );
}

#[test]
fn start_with_env_name_sends_env_config_id() {
    let env = Env::new();
    env.route(
        "GET",
        &format!("{AB}/env-configs"),
        Reply::json(env_configs()),
    )
    .route("GET", &format!("{AS}/agents"), agents())
    .route(
        "GET",
        &format!("{RC}/repositories/github"),
        Reply::json(marinator()),
    )
    .route("POST", &format!("{AS}/tasks"), task());
    let run = env.jcp(&["session", "start", "--env", "My env", "--detach", "Do it"]);
    ok(&run);
    assert_eq!(run.stdout, format!("{SID}\n"));
    assert!(
        env.fake
            .requests_to("GET", &format!("{RC}/repositories/search"))
            .is_empty()
    );
    let post = &env.fake.requests_to("POST", &format!("{AS}/tasks"))[0];
    let body = post.body_json();
    assert_eq!(
        body["repositoryUrl"],
        "https://github.com/JetBrains/marinator"
    );
    assert_eq!(body["ref"], "main");
    assert_eq!(body["sessions"][0]["envConfigId"], "env-1");
    assert_eq!(body["sessions"][0]["agentId"], "claude");
    assert!(
        body["sessions"][0]["launchConfig"]
            .get("sessionConfigOptions")
            .is_none()
    );
    assert!(body.get("environmentConfigFallbackMode").is_none());
}

/// An environment with extra repositories. `extra` gives the name and the default branch of each one.
fn multi_repo_env(env: &Env, extra: &[(&str, Value)]) {
    let urls: Vec<Value> = extra
        .iter()
        .map(|(name, _)| json!({"url": format!("https://github.com/JetBrains/{name}")}))
        .collect();
    env.route(
        "GET",
        &format!("{AB}/env-configs"),
        Reply::json(json!([
            {"id": "env-3", "name": "Multi env", "serviceHost": "github.com", "organization": "JetBrains",
             "repositoryName": "marinator", "shared": false, "additionalRepositories": urls}
        ])),
    )
    .route("GET", &format!("{AS}/agents"), agents())
    .route("POST", &format!("{AS}/tasks"), task());
    for (name, branch) in extra {
        env.route(
            "GET",
            &format!("{RC}/repositories/github?fullName=JetBrains%2F{name}"),
            Reply::json(
                json!({"fullName": format!("JetBrains/{name}"), "provider": "github",
                               "cloneUrl": format!("https://github.com/JetBrains/{name}.git"),
                               "defaultBranch": branch}),
            ),
        );
    }
}

#[test]
fn start_with_env_sends_additional_repos_in_order() {
    let env = Env::new();
    multi_repo_env(
        &env,
        &[("zeta", json!("develop")), ("alpha", json!("master"))],
    );
    let run = env.jcp(&[
        "session",
        "start",
        "--env",
        "Multi env",
        "--branch",
        "feature",
        "--detach",
        "Do it",
    ]);
    ok(&run);
    let body = env.fake.requests_to("POST", &format!("{AS}/tasks"))[0].body_json();
    assert_eq!(body["ref"], "feature");
    assert_eq!(
        body["additionalRepos"],
        json!([
            {"repositoryUrl": "https://github.com/JetBrains/zeta", "startPoint": {"branch": "develop"}},
            {"repositoryUrl": "https://github.com/JetBrains/alpha", "startPoint": {"branch": "master"}}
        ])
    );
    assert!(
        run.stderr
            .contains("Also clones: https://github.com/JetBrains/zeta (develop)\n"),
        "{}",
        run.stderr
    );
}

#[test]
fn start_with_additional_repo_without_default_branch_sends_no_task() {
    let env = Env::new();
    multi_repo_env(&env, &[("zeta", json!("develop")), ("other", Value::Null)]);
    let run = env.jcp(&[
        "session",
        "start",
        "--env",
        "Multi env",
        "--branch",
        "feature",
        "--detach",
        "Do it",
    ]);
    assert_eq!(run.code, 1);
    assert!(
        run.stderr.contains(
            "Cannot get the default branch of the additional repository https://github.com/JetBrains/other: The repository has no default branch."
        ),
        "{}",
        run.stderr
    );
    assert!(
        env.fake
            .requests_to("POST", &format!("{AS}/tasks"))
            .is_empty()
    );
}

/// An environment for a Space repository: the host does not give the provider
fn space_env_configs() -> Value {
    json!([{"id": "env-s", "name": "Space env", "serviceHost": "git.jetbrains.team",
            "organization": "ij", "repositoryName": "ultimate", "shared": true, "accessLevel": "USE"}])
}

fn space_ultimate(default_branch: Value) -> Value {
    json!({"fullName": "IJ/ultimate", "cloneUrl": "https://git.jetbrains.team/ij/ultimate.git",
           "provider": "space", "host": null, "permissions": null, "defaultBranch": default_branch})
}

#[test]
fn start_with_space_env_gets_default_branch_with_search() {
    let env = Env::new();
    env.route(
        "GET",
        &format!("{AB}/env-configs"),
        Reply::json(space_env_configs()),
    )
    .route("GET", &format!("{AS}/agents"), agents())
    .route(
        "GET",
        &format!("{RC}/repositories/search"),
        Reply::json(json!({"data": [space_ultimate(json!("master"))], "providerErrors": null})),
    )
    .route(
        "GET",
        &format!("{RC}/repositories/space"),
        Reply::json(space_ultimate(json!("master"))),
    )
    .route("POST", &format!("{AS}/tasks"), task());
    let run = env.jcp(&[
        "session",
        "start",
        "--env",
        "Space env",
        "--detach",
        "Do it",
    ]);
    ok(&run);
    let details = &env
        .fake
        .requests_to("GET", &format!("{RC}/repositories/space"))[0];
    assert!(
        details.url.contains("fullName=IJ%2Fultimate"),
        "{}",
        details.url
    );
    let body = env.fake.requests_to("POST", &format!("{AS}/tasks"))[0].body_json();
    assert_eq!(
        body["repositoryUrl"],
        "https://git.jetbrains.team/ij/ultimate"
    );
    assert_eq!(body["ref"], "master");
    assert_eq!(body["sessions"][0]["envConfigId"], "env-s");
}

#[test]
fn start_without_default_branch_sends_no_task() {
    let env = Env::new();
    env.route(
        "GET",
        &format!("{AB}/env-configs"),
        Reply::json(space_env_configs()),
    )
    .route("GET", &format!("{AS}/agents"), agents())
    .route(
        "GET",
        &format!("{RC}/repositories/search"),
        Reply::json(json!({"data": [space_ultimate(Value::Null)]})),
    )
    .route(
        "GET",
        &format!("{RC}/repositories/space"),
        Reply::json(space_ultimate(Value::Null)),
    );
    let run = env.jcp(&[
        "session",
        "start",
        "--env",
        "Space env",
        "--detach",
        "Do it",
    ]);
    assert_eq!(run.code, 1);
    assert!(run.stderr.contains("Use --branch"), "{}", run.stderr);
    assert!(
        env.fake
            .requests_to("POST", &format!("{AS}/tasks"))
            .is_empty()
    );
}

#[test]
fn start_without_vcs_authorization_links_integrations_page() {
    let env = Env::new();
    env.route("GET", &format!("{AS}/agents"), agents()).route(
        "GET",
        &format!("{RC}/repositories/github"),
        Reply::text(404, "No token found for provider github"),
    );
    let run = env.jcp(&[
        "session",
        "start",
        "--repo",
        "https://github.com/JetBrains/marinator",
        "--detach",
        "Do it",
    ]);
    assert_eq!(run.code, 1);
    assert!(
        run.stderr.contains(&format!(
            "Your github account is not connected to Air. Connect it at http://air.test/org/{ORG}/integrations"
        )),
        "{}",
        run.stderr
    );
    assert!(
        env.fake
            .requests_to("POST", &format!("{AS}/tasks"))
            .is_empty()
    );
}

#[test]
fn start_shows_vcs_authorization_link_from_server() {
    let env = Env::new();
    let message = "GitHub authorization required. To run this task in the cloud, authorize the JetBrains Air app \
                   on GitHub, then resend your request. [Authorize](https://github.com/login/oauth/authorize?x=1)";
    env.route("GET", &format!("{AS}/agents"), agents()).route(
        "POST",
        &format!("{AS}/tasks"),
        Reply::text(403, message),
    );
    let run = env.jcp(&[
        "session",
        "start",
        "--repo",
        "https://github.com/JetBrains/marinator",
        "--branch",
        "master",
        "--detach",
        "Do it",
    ]);
    assert_eq!(run.code, 1);
    assert!(
        run.stderr
            .contains(&format!("You do not have access (403). {message}")),
        "{}",
        run.stderr
    );
    assert!(
        !run.stderr.contains("allowCloudAgentsRun"),
        "{}",
        run.stderr
    );
}

#[test]
fn start_with_ambiguous_env_name_sends_no_task() {
    let env = Env::new();
    let mut configs = env_configs();
    configs[1]["name"] = json!("My env");
    env.route("GET", &format!("{AB}/env-configs"), Reply::json(configs));
    let run = env.jcp(&["session", "start", "--env", "My env", "--detach", "Do it"]);
    assert_eq!(run.code, 1);
    assert!(
        run.stderr.contains("2 environments have the name `My env`"),
        "{}",
        run.stderr
    );
    assert!(run.stderr.contains("env-2"), "{}", run.stderr);
    assert!(
        env.fake
            .requests_to("POST", &format!("{AS}/tasks"))
            .is_empty()
    );
}

#[test]
fn start_with_env_and_other_repo_sends_nothing_to_spawner() {
    let env = Env::new();
    env.route(
        "GET",
        &format!("{AB}/env-configs"),
        Reply::json(env_configs()),
    );
    let run = env.jcp(&[
        "session",
        "start",
        "--env",
        "env-2",
        "--repo",
        "https://github.com/JetBrains/marinator",
        "--detach",
        "Do it",
    ]);
    assert_eq!(run.code, 1);
    assert!(
        run.stderr.contains("The environment `Team env`"),
        "{}",
        run.stderr
    );
    assert!(
        env.paths().iter().all(|p| !p.contains(AS)),
        "{:?}",
        env.paths()
    );
}

#[test]
fn start_follows_session_until_finished() {
    let env = Env::new();
    start_routes(&env);
    let sessions = format!("{AS}/sessions/{SID}");
    let items = [agent_text("Hello"), agent_text(" world")];
    env.route_seq(
        "GET",
        &sessions,
        vec![
            Reply::json(session("LAUNCHING")),
            Reply::json(session("RUNNING")),
            Reply::json(session("FINISHED")),
        ],
    )
    .route_seq(
        "GET",
        &format!("{sessions}/history"),
        vec![history(&[], 0), history(&items[..1], 1), history(&items, 2)],
    );
    let run = env.jcp(&[
        "session",
        "start",
        "--repo",
        "JetBrains/marinator",
        "--interval",
        "0.01",
        "Do it",
    ]);
    ok(&run);
    assert!(run.stdout.contains("] LAUNCHING"), "{}", run.stdout);
    assert!(run.stdout.contains("] RUNNING"), "{}", run.stdout);
    assert!(run.stdout.contains("Hello world"), "{}", run.stdout);
    assert!(
        run.stdout.trim_end().ends_with("FINISHED"),
        "{}",
        run.stdout
    );
    assert_eq!(run.stdout.matches("Hello").count(), 1, "{}", run.stdout);
}

#[test]
fn start_exits_1_when_session_fails() {
    let env = Env::new();
    start_routes(&env);
    let sessions = format!("{AS}/sessions/{SID}");
    env.route_seq(
        "GET",
        &sessions,
        vec![
            Reply::json(session("RUNNING")),
            Reply::json(session("ERROR")),
        ],
    )
    .route("GET", &format!("{sessions}/history"), history(&[], 0));
    let run = env.jcp(&[
        "session",
        "start",
        "--repo",
        "JetBrains/marinator",
        "--interval",
        "0.01",
        "Do it",
    ]);
    assert_eq!(run.code, 1, "{}", run.stderr);
    assert!(run.stdout.contains("ERROR: boom"), "{}", run.stdout);
    assert!(
        run.stderr
            .contains("The session stopped with status ERROR."),
        "{}",
        run.stderr
    );
}

#[test]
fn start_with_unknown_model_lists_models() {
    let env = Env::new();
    start_routes(&env);
    let run = env.jcp(&[
        "session",
        "start",
        "--repo",
        "JetBrains/marinator",
        "--model",
        "nope",
        "Do it",
    ]);
    assert_eq!(run.code, 1);
    assert!(run.stderr.contains("Valid models: d, m."), "{}", run.stderr);
    assert!(
        env.fake
            .requests_to("POST", &format!("{AS}/tasks"))
            .is_empty()
    );
}

// ---------------------------------------------------------------------------------------------
// Other session commands
// ---------------------------------------------------------------------------------------------

fn sessions_list() -> Value {
    json!({"sessions": [session("RUNNING")], "pagination": {"offset": 0, "limit": 20, "totalItems": 25}})
}

#[test]
fn list_shows_table_and_more_pages() {
    let env = Env::new();
    env.route(
        "GET",
        &format!("{AS}/v1/sessions"),
        Reply::json(sessions_list()),
    );
    let run = env.jcp(&["session", "list"]);
    ok(&run);
    assert_eq!(
        env.fake.requests()[0].url,
        format!("{AS}/v1/sessions?offset=0&limit=20")
    );
    let lines: Vec<&str> = run.stdout.lines().collect();
    assert!(lines[0].starts_with("ID"), "{}", run.stdout);
    for part in [
        SID,
        "RUNNING",
        "2026-09-01T10:00:00Z",
        "JetBrains/marinator",
        "Fix the bug",
    ] {
        assert!(lines[1].contains(part), "{}", run.stdout);
    }
    assert!(run.stderr.contains("--offset 1"), "{}", run.stderr);
}

#[test]
fn list_with_search() {
    let env = Env::new();
    env.route(
        "GET",
        &format!("{AS}/v1/sessions/search"),
        Reply::json(sessions_list()),
    );
    let run = env.jcp(&[
        "sessions",
        "list",
        "--search",
        "fix bug",
        "--case-sensitive",
        "--limit",
        "5",
    ]);
    ok(&run);
    assert_eq!(
        env.fake.requests()[0].url,
        format!("{AS}/v1/sessions/search?q=fix+bug&caseSensitive=true&offset=0&limit=5")
    );
    assert!(run.stdout.contains(SID));
}

#[test]
fn list_json_is_server_json() {
    let env = Env::new();
    env.route(
        "GET",
        &format!("{AS}/v1/sessions"),
        Reply::json(sessions_list()),
    );
    let run = env.jcp(&["session", "list", "--json"]);
    ok(&run);
    assert_eq!(
        serde_json::from_str::<Value>(&run.stdout).unwrap(),
        sessions_list()
    );
}

#[test]
fn get_with_task_url() {
    let env = Env::new();
    env.route(
        "GET",
        &format!("{AS}/sessions/{SID}"),
        Reply::json(session("RUNNING")),
    );
    let run = env.jcp(&[
        "session",
        "get",
        &format!("https://air.jetbrains.cloud/org/{ORG}/task/{SID}/editor"),
    ]);
    ok(&run);
    assert!(
        run.stdout.contains(&format!("ID              {SID}")),
        "{}",
        run.stdout
    );
    assert!(
        run.stdout.contains("Status          RUNNING"),
        "{}",
        run.stdout
    );
    assert!(
        run.stdout
            .contains("Repository      github.com/JetBrains/marinator"),
        "{}",
        run.stdout
    );
    assert!(run.stdout.contains(&format!(
        "URL             http://air.test/org/{ORG}/task/{SID}/editor"
    )));
}

#[test]
fn get_with_bad_reference_sends_nothing() {
    let env = Env::new();
    let run = env.jcp(&["session", "get", "../etc"]);
    assert_eq!(run.code, 1);
    assert!(run.stderr.contains("is not a session ID"), "{}", run.stderr);
    assert!(env.fake.requests().is_empty());
}

#[test]
fn status_prints_text() {
    let env = Env::new();
    env.route(
        "GET",
        &format!("{AS}/sessions/{SID}/status"),
        Reply::text(200, "\"RUNNING\""),
    );
    let run = env.jcp(&["session", "status", SID]);
    ok(&run);
    assert_eq!(run.stdout, "RUNNING\n");
}

#[test]
fn wait_until_finished() {
    let env = Env::new();
    env.route_seq(
        "GET",
        &format!("{AS}/sessions/{SID}/status"),
        vec![
            Reply::text(200, "RUNNING"),
            Reply::text(200, "RUNNING"),
            Reply::text(200, "FINISHED"),
        ],
    );
    let run = env.jcp(&["session", "wait", SID, "--interval", "0.01"]);
    ok(&run);
    assert_eq!(run.stdout, "FINISHED\n");
    assert_eq!(env.fake.requests().len(), 3);
}

#[test]
fn wait_stops_when_agent_needs_input() {
    let env = Env::new();
    env.route_seq(
        "GET",
        &format!("{AS}/sessions/{SID}/status"),
        vec![
            Reply::text(200, "RUNNING"),
            Reply::text(200, "USER_INPUT_REQUIRED"),
        ],
    );
    let run = env.jcp(&["session", "wait", SID, "--interval", "0.01"]);
    ok(&run);
    assert_eq!(run.stdout, "USER_INPUT_REQUIRED\n");
    assert_eq!(env.fake.requests().len(), 2);
}

#[test]
fn wait_exits_1_on_error_and_on_timeout() {
    let env = Env::new();
    env.route(
        "GET",
        &format!("{AS}/sessions/{SID}/status"),
        Reply::text(200, "ABORTED"),
    );
    let run = env.jcp(&["session", "wait", SID, "--interval", "0.01"]);
    assert_eq!(run.code, 1);
    assert_eq!(run.stdout, "ABORTED\n");

    let env = Env::new();
    env.route(
        "GET",
        &format!("{AS}/sessions/{SID}/status"),
        Reply::text(200, "RUNNING"),
    );
    let run = env.jcp(&[
        "session",
        "wait",
        SID,
        "--interval",
        "0.01",
        "--timeout",
        "0.1",
    ]);
    assert_eq!(run.code, 1);
    assert!(run.stderr.contains("Timeout"), "{}", run.stderr);
}

#[test]
fn rename_resume_archive_send_method_and_body() {
    let env = Env::new();
    env.route(
        "PATCH",
        &format!("{AS}/sessions/{SID}/name"),
        Reply::no_content(),
    )
    .route(
        "POST",
        &format!("{AS}/sessions/{SID}/resume"),
        Reply::no_content(),
    )
    .route(
        "PUT",
        &format!("{AS}/sessions/{SID}/archive"),
        Reply::no_content(),
    );
    ok(&env.jcp(&["session", "rename", SID, "New name"]));
    ok(&env.jcp(&["session", "resume", SID, "--prompt", "Go on"]));
    ok(&env.jcp(&["session", "resume", SID]));
    ok(&env.jcp(&["session", "archive", SID]));
    let requests = env.fake.requests();
    assert_eq!(
        env.paths(),
        [
            format!("PATCH {AS}/sessions/{SID}/name"),
            format!("POST {AS}/sessions/{SID}/resume"),
            format!("POST {AS}/sessions/{SID}/resume"),
            format!("PUT {AS}/sessions/{SID}/archive"),
        ]
    );
    assert_eq!(requests[0].body_json(), json!({"name": "New name"}));
    assert_eq!(
        requests[1].body_json(),
        json!({"launchConfig": {"prompt": "Go on"}})
    );
    assert_eq!(requests[2].body_json(), json!({}));
}

#[test]
fn delete_without_yes_and_terminal_exits_2() {
    let env = Env::new();
    env.route(
        "DELETE",
        &format!("{AS}/sessions/{SID}"),
        Reply::no_content(),
    );
    let run = env.jcp(&["session", "delete", SID]);
    assert_eq!(run.code, 2);
    assert!(run.stderr.contains("--yes"), "{}", run.stderr);
    assert!(env.fake.requests().is_empty());

    let run = env.jcp(&["session", "delete", SID, "--yes"]);
    ok(&run);
    assert_eq!(env.paths(), [format!("DELETE {AS}/sessions/{SID}")]);
}

#[test]
fn artifacts_and_download() {
    let env = Env::new();
    env.route(
        "GET",
        &format!("{AS}/sessions/{SID}/artifacts"),
        Reply::json(json!([{"name": "out/report.md", "sizeBytes": 2048}])),
    )
    .route(
        "GET",
        &format!("{AS}/sessions/{SID}/artifacts/download"),
        Reply::json(json!({"url": env.url("/presigned/report.md?sig=1")})),
    )
    .route(
        "GET",
        "/presigned/report.md",
        Reply::bytes(200, "text/markdown", b"# Report\n".to_vec()),
    );

    let run = env.jcp(&["session", "artifacts", SID]);
    ok(&run);
    assert_eq!(run.stdout, "NAME           SIZE\nout/report.md  2.0 KiB\n");

    let out_file = env.dir.path().join("saved.md");
    ok(&env.jcp(&[
        "session",
        "download",
        SID,
        "out/report.md",
        "-o",
        out_file.to_str().unwrap(),
    ]));
    assert_eq!(std::fs::read(&out_file).unwrap(), b"# Report\n");

    ok(&env.jcp(&["session", "download", SID, "out/report.md"]));
    assert_eq!(
        std::fs::read(env.dir.path().join("report.md")).unwrap(),
        b"# Report\n"
    );

    let run = env.jcp(&["session", "download", SID, "out/report.md", "-o", "-"]);
    ok(&run);
    assert_eq!(run.stdout_bytes, b"# Report\n");

    let downloads = env
        .fake
        .requests_to("GET", &format!("{AS}/sessions/{SID}/artifacts/download"));
    assert_eq!(
        downloads[0].url,
        format!("{AS}/sessions/{SID}/artifacts/download?name=out%2Freport.md")
    );
    assert_eq!(env.fake.requests_to("GET", "/presigned/report.md").len(), 3);
}

fn debug_data(env: &Env) -> Value {
    json!({
        "id": SID, "devEnvId": "orca-env-1", "externalUrl": "https://ext.test",
        "exposedPorts": [{"localPort": 8080, "publicUrl": "https://port.test"}],
        "logs": [
            {"name": "job-0-workspace.log.gz", "downloadUrl": env.url("/presigned/workspace.log.gz"), "sizeBytes": 1536},
            {"name": "job-0-worker.log", "downloadUrl": env.url("/presigned/worker.log")}
        ]
    })
}

#[test]
fn diag_shows_all_parts() {
    let env = Env::new();
    env.route(
        "GET",
        &format!("{AS}/sessions/{SID}"),
        Reply::json(session("RUNNING")),
    )
    .route(
        "GET",
        &format!("{AS}/debug/{SID}"),
        Reply::json(debug_data(&env)),
    )
    .route(
        "GET",
        &format!("{AS}/sessions/{SID}/history"),
        history(&[agent_text("Last words")], 1),
    )
    .route(
        "GET",
        &format!("{AS}/sessions/{SID}/artifacts"),
        Reply::json(json!([{"name": "report.md", "sizeBytes": 10}])),
    );
    let run = env.jcp(&["session", "diag", SID]);
    ok(&run);
    for part in [
        "Status          RUNNING",
        "orca-cli --stack staging ssh orca-env-1",
        "https://ext.test",
        "8080 -> https://port.test",
        "job-0-workspace.log.gz  1.5 KiB",
        "Last words",
        "report.md  10 B",
        &format!("http://air.test/org/{ORG}/task/{SID}/editor"),
    ] {
        assert!(run.stdout.contains(part), "no `{part}` in:\n{}", run.stdout);
    }
    assert!(!run.stdout.contains("Failed parts"), "{}", run.stdout);
    let history = &env
        .fake
        .requests_to("GET", &format!("{AS}/sessions/{SID}/history"))[0];
    assert!(
        history.url.ends_with("offset=0&limit=21"),
        "{}",
        history.url
    );

    let run = env.jcp(&["session", "diag", SID, "--json"]);
    ok(&run);
    let report: Value = serde_json::from_str(&run.stdout).unwrap();
    assert_eq!(report["debug"]["devEnvId"], "orca-env-1");
    assert_eq!(report["session"]["status"], "RUNNING");
    assert_eq!(report["artifacts"][0]["name"], "report.md");
    assert_eq!(report["errors"], json!({}));
}

#[test]
fn diag_shows_other_parts_when_debug_fails() {
    let env = Env::new();
    env.route(
        "GET",
        &format!("{AS}/sessions/{SID}"),
        Reply::json(session("RUNNING")),
    )
    .route(
        "GET",
        &format!("{AS}/debug/{SID}"),
        Reply::text(500, "internal"),
    )
    .route(
        "GET",
        &format!("{AS}/sessions/{SID}/history"),
        history(&[agent_text("Last words")], 1),
    )
    .route(
        "GET",
        &format!("{AS}/sessions/{SID}/artifacts"),
        Reply::json(json!([])),
    );
    let run = env.jcp(&["session", "diag", SID]);
    ok(&run);
    assert!(
        run.stdout.contains("Status          RUNNING"),
        "{}",
        run.stdout
    );
    assert!(run.stdout.contains("Last words"), "{}", run.stdout);
    assert!(
        run.stdout.contains("Cannot get the debug data"),
        "{}",
        run.stdout
    );
    assert!(
        run.stdout.contains("Failed parts: debug."),
        "{}",
        run.stdout
    );

    let run = env.jcp(&["session", "diag", SID, "--json"]);
    ok(&run);
    let report: Value = serde_json::from_str(&run.stdout).unwrap();
    assert!(
        report["errors"]["debug"].as_str().unwrap().contains("500"),
        "{report}"
    );
    assert_eq!(report["session"]["status"], "RUNNING");
}

fn follow_routes(env: &Env) {
    let items = [
        tool_call("Read A"),
        tool_call("Read B"),
        tool_call("Read C"),
    ];
    env.route_seq(
        "GET",
        &format!("{AS}/sessions/{SID}"),
        vec![
            Reply::json(session("RUNNING")),
            Reply::json(session("RUNNING")),
            Reply::json(session("FINISHED")),
        ],
    )
    .route_seq(
        "GET",
        &format!("{AS}/sessions/{SID}/history"),
        vec![
            history(&items[..1], 1),
            history(&items[..2], 2),
            history(&items, 3),
        ],
    );
}

#[test]
fn watch_shows_each_change_once() {
    let env = Env::new();
    follow_routes(&env);
    let run = env.jcp(&["session", "watch", SID, "--interval", "0.01"]);
    ok(&run);
    for part in ["] RUNNING", "Read A", "Read B", "Read C", "] FINISHED"] {
        assert_eq!(
            run.stdout.matches(part).count(),
            1,
            "`{part}` in:\n{}",
            run.stdout
        );
    }
    let a = run.stdout.find("Read A").unwrap();
    let c = run.stdout.find("Read C").unwrap();
    assert!(
        a < c && c < run.stdout.find("] FINISHED").unwrap(),
        "{}",
        run.stdout
    );
}

#[test]
fn history_follow_shows_each_item_once() {
    let env = Env::new();
    follow_routes(&env);
    let run = env.jcp(&["session", "history", SID, "--follow", "--interval", "0.01"]);
    ok(&run);
    for part in ["Read A", "Read B", "Read C"] {
        assert_eq!(
            run.stdout.matches(part).count(),
            1,
            "`{part}` in:\n{}",
            run.stdout
        );
    }
    assert!(!run.stdout.contains("RUNNING"), "{}", run.stdout);
}

#[test]
fn history_tail_and_json() {
    let env = Env::new();
    let items = [agent_text("first"), tool_call("second")];
    env.route(
        "GET",
        &format!("{AS}/sessions/{SID}/history"),
        history(&items, 7),
    );
    let run = env.jcp(&["session", "history", SID, "--tail", "2"]);
    ok(&run);
    let request = &env.fake.requests()[0];
    assert_eq!(
        request.url,
        format!("{AS}/sessions/{SID}/history?offset=0&limit=3")
    );
    let first = run.stdout.find("first").expect(&run.stdout);
    assert!(
        first < run.stdout.find("second").expect(&run.stdout),
        "{}",
        run.stdout
    );

    let run = env.jcp(&["session", "history", SID, "--tail", "2", "--json"]);
    ok(&run);
    assert_eq!(run.stdout, format!("{}\n{}\n", items[0], items[1]));
}

/// A history page with the indexes `first..`. `items` are in chronological order.
fn history_page(first: u64, items: &[String], total: u64) -> Reply {
    let messages: Vec<Value> = items
        .iter()
        .enumerate()
        .rev()
        .map(|(i, m)| json!({"index": first + i as u64, "message": m}))
        .collect();
    Reply::json(
        json!({"messages": messages, "pagination": {"offset": 0, "limit": 1000, "totalItems": total}}),
    )
}

fn usage_update() -> String {
    json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": SID, "update": {
        "sessionUpdate": "usage_update", "used": 10, "size": 100}}})
    .to_string()
}

#[test]
fn history_tail_counts_message_chunks_as_one_entry() {
    let env = Env::new();
    let items = [
        tool_call("Read A"),
        agent_text("one "),
        agent_text("two "),
        usage_update(),
        agent_text("three"),
    ];
    let path = format!("{AS}/sessions/{SID}/history");
    env.route(
        "GET",
        &format!("{path}?offset=0&limit=2"),
        history_page(3, &items[3..], 5),
    )
    .route(
        "GET",
        &format!("{path}?offset=2&limit=2"),
        history_page(1, &items[1..3], 5),
    )
    .route(
        "GET",
        &format!("{path}?offset=4&limit=4"),
        history_page(0, &items[..1], 5),
    );
    let run = env.jcp(&["session", "history", SID, "--tail", "1"]);
    ok(&run);
    assert!(
        run.stdout.contains("agent ▎one two three\n"),
        "{}",
        run.stdout
    );
    assert!(!run.stdout.contains("Read A"), "{}", run.stdout);
    assert!(!run.stdout.contains("event"), "{}", run.stdout);
    let urls: Vec<String> = env.fake.requests().iter().map(|r| r.url.clone()).collect();
    assert_eq!(
        urls,
        [
            format!("{path}?offset=0&limit=2"),
            format!("{path}?offset=2&limit=2"),
            format!("{path}?offset=4&limit=4"),
        ]
    );

    let run = env.jcp(&["session", "history", SID, "--tail", "1", "--json"]);
    ok(&run);
    assert_eq!(run.stdout, format!("{}\n", items[1..].join("\n")));
}

#[test]
fn history_shows_each_item_once_when_pages_overlap() {
    let env = Env::new();
    let items = [
        tool_call("Read A"),
        agent_text("x1"),
        agent_text("x2"),
        agent_text("x3"),
        tool_call("Read B"),
    ];
    // The item `Read B` comes after the first page, so the second page starts one item earlier
    env.route_seq(
        "GET",
        &format!("{AS}/sessions/{SID}/history"),
        vec![
            history_page(1, &items[1..4], 4),
            history_page(0, &items[..2], 5),
        ],
    );
    let run = env.jcp(&["session", "history", SID, "--tail", "2"]);
    ok(&run);
    assert_eq!(run.stdout.matches("Read A").count(), 1, "{}", run.stdout);
    assert!(run.stdout.contains("agent ▎x1x2x3\n"), "{}", run.stdout);
    assert_eq!(env.fake.requests().len(), 2);
}

#[test]
fn history_does_not_wrap_when_stdout_is_not_a_terminal() {
    let env = Env::new();
    let text = "word ".repeat(60);
    env.route(
        "GET",
        &format!("{AS}/sessions/{SID}/history"),
        history(&[agent_text(&text)], 1),
    );
    let run = env.jcp(&["session", "history", SID]);
    ok(&run);
    let line = run
        .stdout
        .lines()
        .find(|l| l.contains("agent"))
        .expect(&run.stdout);
    assert!(line.ends_with(text.as_str()), "{}", run.stdout);
}

#[test]
fn debug_shows_env_ports_and_logs() {
    let env = Env::new();
    env.route(
        "GET",
        &format!("{AS}/debug/{SID}"),
        Reply::json(debug_data(&env)),
    );
    let run = env.jcp(&["session", "debug", SID]);
    ok(&run);
    for part in [
        "orca-env-1",
        "orca-cli --stack staging ssh orca-env-1",
        "job-0-worker.log        -",
    ] {
        assert!(run.stdout.contains(part), "no `{part}` in:\n{}", run.stdout);
    }
    let run = env.jcp(&["session", "debug", SID, "--json"]);
    ok(&run);
    assert_eq!(
        serde_json::from_str::<Value>(&run.stdout).unwrap(),
        debug_data(&env)
    );
}

fn gzip(text: &str) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(text.as_bytes()).unwrap();
    encoder.finish().unwrap()
}

#[test]
fn logs_archive_list_and_one_log() {
    let env = Env::new();
    let zip = b"PK\x03\x04 fake zip \x00\xff".to_vec();
    env.route(
        "POST",
        &format!("{AS}/debug/{SID}/logs/archive"),
        Reply::bytes(200, "application/zip", zip.clone()),
    )
    .route(
        "GET",
        &format!("{AS}/debug/{SID}"),
        Reply::json(debug_data(&env)),
    )
    .route(
        "GET",
        "/presigned/workspace.log.gz",
        Reply::bytes(200, "application/gzip", gzip("l1\nl2\nl3\nl4\nl5\n")),
    );

    let out_file = env.dir.path().join("logs.zip");
    ok(&env.jcp(&["session", "logs", SID, "-o", out_file.to_str().unwrap()]));
    assert_eq!(std::fs::read(&out_file).unwrap(), zip);
    ok(&env.jcp(&["session", "logs", SID]));
    assert_eq!(
        std::fs::read(env.dir.path().join(format!("{SID}-logs.zip"))).unwrap(),
        zip
    );

    let run = env.jcp(&["session", "logs", SID, "--list"]);
    ok(&run);
    assert_eq!(
        run.stdout,
        "NAME                    SIZE\njob-0-workspace.log.gz  1.5 KiB\njob-0-worker.log        -\n"
    );

    let run = env.jcp(&[
        "session",
        "logs",
        SID,
        "--name",
        "job-0-workspace.log.gz",
        "--tail",
        "3",
    ]);
    ok(&run);
    assert_eq!(run.stdout, "l3\nl4\nl5\n");

    let run = env.jcp(&["session", "logs", SID, "--name", "x.log"]);
    assert_eq!(run.code, 1);
    assert!(
        run.stderr
            .contains("Logs: job-0-workspace.log.gz, job-0-worker.log."),
        "{}",
        run.stderr
    );
}

// ---------------------------------------------------------------------------------------------
// env, repo, agents
// ---------------------------------------------------------------------------------------------

#[test]
fn env_list_and_filters() {
    let env = Env::new();
    env.route(
        "GET",
        &format!("{AB}/env-configs"),
        Reply::json(env_configs()),
    );
    let run = env.jcp(&["env", "list"]);
    ok(&run);
    assert_eq!(
        env.fake.requests()[0].url,
        format!("{AB}/env-configs?includeDrafts=false")
    );
    let lines: Vec<&str> = run.stdout.lines().collect();
    assert_eq!(lines.len(), 3, "{}", run.stdout);
    assert!(lines[0].starts_with("ID     NAME"), "{}", run.stdout);
    assert!(
        lines[1].contains("JetBrains/marinator +1") && lines[1].contains(" me "),
        "{}",
        run.stdout
    );
    assert!(
        lines[2].contains("JetBrains/air") && lines[2].contains("shared"),
        "{}",
        run.stdout
    );

    let mine = env.jcp(&["envs", "list", "--mine"]);
    assert!(
        mine.stdout.contains("env-1") && !mine.stdout.contains("env-2"),
        "{}",
        mine.stdout
    );
    let shared = env.jcp(&["environment", "list", "--shared"]);
    assert!(
        !shared.stdout.contains("env-1") && shared.stdout.contains("env-2"),
        "{}",
        shared.stdout
    );
    let repo = env.jcp(&[
        "env",
        "list",
        "--repo",
        "https://github.com/jetbrains/air.git",
    ]);
    assert!(
        !repo.stdout.contains("env-1") && repo.stdout.contains("env-2"),
        "{}",
        repo.stdout
    );
    let ids = |run: &Run| -> Vec<String> {
        ok(run);
        let items: Vec<Value> = serde_json::from_str(&run.stdout).unwrap();
        items
            .iter()
            .map(|i| i["id"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(
        ids(&env.jcp(&["env", "list", "--json"])),
        ["env-1", "env-2"]
    );
    let mine_json = env.jcp(&["env", "list", "--mine", "--json"]);
    assert_eq!(ids(&mine_json), ["env-1"]);
    // The server JSON is kept as it is, with the fields that the CLI does not read
    assert_eq!(
        serde_json::from_str::<Value>(&mine_json.stdout).unwrap()[0],
        env_configs()[0]
    );
    assert_eq!(
        ids(&env.jcp(&["env", "list", "--shared", "--json"])),
        ["env-2"]
    );
    assert_eq!(
        ids(&env.jcp(&["env", "list", "--repo", "JetBrains/air", "--json"])),
        ["env-2"]
    );
    ok(&env.jcp(&["env", "list", "--drafts", "--project", "p1"]));
    assert_eq!(
        env.fake.requests().last().unwrap().url,
        format!("{AB}/env-configs?includeDrafts=true&orgProjectId=p1")
    );
}

#[test]
fn env_get_by_name() {
    let env = Env::new();
    let mut details = env_configs()[0].clone();
    details["variables"] = json!([{"key": "API_URL", "value": "https://x"}]);
    details["personalSecrets"] = json!([{"key": "TOKEN", "value": "***", "isSecret": true}]);
    env.route(
        "GET",
        &format!("{AB}/env-configs"),
        Reply::json(env_configs()),
    )
    .route(
        "GET",
        &format!("{AB}/env-configs/env-1"),
        Reply::json(details),
    );
    let run = env.jcp(&["env", "get", "My env"]);
    ok(&run);
    assert_eq!(
        env.paths(),
        [
            format!("GET {AB}/env-configs"),
            format!("GET {AB}/env-configs/env-1")
        ]
    );
    for part in ["env-1", "My env", "API_URL", "TOKEN"] {
        assert!(run.stdout.contains(part), "no `{part}` in:\n{}", run.stdout);
    }
    assert!(!run.stdout.contains("https://x"), "{}", run.stdout);
}

#[test]
fn repo_list_warns_with_manage_url() {
    let env = Env::new();
    env.route(
        "GET",
        &format!("{RC}/repositories"),
        Reply::json(json!({
            "data": [marinator()],
            "providerErrors": [{"provider": "github", "error": "APP_NOT_INSTALLED",
                "target": {"type": "ORGANIZATION", "name": "Acme"}, "manageUrl": "https://github.com/apps/air/installations/new"}]
        })),
    );
    let run = env.jcp(&["repos", "list", "--limit", "5"]);
    ok(&run);
    assert_eq!(
        env.fake.requests()[0].url,
        format!("{RC}/repositories?limit=5")
    );
    assert!(
        run.stdout.contains("JetBrains/marinator  github    yes/no"),
        "{}",
        run.stdout
    );
    assert!(
        run.stderr
            .contains("Fix it at https://github.com/apps/air/installations/new"),
        "{}",
        run.stderr
    );
}

#[test]
fn repo_branches_and_providers() {
    let env = Env::new();
    env.route("GET", &format!("{RC}/repositories/search"), Reply::json(json!({"data": [marinator()]})))
        .route(
            "GET",
            &format!("{RC}/repositories/github/branches"),
            Reply::json(json!({"data": [{"name": "main", "commitSha": "abc"}, {"name": "dev"}]})),
        )
        .route(
            "GET",
            &format!("{RC}/providers"),
            Reply::json(json!({"authorizedProviders": [{"provider": "github", "username": "octo", "manage": "https://m"}]})),
        );
    let run = env.jcp(&["repo", "branches", "JetBrains/marinator", "--search", "ma"]);
    ok(&run);
    assert_eq!(run.stdout, "NAME  COMMIT\nmain  abc\ndev\n");
    let branches = &env
        .fake
        .requests_to("GET", &format!("{RC}/repositories/github/branches"))[0];
    assert!(
        branches.url.contains("fullName=JetBrains%2Fmarinator"),
        "{}",
        branches.url
    );
    assert!(branches.url.contains("q=ma"), "{}", branches.url);

    let run = env.jcp(&["repo", "providers"]);
    ok(&run);
    assert!(
        run.stdout.contains("github") && run.stdout.contains("octo"),
        "{}",
        run.stdout
    );
}

#[test]
fn agents_marks_defaults() {
    let env = Env::new();
    env.route("GET", &format!("{AS}/agents"), agents());
    let run = env.jcp(&["agents"]);
    ok(&run);
    assert!(
        run.stdout
            .starts_with("claude (Claude) [default]\n  model d* (Default)  reasoning: low, high\n"),
        "{}",
        run.stdout
    );
    assert!(run.stdout.contains("codex (Codex)"), "{}", run.stdout);
}

#[test]
fn empty_lists_print_one_line() {
    let env = Env::new();
    let sessions =
        json!({"sessions": [], "pagination": {"offset": 0, "limit": 20, "totalItems": 0}});
    env.route(
        "GET",
        &format!("{AS}/v1/sessions"),
        Reply::json(sessions.clone()),
    )
    .route(
        "GET",
        &format!("{AS}/sessions/{SID}/artifacts"),
        Reply::json(json!([])),
    )
    .route(
        "GET",
        &format!("{AS}/debug/{SID}"),
        Reply::json(json!({"id": SID, "logs": []})),
    )
    .route("GET", &format!("{AB}/env-configs"), Reply::json(json!([])))
    .route(
        "GET",
        &format!("{RC}/repositories"),
        Reply::json(json!({"data": []})),
    )
    .route(
        "GET",
        &format!("{RC}/repositories/github/branches"),
        Reply::json(json!({"data": []})),
    )
    .route(
        "GET",
        &format!("{RC}/providers"),
        Reply::json(json!({"authorizedProviders": []})),
    );
    let cases = [
        (vec!["session", "list"], "No sessions.\n"),
        (vec!["session", "artifacts", SID], "No artifacts.\n"),
        (vec!["session", "logs", SID, "--list"], "No log files.\n"),
        (vec!["env", "list"], "No environments.\n"),
        (vec!["repo", "list"], "No repositories.\n"),
        (
            vec!["repo", "branches", "https://github.com/JetBrains/marinator"],
            "No branches.\n",
        ),
        (
            vec!["repo", "providers"],
            "No VCS accounts are connected.\n",
        ),
    ];
    for (args, expected) in cases {
        let run = env.jcp(&args);
        ok(&run);
        assert_eq!(run.stdout, expected, "{args:?}");
    }
    let run = env.jcp(&["session", "list", "--json"]);
    ok(&run);
    assert_eq!(
        serde_json::from_str::<Value>(&run.stdout).unwrap(),
        sessions
    );
    let run = env.jcp(&["session", "artifacts", SID, "--json"]);
    ok(&run);
    assert_eq!(run.stdout, "[]\n");
}

// ---------------------------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------------------------

#[test]
fn http_errors_have_clear_messages() {
    let env = Env::new();
    env.route("GET", &format!("{AS}/v1/sessions"), Reply::text(401, ""))
        .route(
            "GET",
            &format!("{AS}/agents"),
            Reply::text(403, "Cloud agent runs are disabled by AI Management policy"),
        )
        .route(
            "GET",
            &format!("{AS}/sessions/{SID}"),
            Reply::text(404, "no session"),
        )
        .route(
            "PATCH",
            &format!("{AS}/sessions/{SID}/name"),
            Reply::text(400, "The name is too long"),
        );

    let cases = [
        (vec!["session", "list"], "jcp login"),
        (vec!["agents"], "allowCloudAgentsRun"),
        (vec!["session", "get", SID], "Not found (404)"),
        (
            vec!["session", "rename", SID, "x"],
            "Bad request (400): The name is too long",
        ),
    ];
    for (args, message) in cases {
        let run = env.jcp(&args);
        assert_eq!(run.code, 1, "{args:?}");
        assert!(run.stderr.contains(message), "{args:?}: {}", run.stderr);
        assert!(
            run.stderr.starts_with("Error: "),
            "{args:?}: {}",
            run.stderr
        );
    }
}

#[test]
fn static_token_is_not_renewed_on_401() {
    let env = Env::new();
    env.route(
        "GET",
        &format!("{AS}/sessions/{SID}/status"),
        Reply::text(401, ""),
    );
    let run = env.jcp(&["session", "wait", SID, "--interval", "0.01"]);
    assert_eq!(run.code, 1);
    assert!(run.stderr.contains("jcp login"), "{}", run.stderr);
    assert_eq!(env.fake.requests().len(), 1);
}

/// A JWT with one organization and one workspace. The CLI does not verify the signature.
const ORGS_USER_INFO: &str = "eyJhbGciOiJub25lIn0.eyJvcmdNZW1iZXJzaGlwcyI6W3sib3JnSWQiOiJPUkciLCJ3b3Jrc3BhY2VzIjpbeyJpZCI6IldTIn1dfV19.sig";

/// Runs jcp with the login from the keychain file, not with `JCP_ACCESS_TOKEN`. The first login gives the token
/// `A`, the second login gives the token `B`.
fn jcp_with_login(env: &Env, args: &[&str]) -> Run {
    env.check_tokens.set(false);
    let keychain = env.dir.path().join("secrets.toml");
    std::fs::write(&keychain, "[secrets]\n\"refresh-token\" = \"r1\"\n").unwrap();
    // Each login refreshes the token and then switches the audience
    env.route_seq(
        "POST",
        "/oauth2/token",
        vec![
            Reply::json(json!({"access_token": "login-1"})),
            Reply::json(json!({"access_token": "A"})),
            Reply::json(json!({"access_token": "login-2"})),
            Reply::json(json!({"access_token": "B"})),
        ],
    )
    .route("GET", "/org/orgsuserinfo", Reply::text(200, ORGS_USER_INFO));
    let output = Command::new(env!("CARGO_BIN_EXE_jcp"))
        .arg("--staging")
        .args(args)
        .current_dir(env.dir.path())
        .env("JCP_API_URL", env.fake.base_url())
        .env("OAUTH_URL", env.url("/oauth2"))
        .env("KEYCHAIN_FILE", &keychain)
        .env_remove("JCP_ACCESS_TOKEN")
        .env_remove("JCP_ENVIRONMENT")
        .output()
        .unwrap();
    Run {
        code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stdout_bytes: output.stdout,
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

fn status_tokens(env: &Env) -> Vec<Option<String>> {
    env.fake
        .requests_to("GET", &format!("{AS}/sessions/{SID}/status"))
        .into_iter()
        .map(|r| r.authorization)
        .collect()
}

#[test]
fn login_token_is_renewed_one_time_on_401() {
    let env = Env::new();
    env.route_seq(
        "GET",
        &format!("{AS}/sessions/{SID}/status"),
        vec![Reply::text(401, ""), Reply::text(200, "FINISHED")],
    );
    let run = jcp_with_login(&env, &["session", "wait", SID, "--interval", "0.01"]);
    ok(&run);
    assert_eq!(run.stdout, "FINISHED\n");
    assert_eq!(
        status_tokens(&env),
        [Some("Bearer A".to_string()), Some("Bearer B".to_string())]
    );
    assert_eq!(env.fake.requests_to("POST", "/oauth2/token").len(), 4);
    assert_eq!(env.fake.requests_to("GET", "/org/orgsuserinfo").len(), 2);
}

#[test]
fn second_401_after_renewal_is_an_error() {
    let env = Env::new();
    env.route(
        "GET",
        &format!("{AS}/sessions/{SID}/status"),
        Reply::text(401, ""),
    );
    let run = jcp_with_login(&env, &["session", "wait", SID, "--interval", "0.01"]);
    assert_eq!(run.code, 1);
    assert!(run.stderr.contains("jcp login"), "{}", run.stderr);
    assert_eq!(
        status_tokens(&env),
        [Some("Bearer A".to_string()), Some("Bearer B".to_string())]
    );
}

#[test]
fn command_without_token_and_login_asks_for_login() {
    // Without JCP_ACCESS_TOKEN and without a keychain, the command asks for a login.
    let env = Env::new();
    let output = Command::new(env!("CARGO_BIN_EXE_jcp"))
        .args(["--staging", "session", "list"])
        .current_dir(env.dir.path())
        .env("JCP_API_URL", env.fake.base_url())
        .env(
            "KEYCHAIN_FILE",
            Path::new(env.dir.path()).join("no-keychain"),
        )
        .env_remove("JCP_ACCESS_TOKEN")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("jcp login"));
    assert!(env.fake.requests().is_empty());
}
