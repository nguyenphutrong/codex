# Implement Core-Native LSP Integration

## Summary

- Add a native LSP subsystem inside `codex-rs` and expose a built-in `lsp` tool from `core`.
- Keep v1 experimental behind `features.lsp`, default `false`.
- Support a small built-in server catalog plus user-defined overrides/custom servers in `config.toml`.
- Enrich structured edit output with LSP diagnostics after file changes, matching the OpenCode-style feedback loop.
- Do not add new app-server RPCs or use `dynamicTools` for this feature.

## Key Changes

### 1. Add a reusable Rust LSP runtime
- Create a new workspace crate, `codex-lsp`, responsible for:
  - spawning stdio language servers
  - JSON-RPC request/response framing
  - initialization and shutdown
  - workspace root resolution
  - opened-document tracking with monotonically increasing versions
  - diagnostics cache per file
- Runtime surface should be synchronous from `core`’s perspective via async methods:
  - `has_server_for_file(path)`
  - `touch_file(path, wait_for_diagnostics)`
  - `diagnostics_for_paths(paths)`
  - `status_for_file(path)`
  - `hover/definition/references/implementation/document_symbol/workspace_symbol/call_hierarchy`
- Use one client per `(server_id, workspace_root)` and reuse across calls in a session.
- Normalize all returned paths to absolute filesystem paths and convert positions/ranges to 1-based coordinates before returning to tool callers.
- Handle server failures defensively:
  - failed spawn/init marks that `(server_id, workspace_root)` as broken for the session
  - broken servers do not fail unrelated tool calls unless the user explicitly invoked `lsp`
  - edit flows never fail solely because LSP is unavailable

### 2. Add config and feature gating
- Add `Feature::Lsp` in `codex-rs/core/src/features.rs`, stage `Experimental`, default `false`.
- Extend `ConfigToml` with a new optional `lsp` section:
  - `enabled: Option<bool>`
  - `servers: HashMap<String, LspServerToml>`
- Add `LspServerToml` and resolved runtime config types in `core` config types.
- `LspServerToml` fields:
  - `disabled: Option<bool>`
  - `command: Option<String>`
  - `args: Option<Vec<String>>`
  - `extensions: Option<Vec<String>>`
  - `env: Option<HashMap<String, String>>`
  - `initialization: Option<serde_json::Value>`
  - `root_markers: Option<Vec<String>>`
- Built-in catalog is always available when feature enabled; user config can:
  - disable a built-in server
  - override its command/extensions/env/init/root markers
  - add a custom server id
- Built-in catalog for v1:
  - `rust` => `rust-analyzer`, `.rs`, markers `Cargo.toml`, `rust-project.json`
  - `typescript` => `typescript-language-server --stdio`, `.ts/.tsx/.js/.jsx/.mts/.cts/.mjs/.cjs`, markers lockfiles and `package.json`
  - `pyright` => `pyright-langserver --stdio`, `.py/.pyi`, markers `pyproject.toml`, `setup.py`, `requirements.txt`
  - `gopls` => `gopls`, `.go`, markers `go.mod`, `go.work`
  - `clangd` => `clangd`, C/C++ extensions, markers `compile_commands.json`, `.clangd`
  - `sourcekit` => `sourcekit-lsp`, `.swift/.objc/.objcpp`, markers `Package.swift`, `.xcodeproj`, `.xcworkspace`
- Only activate a server if its command resolves successfully; v1 does not auto-install or auto-download.
- Because `ConfigToml` changes, run `just write-config-schema` as part of implementation and include generated `codex-rs/core/config.schema.json`.

### 3. Expose built-in `lsp` tool from `core`
- Add a new tool spec and handler in `core` tool registry.
- Tool name: `lsp`
- Tool operations:
  - `go_to_definition`
  - `find_references`
  - `hover`
  - `document_symbol`
  - `workspace_symbol`
  - `go_to_implementation`
  - `prepare_call_hierarchy`
  - `incoming_calls`
  - `outgoing_calls`
- Tool request schema:
  - `operation: string` required
  - `file_path: string` required for all file-scoped ops
  - `line: integer` required for position-based ops
  - `character: integer` required for position-based ops
  - `query: string` required only for `workspace_symbol`
