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
$ brew install bazhenov/tap/jcp
```

## Linux

```console
$ curl --proto '=https' --tlsv1.2 -LsSf https://github.com/bazhenov/jcp-cli/releases/latest/download/jcp-installer.sh | sh
```

## Windows

```console
powershell -ExecutionPolicy Bypass -c "irm https://github.com/bazhenov/jcp-cli/releases/latest/download/jcp-installer.ps1 | iex"
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
    "args": ["acp"],
    "env": {
      // You can put AI_PLATFORM_TOKEN here or in .env file
      "AI_PLATFORM_TOKEN": "...",
    }
  }
}
```
