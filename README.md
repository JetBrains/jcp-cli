# Installation

Supported platforms:

| OS      | Arch    |     |
| ------- | ------- | --- |
| macOS   | x64     | ✅  |
| macOS   | aarch64 | ✅  |
| Linux   | x64     | ✅  |
| Linux   | aarch64 | ✅  |
| Windows | x64     | ✅  |
| Windows | aarch64 | ❌  |

## Homebrew (macOS)

```console
$ brew install jetbrains/utils/jcp
```

## Linux

```console
$ curl --proto '=https' --tlsv1.2 -LsSf https://github.com/JetBrains/jcp-cli/releases/latest/download/jcp-installer.sh | sh
```

## Windows

```console
powershell -ExecutionPolicy Bypass -c "irm https://github.com/JetBrains/jcp-cli/releases/latest/download/jcp-installer.ps1 | iex"
```

## Building from sources

At the moment installation requires rust toolchain:

```console
$ git clone https://github.com/JetBrains/jcp-cli
$ cd jcp-cli
$ cargo install --path=.
```

# Configuring

1. do `jcp login`
2. configure your IDE with `jcp acp` as an ACP agent.

## Zed

In `settings.json`:

```json
"agent_servers": {
  "JCP": {
    "type": "custom",
    "command": "jcp",
    "args": ["acp"]
  }
}
```

# Usage

`jcp login` gives access to all commands. Use `jcp <command> --help` to see all options.

## Air Cloud sessions

A `<SESSION>` argument is a session ID or an Air task URL.

```console
$ jcp session start --repo JetBrains/marinator "List the top-level files."   # follow the run until the agent stops
$ jcp session start --detach --json --env "My env" --agent claude "Fix the bug."
$ jcp session list [--search TEXT] [--limit N] [--offset N] [--json]
$ jcp session get <SESSION> [--json]
$ jcp session status <SESSION>
$ jcp session wait <SESSION> [--timeout SECS]
$ jcp session rename <SESSION> <NAME>
$ jcp session resume <SESSION> [--prompt TEXT]
$ jcp session archive <SESSION>
$ jcp session delete <SESSION> [--yes]
$ jcp session artifacts <SESSION> [--json]
$ jcp session download <SESSION> <NAME> [-o FILE]
```

`session start` uses the repository of `--repo`, then the repository of `--env`, then the `origin` remote of the
current directory. The branch is `--branch`, then the current branch (local repository only), then the default branch.
Without `--detach`, the command shows the status changes and the conversation. It exits with code 1 if the session
stops with `ERROR`, `ABORTED` or `CANCELLED`.

## Diagnostics

```console
$ jcp session diag <SESSION> [--json]        # session, Orca environment, last history items, artifacts
$ jcp session watch <SESSION>                # status changes and new conversation items
$ jcp session history <SESSION> [--follow] [--tail N] [--json]
$ jcp session debug <SESSION> [--json]       # Orca environment ID, ports, log files
$ jcp session logs <SESSION> [-o FILE]       # all logs in one ZIP file
$ jcp session logs <SESSION> --list
$ jcp session logs <SESSION> --name LOG [--tail N] [-o FILE]
```

## Environments, repositories and agents

```console
$ jcp env list [--repo URL|owner/name] [--mine | --shared] [--project ID] [--drafts] [--json]
$ jcp env get <ID|NAME> [--json]
$ jcp repo list [--search TEXT] [--limit N] [--json]
$ jcp repo branches <URL|owner/name> [--search TEXT] [--json]
$ jcp repo providers [--json]
$ jcp agents [--json]
```

## Exit codes

| Code | Meaning |
| ---- | ------- |
| 0    | Success. |
| 1    | Error, or the session stopped with `ERROR`, `ABORTED` or `CANCELLED`. |
| 2    | `session delete` has no `--yes` and there is no terminal for the question. |

## Scripts and CI

Set `JCP_ACCESS_TOKEN` to use an access token instead of the login in the keychain.
Set also `JCP_ORG_ID` to show the Air web URLs.
