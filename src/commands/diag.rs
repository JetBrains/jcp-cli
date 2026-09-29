//! Diagnostics of a session: watch, history, debug data, logs, and the combined `diag` report.

use super::{
    CliError, Clock, Services,
    format::{human_size, or_dash, table, time_of_day},
    history::{self, HistoryRenderer},
    session::{artifacts_table, describe_session, write_output},
    usage,
};
use crate::api::spawner::{FINAL_STATUSES, LogFileInfo, SessionDebugData, SpawnerApi};
use flate2::read::MultiGzDecoder;
use serde_json::{Map, Value as JsonValue, json};
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};

/// History items in the `diag` report
const DIAG_HISTORY_ITEMS: u64 = 20;

pub struct WatchOptions {
    pub interval: Duration,
    /// Show a line for each status change
    pub show_status: bool,
    /// Show only the last N items that exist at the start. `None` shows all items.
    pub initial_tail: Option<u64>,
    /// Print the raw JSON-RPC messages, one on each line
    pub json: bool,
}

/// Polls the session and its history until the status is final. Returns the final status.
///
/// Each poll gets the session first and the history second. Thus the history is complete when the status is final.
pub fn watch(
    api: &SpawnerApi,
    clock: &dyn Clock,
    id: &str,
    options: &WatchOptions,
    out: &mut impl Write,
) -> Result<String, CliError> {
    let mut renderer = HistoryRenderer::new(&mut *out);
    let mut last_status: Option<String> = None;
    let mut last_index: Option<u64> = None;
    let mut first = true;
    loop {
        let session = api.get_session(id)?.value;
        let items = match options.initial_tail {
            Some(tail) if first => history::fetch(api, id, Some(tail))?,
            _ => history::fetch_newer(api, id, last_index)?,
        };
        first = false;
        if let Some(last) = items.last() {
            last_index = Some(last.index);
        }
        let status = session.status.clone();
        let is_final = FINAL_STATUSES.contains(&status.as_str());
        let changed = last_status.as_deref() != Some(status.as_str());
        let status_line = || {
            let time = time_of_day(clock.now());
            match session.status_comment.as_deref().filter(|c| !c.is_empty()) {
                Some(comment) => format!("[{time}] {status}: {comment}"),
                None => format!("[{time}] {status}"),
            }
        };
        if options.show_status && changed && !is_final {
            renderer.plain_line(&status_line())?;
        }
        for item in &items {
            if options.json {
                renderer.plain_line(&item.message)?;
            } else {
                renderer.render(&item.message);
            }
        }
        if is_final {
            if options.show_status {
                renderer.plain_line(&status_line())?;
            } else {
                renderer.finish();
            }
            return Ok(status);
        }
        last_status = Some(status);
        clock.sleep(options.interval);
    }
}

pub struct HistoryArgs {
    pub follow: bool,
    pub tail: Option<u64>,
    pub json: bool,
    pub interval: Duration,
}

pub fn history(
    api: &SpawnerApi,
    clock: &dyn Clock,
    id: &str,
    args: &HistoryArgs,
    out: &mut impl Write,
) -> Result<(), CliError> {
    if args.follow {
        let options = WatchOptions {
            interval: args.interval,
            show_status: false,
            initial_tail: args.tail,
            json: args.json,
        };
        watch(api, clock, id, &options, out)?;
        return Ok(());
    }
    let items = history::fetch(api, id, args.tail)?;
    if args.json {
        for item in &items {
            writeln!(out, "{}", item.message)?;
        }
        return Ok(());
    }
    let mut renderer = HistoryRenderer::new(&mut *out);
    for item in &items {
        renderer.render(&item.message);
    }
    renderer.finish();
    Ok(())
}

/// Makes the text of the debug data.
pub fn describe_debug(debug: &SessionDebugData, orca_stack: &str) -> String {
    let mut out = String::new();
    let mut line = |name: &str, value: String| out.push_str(&format!("{name:<16}{value}\n"));
    line("Session", debug.id.clone());
    line("Orca env", or_dash(debug.dev_env_id.as_deref()));
    if let Some(hint) = ssh_hint(debug, orca_stack) {
        line("SSH", hint);
    }
    line("External URL", or_dash(debug.external_url.as_deref()));
    for port in &debug.exposed_ports {
        let local = port.local_port.map(|p| p.to_string()).unwrap_or("-".into());
        line(
            "Port",
            format!("{local} -> {}", or_dash(port.public_url.as_deref())),
        );
    }
    if let Some(e) = debug.get_env_exception.as_deref() {
        line("Env error", e.to_string());
    }
    if let Some(e) = debug.logs_exception.as_deref() {
        line("Logs error", e.to_string());
    }
    if !debug.logs.is_empty() {
        out.push_str("\nLogs:\n");
        out.push_str(&logs_table(&debug.logs));
    }
    out
}

