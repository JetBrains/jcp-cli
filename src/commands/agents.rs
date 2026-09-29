//! `jcp agents` and the check of `--agent`, `--model`, `--mode` and `--reasoning`.

use super::{CliError, usage};
use crate::api::spawner::{AgentInfo, ModelInfo, SessionConfigOptions, SpawnerApi};
use std::io::Write;

pub fn list(api: &SpawnerApi, json: bool, out: &mut impl Write) -> Result<(), CliError> {
    let response = api.agents()?;
    if json {
        writeln!(out, "{}", response.raw)?;
        return Ok(());
    }
    write!(out, "{}", describe(&response.value.agents))?;
    Ok(())
}

/// Makes the `agents` text. `*` marks the default values.
pub fn describe(agents: &[AgentInfo]) -> String {
    let mut s = String::new();
    for agent in agents {
        let mut flags = Vec::new();
        if agent.is_default == Some(true) {
            flags.push("default");
        }
        if !agent.enabled {
            flags.push("disabled");
        }
        let flags = if flags.is_empty() {
            String::new()
        } else {
            format!(" [{}]", flags.join(", "))
        };
        s.push_str(&format!("{} ({}){flags}\n", agent.id, agent.name));
        for model in &agent.models {
            let default = if model.is_default { "*" } else { "" };
            let levels = reasoning_levels(model);
            let levels = if levels.is_empty() {
                String::new()
            } else {
                format!("  reasoning: {}", levels.join(", "))
            };
            s.push_str(&format!(
                "  model {}{default} ({}){levels}\n",
                model.id, model.name
            ));
        }
        for mode in &agent.permission_modes {
            let default = if mode.is_default == Some(true) {
                "*"
            } else {
                ""
            };
            s.push_str(&format!("  mode  {}{default} ({})\n", mode.id, mode.name));
        }
    }
    s
}

/// Reasoning levels without the `agent:` prefix
fn reasoning_levels(model: &ModelInfo) -> Vec<&str> {
    model
        .thinking_levels
        .iter()
        .map(|l| l.split_once(':').map(|(_, level)| level).unwrap_or(l))
        .collect()
}

/// The agent and the session options for `POST /tasks`
#[derive(Debug, PartialEq)]
pub struct AgentChoice {
    pub agent_id: String,
    pub options: SessionConfigOptions,
}

/// Checks the values against `GET /agents`. An unknown value gives an error with the valid values.
pub fn choose(
    agents: &[AgentInfo],
    agent: Option<&str>,
    model: Option<&str>,
    mode: Option<&str>,
    reasoning: Option<&str>,
) -> Result<AgentChoice, CliError> {
    let enabled: Vec<&AgentInfo> = agents.iter().filter(|a| a.enabled).collect();
    let valid_agents = || join(enabled.iter().map(|a| a.id.as_str()));
    let agent = match agent {
        Some(id) => *enabled
            .iter()
            .find(|a| a.id == id || a.name.eq_ignore_ascii_case(id))
            .ok_or_else(|| {
                usage(format!(
                    "Unknown agent `{id}`. Valid agents: {}.",
                    valid_agents()
                ))
            })?,
        None => *enabled
            .iter()
            .find(|a| a.is_default == Some(true))
            .or_else(|| enabled.first())
            .ok_or_else(|| usage("No agent is available. Run `jcp agents`."))?,
    };

    let model_info = match model {
        Some(id) => Some(agent.models.iter().find(|m| m.id == id).ok_or_else(|| {
            usage(format!(
                "Unknown model `{id}` for agent `{}`. Valid models: {}.",
                agent.id,
                join(agent.models.iter().map(|m| m.id.as_str()))
            ))
        })?),
        None => agent.models.iter().find(|m| m.is_default),
    };

    if let Some(id) = mode
        && !agent.permission_modes.iter().any(|m| m.id == id)
    {
        return Err(usage(format!(
            "Unknown mode `{id}` for agent `{}`. Valid modes: {}.",
            agent.id,
            join(agent.permission_modes.iter().map(|m| m.id.as_str()))
        )));
    }

    let reasoning = match (reasoning, model_info) {
        (Some(level), Some(model_info)) => {
            let level = level.split_once(':').map(|(_, l)| l).unwrap_or(level);
            let levels = reasoning_levels(model_info);
            if !levels.contains(&level) {
                return Err(usage(format!(
                    "Unknown reasoning level `{level}` for model `{}`. Valid levels: {}.",
                    model_info.id,
                    join(levels.into_iter())
                )));
            }
            Some(level.to_string())
        }
        (Some(level), None) => Some(level.to_string()),
        (None, _) => None,
    };

    Ok(AgentChoice {
        agent_id: agent.id.clone(),
        options: SessionConfigOptions {
            model: model.map(str::to_string),
            mode: mode.map(str::to_string),
            reasoning_effort: reasoning,
        },
    })
}