- Validation rules:
  - reject missing required fields per operation
  - reject non-positive line/character
  - reject file operations when file does not exist
  - allow relative `file_path`, resolve against current turn cwd
- Tool output shape is JSON text optimized for model use, not raw LSP protocol:
  - `server`
  - `workspace_root`
  - `operation`
  - `items`
- Each item should be normalized into a small stable schema:
  - locations: `path`, `range.start.line`, `range.start.character`, `range.end.line`, `range.end.character`
  - symbols: `name`, `kind`, `detail`, optional `container_name`, optional `location`
  - hover: `contents`, optional `range`
  - call hierarchy: `name`, `kind`, `path`, `selection_range`, `from_ranges` or `to_ranges` as applicable
- If no matching server exists, return a clear tool error: `No LSP server available for this file type.`
- Mark `lsp` as non-parallel-safe unless the runtime implementation guarantees safe concurrent access per client; v1 should default to non-parallel-safe.

### 4. Integrate diagnostics into edit flows
- Introduce an LSP session/service handle into `core` session services so tool handlers can access it without rebuilding state each call.
- On structured edits, call `touch_file(..., wait_for_diagnostics = true)` after writes complete.
- Start with `apply_patch` integration only for v1; do not attempt shell/unified-exec change detection.
- For `apply_patch`, collect touched file paths from the patch result and request diagnostics only for those files.
- Append diagnostics to tool output only for severity `ERROR`.
- Output format should be deterministic and compact, e.g.:
  - one diagnostics block per file
  - maximum 20 errors per file
  - maximum 5 files
  - include line/column and message
- If there are no LSP errors, do not add diagnostics text.
- If LSP times out or a server crashes, skip enrichment and preserve normal patch success.
- Optional optimization: when `read_file` is available, warm matching files into LSP on successful reads without waiting for diagnostics.

### 5. Documentation updates
- Update Rust docs/config docs to cover:
  - `features.lsp`
  - `[lsp]`
  - built-in catalog behavior
  - custom server examples
  - `lsp` tool behavior and supported operations
  - diagnostics enrichment after `apply_patch`
- Keep examples minimal and aligned with actual config keys.
- If any app-server docs mention available built-in capabilities generically, do not add new protocol APIs there.

## Public Interfaces

- New feature flag:
  - `features.lsp`
- New config:
  - `[lsp]`
  - `[lsp.servers.<id>]`
- New built-in tool:
  - `lsp`
- No changes to app-server protocol or dynamic tool APIs.

## Test Plan

- `codex-lsp` unit/integration tests with a fake stdio LSP server:
  - initialize handshake
  - `workspace/workspaceFolders`
  - diagnostics publish and caching
  - file open/change versioning
  - hover/definition/references/document symbols/workspace symbols/call hierarchy
  - timeout, malformed payload, and server crash handling
- `core` config tests:
  - deserialize/serialize `[lsp]`
  - built-in override behavior
  - custom server config
  - feature gating
- `core` tool spec/registry tests:
  - `lsp` hidden when feature disabled
  - `lsp` present when feature enabled
  - input validation by operation
- `apply_patch` tests:
  - appends diagnostics block for touched files with errors
  - omits diagnostics when no errors
  - does not fail patch when LSP unavailable
- If config schema changes, regenerate and verify schema artifacts.

## Validation Commands

- `just fmt` in `codex-rs`
- `cargo test -p codex-lsp` if created as a dedicated crate
- `cargo test -p codex-core`
- Ask before running full `cargo test`, because this change touches shared `core`
- If Clippy cleanup is needed for the new crate and `core`, run `just fix -p codex-lsp` and `just fix -p codex-core` before finalizing

## Assumptions And Defaults

- v1 is experimental and off by default.
- v1 does not auto-download servers.
- v1 enriches diagnostics only for `apply_patch`, not arbitrary shell edits.
- v1 returns normalized, model-friendly JSON rather than raw LSP wire payloads.
- Workspace root detection uses nearest matching marker from `root_markers`; if none are found, it falls back to the turn cwd/workspace root.
