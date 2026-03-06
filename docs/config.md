# Configuration

For basic configuration instructions, see [this documentation](https://developers.openai.com/codex/config-basic).

For advanced configuration instructions, see [this documentation](https://developers.openai.com/codex/config-advanced).

For a full configuration reference, see [this documentation](https://developers.openai.com/codex/config-reference).

## Connecting to MCP servers

Codex can connect to MCP servers configured in `~/.codex/config.toml`. See the configuration reference for the latest MCP server options:

- https://developers.openai.com/codex/config-reference

## LSP integration

Codex includes an experimental built-in LSP integration for definitions, references, hover text, symbols, call hierarchy, and post-`apply_patch` diagnostics.

Enable it with:

```toml
[features]
lsp = true

[lsp]
enabled = true
```

Codex ships a small built-in server catalog for `rust-analyzer`, `typescript-language-server`, `pyright-langserver`, `gopls`, `clangd`, `sourcekit-lsp`, and `intelephense`. A server is only activated when its command is available locally; v1 does not auto-install or auto-download LSP servers.

You can override a built-in server or add a custom one:

```toml
[lsp.servers.rust]
root_markers = ["Cargo.toml", "rust-project.json", ".git"]

[lsp.servers.custom]
command = "custom-lsp-server"
args = ["--stdio"]
extensions = [".custom"]
root_markers = [".git"]
```

Each server entry supports:

- `disabled`
- `command`
- `args`
- `extensions`
- `env`
- `initialization`
- `root_markers`

When enabled, Codex exposes a built-in `lsp` tool with `go_to_definition`, `find_references`, `hover`, `document_symbol`, `workspace_symbol`, `status`, `go_to_implementation`, `prepare_call_hierarchy`, `incoming_calls`, and `outgoing_calls`.

For `workspace_symbol`, you can optionally pass `file_path` to scope the query to the servers that match a specific file type. Without `file_path`, Codex only queries already-active LSP clients for the current workspace.

Codex also appends LSP `ERROR` diagnostics for touched files after successful `apply_patch` runs when a matching server is available.

For PHP projects, the built-in `intelephense` server expects the `intelephense` command to be installed locally. If you use an Intelephense license key, place the file at `$HOME/intelephense/license.txt` on macOS/Linux or `%USERPROFILE%/intelephense/license.txt` on Windows.

## Apps (Connectors)

Use `$` in the composer to insert a ChatGPT connector; the popover lists accessible
apps. The `/apps` command lists available and installed apps. Connected apps appear first
and are labeled as connected; others are marked as can be installed.

## Notify

Codex can run a notification hook when the agent finishes a turn. See the configuration reference for the latest notification settings:

- https://developers.openai.com/codex/config-reference

When Codex knows which client started the turn, the legacy notify JSON payload also includes a top-level `client` field. The TUI reports `codex-tui`, and the app server reports the `clientInfo.name` value from `initialize`.

## JSON Schema

The generated JSON Schema for `config.toml` lives at `codex-rs/core/config.schema.json`.

## SQLite State DB

Codex stores the SQLite-backed state DB under `sqlite_home` (config key) or the
`CODEX_SQLITE_HOME` environment variable. When unset, WorkspaceWrite sandbox
sessions default to a temp directory; other modes default to `CODEX_HOME`.

## Notices

Codex stores "do not show again" flags for some UI prompts under the `[notice]` table.

## Plan mode defaults

`plan_mode_reasoning_effort` lets you set a Plan-mode-specific default reasoning
effort override. When unset, Plan mode uses the built-in Plan preset default
(currently `medium`). When explicitly set (including `none`), it overrides the
Plan preset. The string value `none` means "no reasoning" (an explicit Plan
override), not "inherit the global default". There is currently no separate
config value for "follow the global default in Plan mode".

Ctrl+C/Ctrl+D quitting uses a ~1 second double-press hint (`ctrl + c again to quit`).