fn join<'a>(values: impl Iterator<Item = &'a str>) -> String {
    let values: Vec<&str> = values.collect();
    if values.is_empty() {
        "none".to_string()
    } else {
        values.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn agents() -> Vec<AgentInfo> {
        serde_json::from_value(json!([
            {"id": "codex", "name": "Codex", "enabled": true, "models": [{"id": "gpt", "name": "GPT", "isDefault": true}]},
            {"id": "claude", "name": "Claude", "isDefault": true, "enabled": true,
             "models": [
                {"id": "opus", "name": "Opus", "isDefault": true, "thinkingLevels": ["claude:low", "claude:high"]},
                {"id": "sonnet", "name": "Sonnet", "isDefault": false}
             ],
             "permissionModes": [{"id": "default", "name": "Default", "description": "", "isDefault": true},
                                 {"id": "plan", "name": "Plan", "description": ""}]},
            {"id": "off", "name": "Off", "enabled": false, "models": []}
        ]))
        .unwrap()
    }

    #[test]
    fn default_agent_has_no_options() {
        let choice = choose(&agents(), None, None, None, None).unwrap();
        assert_eq!(choice.agent_id, "claude");
        assert_eq!(choice.options, SessionConfigOptions::default());
    }

    #[test]
    fn valid_values() {
        let choice = choose(
            &agents(),
            Some("claude"),
            Some("opus"),
            Some("plan"),
            Some("high"),
        )
        .unwrap();
        assert_eq!(
            choice.options,
            SessionConfigOptions {
                model: Some("opus".into()),
                mode: Some("plan".into()),
                reasoning_effort: Some("high".into())
            }
        );
    }

    #[test]
    fn reasoning_uses_default_model_and_accepts_prefix() {
        let choice = choose(&agents(), None, None, None, Some("claude:low")).unwrap();
        assert_eq!(choice.options.reasoning_effort.as_deref(), Some("low"));
    }

    #[test]
    fn unknown_values_list_valid_values() {
        let e = choose(&agents(), Some("off"), None, None, None)
            .unwrap_err()
            .to_string();
        assert!(e.contains("Valid agents: codex, claude."), "{e}");
        let e = choose(&agents(), Some("claude"), Some("x"), None, None)
            .unwrap_err()
            .to_string();
        assert!(e.contains("Valid models: opus, sonnet."), "{e}");
        let e = choose(&agents(), Some("claude"), None, Some("x"), None)
            .unwrap_err()
            .to_string();
        assert!(e.contains("Valid modes: default, plan."), "{e}");
        let e = choose(
            &agents(),
            Some("claude"),
            Some("sonnet"),
            None,
            Some("high"),
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("Valid levels: none."), "{e}");
    }

    #[test]
    fn describe_marks_defaults() {
        let out = describe(&agents());
        assert!(out.contains("claude (Claude) [default]"), "{out}");
        assert!(
            out.contains("  model opus* (Opus)  reasoning: low, high"),
            "{out}"
        );
        assert!(out.contains("  mode  default* (Default)"), "{out}");
        assert!(out.contains("off (Off) [disabled]"), "{out}");
    }
}