fn ssh_hint(debug: &SessionDebugData, orca_stack: &str) -> Option<String> {
    debug
        .dev_env_id
        .as_deref()
        .filter(|id| !id.is_empty() && *id != "UNDEFINED")
        .map(|id| format!("orca-cli --stack {orca_stack} ssh {id}"))
}

pub fn logs_table(logs: &[LogFileInfo]) -> String {
    let rows: Vec<Vec<String>> = logs
        .iter()
        .map(|l| {
            vec![
                l.name.clone(),
                l.size_bytes.map(human_size).unwrap_or("-".into()),
            ]
        })
        .collect();
    table(&["NAME", "SIZE"], &rows)
}

pub fn debug(
    services: &Services,
    api: &SpawnerApi,
    id: &str,
    json: bool,
    out: &mut impl Write,
) -> Result<(), CliError> {
    let response = api.debug(id)?;
    if json {
        writeln!(out, "{}", response.raw)?;
    } else {
        write!(
            out,
            "{}",
            describe_debug(&response.value, &services.orca_stack)
        )?;
    }
    Ok(())
}

pub struct LogsArgs<'a> {
    pub list: bool,
    pub name: Option<&'a str>,
    pub tail: Option<usize>,
    pub output: Option<&'a Path>,
}

pub fn logs(
    api: &SpawnerApi,
    id: &str,
    args: &LogsArgs,
    out: &mut impl Write,
    err: &mut impl Write,
) -> Result<(), CliError> {
    if !args.list && args.name.is_none() {
        if args.tail.is_some() {
            return Err(usage("Use --tail only with --name."));
        }
        let bytes = api.logs_archive(id)?;
        let path = args
            .output
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from(format!("{id}-logs.zip")));
        return write_output(&path, &bytes, out, err);
    }
    let debug = api.debug(id)?.value;
    if let Some(e) = debug.logs_exception.as_deref() {
        writeln!(err, "Warning: the server cannot list all logs: {e}")?;
    }
    let Some(name) = args.name else {
        write!(out, "{}", logs_table(&debug.logs))?;
        return Ok(());
    };
    let Some(log) = debug.logs.iter().find(|l| l.name == name) else {
        let names: Vec<&str> = debug.logs.iter().map(|l| l.name.as_str()).collect();
        return Err(usage(format!(
            "The session has no log `{name}`. Logs: {}.",
            if names.is_empty() {
                "none".to_string()
            } else {
                names.join(", ")
            }
        )));
    };
    let mut bytes = api.download(&log.download_url)?;
    if bytes.starts_with(&[0x1f, 0x8b]) {
        let mut decoded = Vec::new();
        MultiGzDecoder::new(bytes.as_slice()).read_to_end(&mut decoded)?;
        bytes = decoded;
    }
    if let Some(tail) = args.tail {
        bytes = last_lines(&bytes, tail);
    }
    match args.output {
        Some(path) => write_output(path, &bytes, out, err),
        None => {
            out.write_all(&bytes)?;
            out.flush()?;
            Ok(())
        }
    }
}

/// Returns the last `n` lines of the text.
pub fn last_lines(bytes: &[u8], n: usize) -> Vec<u8> {
    let body = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    if n == 0 || body.is_empty() {
        return Vec::new();
    }
    let start = body
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, b)| **b == b'\n')
        .nth(n - 1)
        .map(|(i, _)| i + 1)
        .unwrap_or(0);
    let mut result = body[start..].to_vec();
    result.push(b'\n');
    result
}

