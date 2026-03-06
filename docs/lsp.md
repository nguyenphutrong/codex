# LSP integration

Codex ships an experimental built-in LSP integration for:

- `go_to_definition`
- `find_references`
- `hover`
- `document_symbol`
- `workspace_symbol`
- `go_to_implementation`
- `prepare_call_hierarchy`
- `incoming_calls`
- `outgoing_calls`
- `status`
- post-edit diagnostics for touched files

## Enable LSP

Enable the feature flag and the runtime section together:

```toml
[features]
lsp = true

[lsp]
enabled = true
```

When enabled, Codex starts matching language servers on demand. A built-in server is only used when its command is already installed locally.

## Built-in servers

The current catalog contains 32 built-in server definitions:

| Server id | Command | Extensions |
| --- | --- | --- |
| `astro` | `astro-ls` | `.astro` |
| `bash` | `bash-language-server` | `.sh`, `.bash`, `.zsh`, `.ksh` |
| `clangd` | `clangd` | `.c`, `.cc`, `.cpp`, `.cxx`, `.c++`, `.h`, `.hh`, `.hpp`, `.hxx`, `.h++`, `.m`, `.mm` |
| `clojure-lsp` | `clojure-lsp` | `.clj`, `.cljs`, `.cljc`, `.edn` |
| `csharp` | `csharp-ls` | `.cs` |
| `dart` | `dart` | `.dart` |
| `deno` | `deno` | `.ts`, `.tsx`, `.js`, `.jsx`, `.mjs` |
| `elixir-ls` | `elixir-ls` | `.ex`, `.exs` |
| `eslint` | `vscode-eslint-language-server` | `.ts`, `.tsx`, `.js`, `.jsx`, `.mjs`, `.cjs`, `.mts`, `.cts`, `.vue` |
| `fsharp` | `fsautocomplete` | `.fs`, `.fsi`, `.fsx`, `.fsscript` |
| `gleam` | `gleam` | `.gleam` |
| `gopls` | `gopls` | `.go` |
| `hls` | `haskell-language-server-wrapper` | `.hs`, `.lhs` |
| `intelephense` | `intelephense` | `.php` |
| `jdtls` | `jdtls` | `.java` |
| `julials` | `julia` | `.jl` |
| `kotlin-ls` | `kotlin-lsp` | `.kt`, `.kts` |
| `lua-ls` | `lua-language-server` | `.lua` |
| `nixd` | `nixd` | `.nix` |
| `ocaml-lsp` | `ocamllsp` | `.ml`, `.mli` |
| `prisma` | `prisma` | `.prisma` |
| `pyright` | `pyright-langserver` | `.py`, `.pyi` |
| `ruby-lsp` | `rubocop` | `.rb`, `.rake`, `.gemspec`, `.ru` |
| `rust` | `rust-analyzer` | `.rs` |
| `sourcekit` | `sourcekit-lsp` | `.swift`, `.m`, `.mm`, `.objc`, `.objcpp` |
| `svelte` | `svelteserver` | `.svelte` |
| `terraform` | `terraform-ls` | `.tf`, `.tfvars` |
| `tinymist` | `tinymist` | `.typ`, `.typc` |
| `typescript` | `typescript-language-server` | `.ts`, `.tsx`, `.js`, `.jsx`, `.mts`, `.cts`, `.mjs`, `.cjs` |
| `vue` | `vue-language-server` | `.vue` |
| `yaml-ls` | `yaml-language-server` | `.yaml`, `.yml` |
| `zls` | `zls` | `.zig`, `.zon` |

## Override a built-in server

Override only the fields you need. Any omitted field falls back to the built-in definition.

```toml
[lsp.servers.rust]
command = "rust-analyzer"
root_markers = ["Cargo.toml", "rust-project.json", ".git"]

[lsp.servers.typescript]
env = { TSSERVER_LOG_FILE = "/tmp/tsserver.log" }
```

Each server entry supports:

- `disabled`
- `command`
- `args`
- `extensions`
- `env`
- `initialization`
- `root_markers`

`root_markers` are walked upward from the file directory until Codex reaches the current workspace directory or the filesystem root.

## Add a custom server

Custom servers must provide `command` and at least one file extension:

```toml
[lsp.servers.mydsl]
command = "mydsl-lsp"
args = ["--stdio"]
extensions = [".dsl", ".dsli"]
root_markers = [".git", "mydsl.toml"]
initialization = { telemetry = { enabled = false } }
```

## Disable LSP

Disable all LSP integration:

```toml
[lsp]
enabled = false
```

Disable one built-in server while keeping the rest:

```toml
[lsp.servers.eslint]
disabled = true
```

## Example config

```toml
[features]
lsp = true

[lsp]
enabled = true

[lsp.servers.rust]
root_markers = ["Cargo.toml", "rust-project.json", ".git"]

[lsp.servers.typescript]
root_markers = ["package.json", "pnpm-lock.yaml", "yarn.lock", ".git"]

[lsp.servers.intelephense]
disabled = true

[lsp.servers.mydsl]
command = "mydsl-lsp"
args = ["--stdio"]
extensions = [".dsl"]
root_markers = [".git", "mydsl.toml"]
```

## Troubleshooting

Server does not start:

- Confirm `[features].lsp = true` and `[lsp].enabled = true`.
- Make sure the configured `command` is installed and on `PATH`.
- Check that the file extension matches the server entry.
- Check that the workspace contains at least one expected `root_markers` file when the server depends on project roots.

Diagnostics do not appear:

- Codex only queries servers whose commands are available locally.
- Diagnostics are best-effort and only shown for files touched during the turn.
- `didSave` is only sent for documents Codex has already opened with that LSP client.
- If a server advertises no diagnostics support, Codex can still use other LSP features but will not expect publish-diagnostics traffic from it.

PHP + `intelephense`:

- If you use an Intelephense license key, place it at `$HOME/intelephense/license.txt` on macOS/Linux or `%USERPROFILE%/intelephense/license.txt` on Windows.