/// Prints the combined report: session, debug data, the last history items and the artifacts.
///
/// A failed part does not stop the other parts. The report shows each failed part.
pub fn diag(
    services: &Services,
    api: &SpawnerApi,
    id: &str,
    json: bool,
    out: &mut impl Write,
) -> Result<(), CliError> {
    let session = api.get_session(id);
    let debug = api.debug(id);
    let items = history::fetch(api, id, Some(DIAG_HISTORY_ITEMS));
    let artifacts = api.artifacts(id);

    if json {
        let mut errors = Map::new();
        let mut part = |name: &str, raw: Result<String, String>| match raw {
            Ok(raw) => serde_json::from_str::<JsonValue>(&raw).unwrap_or(JsonValue::String(raw)),
            Err(e) => {
                errors.insert(name.to_string(), JsonValue::String(e));
                JsonValue::Null
            }
        };
        let session_json = part("session", session.map(|r| r.raw).map_err(|e| e.to_string()));
        let debug_json = part("debug", debug.map(|r| r.raw).map_err(|e| e.to_string()));
        let artifacts_json = part(
            "artifacts",
            artifacts.map(|r| r.raw).map_err(|e| e.to_string()),
        );
        let history_json = match items {
            Ok(items) => JsonValue::Array(
                items
                    .iter()
                    .map(|i| {
                        serde_json::from_str(&i.message)
                            .unwrap_or(JsonValue::String(i.message.clone()))
                    })
                    .collect(),
            ),
            Err(e) => {
                errors.insert("history".into(), JsonValue::String(e.to_string()));
                JsonValue::Null
            }
        };
        let url = session_url(
            services,
            id,
            session_json.get("taskId").and_then(JsonValue::as_str),
        );
        let report = json!({
            "session": session_json,
            "url": url,
            "debug": debug_json,
            "history": history_json,
            "artifacts": artifacts_json,
            "errors": errors,
        });
        writeln!(out, "{report}")?;
        return Ok(());
    }

    let mut failed = Vec::new();
    writeln!(out, "== Session")?;
    match &session {
        Ok(r) => {
            let url = session_url(services, id, r.value.task_id.as_deref());
            write!(out, "{}", describe_session(&r.value, url.as_deref()))?;
        }
        Err(e) => {
            writeln!(out, "Cannot get the session: {e}")?;
            failed.push("session");
        }
    }
    writeln!(out, "\n== Environment")?;
    match &debug {
        Ok(r) => write!(out, "{}", describe_debug(&r.value, &services.orca_stack))?,
        Err(e) => {
            writeln!(out, "Cannot get the debug data: {e}")?;
            failed.push("debug");
        }
    }
    writeln!(out, "\n== Last {DIAG_HISTORY_ITEMS} history items")?;
    match &items {
        Ok(items) if items.is_empty() => writeln!(out, "No items.")?,
        Ok(items) => {
            let mut renderer = HistoryRenderer::new(&mut *out);
            for item in items {
                renderer.render(&item.message);
            }
            renderer.finish();
        }
        Err(e) => {
            writeln!(out, "Cannot get the history: {e}")?;
            failed.push("history");
        }
    }
    writeln!(out, "\n== Artifacts")?;
    match &artifacts {
        Ok(r) if r.value.is_empty() => writeln!(out, "No artifacts.")?,
        Ok(r) => write!(out, "{}", artifacts_table(&r.value))?,
        Err(e) => {
            writeln!(out, "Cannot get the artifacts: {e}")?;
            failed.push("artifacts");
        }
    }
    if !failed.is_empty() {
        writeln!(out, "\nFailed parts: {}.", failed.join(", "))?;
    }
    Ok(())
}

fn session_url(services: &Services, id: &str, task_id: Option<&str>) -> Option<String> {
    services.session_web_url(id, task_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_lines_of_text() {
        assert_eq!(last_lines(b"a\nb\nc\nd\n", 2), b"c\nd\n");
        assert_eq!(last_lines(b"a\nb\nc\nd", 2), b"c\nd\n");
        assert_eq!(last_lines(b"a\nb\n", 5), b"a\nb\n");
        assert_eq!(last_lines(b"a\nb\n", 0), b"");
        assert_eq!(last_lines(b"", 3), b"");
    }

    #[test]
    fn debug_text_has_ssh_hint_and_logs() {
        let debug: SessionDebugData = serde_json::from_value(json!({
            "id": "s1", "devEnvId": "env-1", "externalUrl": "https://x",
            "exposedPorts": [{"localPort": 8080, "publicUrl": "https://p"}],
            "logs": [{"name": "job-0-workspace.log.gz", "downloadUrl": "https://d", "sizeBytes": 2048}]
        }))
        .unwrap();
        let text = describe_debug(&debug, "production");
        assert!(
            text.contains("orca-cli --stack production ssh env-1"),
            "{text}"
        );
        assert!(text.contains("8080 -> https://p"), "{text}");
        assert!(text.contains("job-0-workspace.log.gz  2.0 KiB"), "{text}");
    }

    #[test]
    fn debug_text_skips_undefined_env() {
        let debug: SessionDebugData =
            serde_json::from_value(json!({"id": "s1", "devEnvId": "UNDEFINED"})).unwrap();
        assert!(!describe_debug(&debug, "staging").contains("orca-cli"));
    }
}
