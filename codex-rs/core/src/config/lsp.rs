use crate::config::ConfigToml;
use crate::config::ManagedFeatures;
use crate::config::types::LspConfig;
use crate::config::types::LspMode;
use crate::config::types::LspRuntimeKind;
use crate::config::types::LspServerConfig;
use crate::config::types::LspServerToml;
use crate::config::types::ManagedNpmLspServerConfig;
use crate::features::Feature;
use serde_json::json;
use std::collections::HashMap;
use std::io;
use std::io::ErrorKind;

pub(super) fn resolve_lsp_config(
    cfg: &ConfigToml,
    features: &ManagedFeatures,
) -> io::Result<Option<LspConfig>> {
    if !features.enabled(Feature::Lsp) {
        return Ok(None);
    }

    let lsp = cfg.lsp.clone().unwrap_or_default();
    if matches!(lsp.enabled, Some(false)) {
        return Ok(None);
    }

    let mut servers = built_in_servers();
    for (id, override_server) in lsp.servers {
        if matches!(override_server.disabled, Some(true)) {
            servers.remove(&id);
            continue;
        }

        let next = match servers.remove(&id) {
            Some(existing) => merge_server(existing, override_server),
            None => custom_server(id.clone(), override_server)?,
        };
        servers.insert(id, next);
    }

    let mut servers = servers.into_values().collect::<Vec<_>>();
    servers.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(Some(LspConfig {
        mode: lsp.mode.unwrap_or(LspMode::Auto),
        assume_yes: lsp.assume_yes.unwrap_or(false),
        servers,
    }))
}

fn merge_server(existing: LspServerConfig, override_server: LspServerToml) -> LspServerConfig {
    LspServerConfig {
        id: existing.id,
        command: override_server.command.unwrap_or(existing.command),
        args: override_server.args.unwrap_or(existing.args),
        extensions: override_server.extensions.unwrap_or(existing.extensions),
        env: override_server.env.unwrap_or(existing.env),
        initialization: override_server.initialization.or(existing.initialization),
        root_markers: override_server
            .root_markers
            .unwrap_or(existing.root_markers),
        runtime_kind: override_server
            .runtime_kind
            .unwrap_or(existing.runtime_kind),
        project_local_candidates: override_server
            .project_local_candidates
            .unwrap_or(existing.project_local_candidates),
        requirements: override_server.requirements.or(existing.requirements),
        managed_npm: override_server
            .managed_npm
            .map(managed_npm)
            .or(existing.managed_npm),
    }
}

fn custom_server(id: String, server: LspServerToml) -> io::Result<LspServerConfig> {
    let command = server.command.ok_or_else(|| {
        io::Error::new(
            ErrorKind::InvalidInput,
            format!("custom LSP server `{id}` requires `command`"),
        )
    })?;
    let extensions = server.extensions.ok_or_else(|| {
        io::Error::new(
            ErrorKind::InvalidInput,
            format!("custom LSP server `{id}` requires `extensions`"),
        )
    })?;
    if extensions.is_empty() {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            format!("custom LSP server `{id}` requires at least one extension"),
        ));
    }

    Ok(LspServerConfig {
        id,
        command,
        args: server.args.unwrap_or_default(),
        extensions,
        env: server.env.unwrap_or_default(),
        initialization: server.initialization,
        root_markers: server.root_markers.unwrap_or_default(),
        runtime_kind: server
            .runtime_kind
            .unwrap_or(LspRuntimeKind::UserConfigured),
        project_local_candidates: server.project_local_candidates.unwrap_or_default(),
        requirements: server.requirements,
        managed_npm: server.managed_npm.map(managed_npm),
    })
}

fn managed_npm(server: crate::config::types::ManagedNpmLspServerToml) -> ManagedNpmLspServerConfig {
    ManagedNpmLspServerConfig {
        package: server.package,
        version: server.version,
        bin: server.bin,
    }
}

fn built_in_servers() -> HashMap<String, LspServerConfig> {
    [
        managed_npm_server(
            "astro",
            "astro-ls",
            vec!["--stdio"],
            vec![".astro"],
            package_roots(),
            "@astrojs/language-server",
            "2.16.3",
            "astro-ls",
            vec!["node_modules/.bin/astro-ls"],
            "Auto-installs the managed Astro language server when needed.",
        ),
        managed_npm_server(
            "bash",
            "bash-language-server",
            vec!["start"],
            vec![".sh", ".bash", ".zsh", ".ksh"],
            Vec::new(),
            "bash-language-server",
            "5.6.0",
            "bash-language-server",
            vec!["node_modules/.bin/bash-language-server"],
            "Auto-installs the managed bash-language-server when needed.",
        ),
        toolchain_server(
            "clangd",
            "clangd",
            vec!["--background-index", "--clang-tidy"],
            vec![
                ".c", ".cc", ".cpp", ".cxx", ".c++", ".h", ".hh", ".hpp", ".hxx", ".h++", ".m",
                ".mm",
            ],
            vec![
                "compile_commands.json",
                "compile_flags.txt",
                ".clangd",
                "CMakeLists.txt",
                "Makefile",
            ],
            "Requires `clangd` from the local LLVM/Clang toolchain.",
        ),
        toolchain_server(
            "clojure-lsp",
            "clojure-lsp",
            vec!["listen"],
            vec![".clj", ".cljs", ".cljc", ".edn"],
            vec!["deps.edn", "project.clj", "shadow-cljs.edn", "bb.edn", "build.boot"],
            "Requires `clojure-lsp` installed locally.",
        ),
        toolchain_server(
            "csharp",
            "csharp-ls",
            Vec::<&str>::new(),
            vec![".cs"],
            vec![".slnx", ".sln", ".csproj", "global.json"],
            "Requires .NET SDK and `csharp-ls` installed locally.",
        ),
        toolchain_server(
            "dart",
            "dart",
            vec!["language-server", "--lsp"],
            vec![".dart"],
            vec!["pubspec.yaml", "analysis_options.yaml"],
            "Requires the `dart` command from a local Dart/Flutter toolchain.",
        ),
        toolchain_server(
            "deno",
            "deno",
            vec!["lsp"],
            vec![".ts", ".tsx", ".js", ".jsx", ".mjs"],
            vec!["deno.json", "deno.jsonc"],
            "Requires the `deno` command and a Deno workspace.",
        ),
        toolchain_server(
            "elixir-ls",
            "elixir-ls",
            Vec::<&str>::new(),
            vec![".ex", ".exs"],
            vec!["mix.exs", "mix.lock"],
            "Requires Elixir tooling and `elixir-ls` installed locally.",
        ),
        project_server(
            "eslint",
            "vscode-eslint-language-server",
            vec!["--stdio"],
            vec![".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".mts", ".cts", ".vue"],
            package_roots(),
            vec!["node_modules/.bin/vscode-eslint-language-server"],
            "Requires an ESLint language server binary in the workspace or on PATH.",
        ),
        toolchain_server(
            "fsharp",
            "fsautocomplete",
            Vec::<&str>::new(),
            vec![".fs", ".fsi", ".fsx", ".fsscript"],
            vec![".slnx", ".sln", ".fsproj", "global.json"],
            "Requires .NET SDK and `fsautocomplete` installed locally.",
        ),
        toolchain_server(
            "gleam",
            "gleam",
            vec!["lsp"],
            vec![".gleam"],
            vec!["gleam.toml"],
            "Requires the `gleam` toolchain installed locally.",
        ),
        toolchain_server(
            "gopls",
            "gopls",
            Vec::<&str>::new(),
            vec![".go"],
            vec!["go.mod", "go.work", "go.sum"],
            "Requires the `go` toolchain and `gopls` installed locally.",
        ),
        toolchain_server(
            "hls",
            "haskell-language-server-wrapper",
            vec!["--lsp"],
            vec![".hs", ".lhs"],
            vec!["stack.yaml", "cabal.project", "hie.yaml"],
            "Requires `haskell-language-server-wrapper` installed locally.",
        ),
        managed_npm_server(
            "intelephense",
            "intelephense",
            vec!["--stdio"],
            vec![".php"],
            vec!["composer.json", "composer.lock", ".php-version", ".git"],
            "intelephense",
            "1.16.5",
            "intelephense",
            vec!["node_modules/.bin/intelephense"],
            "Auto-installs the managed Intelephense package when needed.",
        )
        .with_initialization(json!({
            "telemetry": {
                "enabled": false,
            },
        })),
        toolchain_server(
            "jdtls",
            "jdtls",
            Vec::<&str>::new(),
            vec![".java"],
            vec!["pom.xml", "build.gradle", "build.gradle.kts", ".project", ".classpath"],
            "Requires Java SDK and `jdtls` installed locally.",
        ),
        toolchain_server(
            "julials",
            "julia",
            vec![
                "--startup-file=no",
                "--history-file=no",
                "-e",
                "using LanguageServer; runserver()",
            ],
            vec![".jl"],
            vec!["Project.toml", "Manifest.toml"],
            "Requires the `julia` toolchain installed locally.",
        ),
        toolchain_server(
            "kotlin-ls",
            "kotlin-lsp",
            vec!["--stdio"],
            vec![".kt", ".kts"],
            vec![
                "settings.gradle.kts",
                "settings.gradle",
                "build.gradle.kts",
                "build.gradle",
                "pom.xml",
            ],
            "Requires `kotlin-lsp` installed locally.",
        ),
        toolchain_server(
            "lua-ls",
            "lua-language-server",
            Vec::<&str>::new(),
            vec![".lua"],
            vec![
                ".luarc.json",
                ".luarc.jsonc",
                ".luacheckrc",
                ".stylua.toml",
                "stylua.toml",
                "selene.toml",
                "selene.yml",
            ],
            "Requires `lua-language-server` installed locally.",
        ),
        toolchain_server(
            "nixd",
            "nixd",
            Vec::<&str>::new(),
            vec![".nix"],
            vec!["flake.nix", "default.nix", "shell.nix"],
            "Requires `nixd` installed locally.",
        ),
        toolchain_server(
            "ocaml-lsp",
            "ocamllsp",
            Vec::<&str>::new(),
            vec![".ml", ".mli"],
            vec!["dune-project", "dune-workspace", ".merlin", "opam"],
            "Requires `ocamllsp` installed locally.",
        ),
        toolchain_server(
            "prisma",
            "prisma",
            vec!["language-server"],
            vec![".prisma"],
            vec!["schema.prisma"],
            "Requires the `prisma` CLI installed locally.",
        ),
        managed_npm_server(
            "pyright",
            "pyright-langserver",
            vec!["--stdio"],
            vec![".py", ".pyi"],
            vec![
                "pyproject.toml",
                "setup.py",
                "setup.cfg",
                "requirements.txt",
                "Pipfile",
                "pyrightconfig.json",
            ],
            "pyright",
            "1.1.408",
            "pyright-langserver",
            vec!["node_modules/.bin/pyright-langserver"],
            "Uses workspace `pyright` when present, otherwise installs a managed copy.",
        ),
        toolchain_server(
            "ruby-lsp",
            "rubocop",
            vec!["--lsp"],
            vec![".rb", ".rake", ".gemspec", ".ru"],
            vec!["Gemfile"],
            "Requires Ruby tooling and `rubocop --lsp` support locally.",
        ),
        toolchain_server(
            "rust",
            "rust-analyzer",
            Vec::<&str>::new(),
            vec![".rs"],
            vec!["Cargo.toml", "rust-project.json"],
            "Requires `rust-analyzer` installed locally.",
        ),
        toolchain_server(
            "sourcekit",
            "sourcekit-lsp",
            Vec::<&str>::new(),
            vec![".swift", ".m", ".mm", ".objc", ".objcpp"],
            vec!["Package.swift", ".xcodeproj", ".xcworkspace"],
            "Requires the local Swift/Xcode toolchain with `sourcekit-lsp`.",
        ),
        managed_npm_server(
            "svelte",
            "svelteserver",
            vec!["--stdio"],
            vec![".svelte"],
            package_roots(),
            "svelte-language-server",
            "0.17.29",
            "svelteserver",
            vec!["node_modules/.bin/svelteserver"],
            "Auto-installs the managed Svelte language server when needed.",
        ),
        toolchain_server(
            "terraform",
            "terraform-ls",
            vec!["serve"],
            vec![".tf", ".tfvars"],
            vec![".terraform.lock.hcl", "terraform.tfstate"],
            "Requires `terraform-ls` installed locally.",
        )
        .with_initialization(json!({
            "experimentalFeatures": {
                "prefillRequiredFields": true,
                "validateOnSave": true,
            },
        })),
        toolchain_server(
            "tinymist",
            "tinymist",
            Vec::<&str>::new(),
            vec![".typ", ".typc"],
            vec!["typst.toml"],
            "Requires `tinymist` installed locally.",
        ),
        managed_npm_server(
            "typescript",
            "typescript-language-server",
            vec!["--stdio"],
            vec![".ts", ".tsx", ".js", ".jsx", ".mts", ".cts", ".mjs", ".cjs"],
            package_roots(),
            "typescript-language-server",
            "5.1.3",
            "typescript-language-server",
            vec!["node_modules/.bin/typescript-language-server"],
            "Uses workspace `typescript-language-server` when present, otherwise installs a managed copy.",
        ),
        managed_npm_server(
            "vue",
            "vue-language-server",
            vec!["--stdio"],
            vec![".vue"],
            package_roots(),
            "@vue/language-server",
            "3.2.5",
            "vue-language-server",
            vec!["node_modules/.bin/vue-language-server"],
            "Auto-installs the managed Vue language server when needed.",
        ),
        managed_npm_server(
            "yaml-ls",
            "yaml-language-server",
            vec!["--stdio"],
            vec![".yaml", ".yml"],
            Vec::<&str>::new(),
            "yaml-language-server",
            "1.21.0",
            "yaml-language-server",
            vec!["node_modules/.bin/yaml-language-server"],
            "Auto-installs the managed YAML language server when needed.",
        ),
        toolchain_server(
            "zls",
            "zls",
            Vec::<&str>::new(),
            vec![".zig", ".zon"],
            vec!["build.zig"],
            "Requires `zls` installed locally.",
        ),
    ]
    .into_iter()
    .map(|server| (server.id.clone(), server))
    .collect()
}

fn toolchain_server(
    id: &str,
    command: &str,
    args: Vec<&str>,
    extensions: Vec<&str>,
    root_markers: Vec<&str>,
    requirements: &str,
) -> LspServerConfig {
    LspServerConfig {
        id: id.to_string(),
        command: command.to_string(),
        args: strings(args),
        extensions: strings(extensions),
        env: HashMap::new(),
        initialization: None,
        root_markers: strings(root_markers),
        runtime_kind: LspRuntimeKind::ToolchainProvided,
        project_local_candidates: Vec::new(),
        requirements: Some(requirements.to_string()),
        managed_npm: None,
    }
}

fn project_server(
    id: &str,
    command: &str,
    args: Vec<&str>,
    extensions: Vec<&str>,
    root_markers: Vec<&str>,
    project_local_candidates: Vec<&str>,
    requirements: &str,
) -> LspServerConfig {
    LspServerConfig {
        id: id.to_string(),
        command: command.to_string(),
        args: strings(args),
        extensions: strings(extensions),
        env: HashMap::new(),
        initialization: None,
        root_markers: strings(root_markers),
        runtime_kind: LspRuntimeKind::ProjectDependency,
        project_local_candidates: strings(project_local_candidates),
        requirements: Some(requirements.to_string()),
        managed_npm: None,
    }
}

fn managed_npm_server(
    id: &str,
    command: &str,
    args: Vec<&str>,
    extensions: Vec<&str>,
    root_markers: Vec<&str>,
    package: &str,
    version: &str,
    bin: &str,
    project_local_candidates: Vec<&str>,
    requirements: &str,
) -> LspServerConfig {
    LspServerConfig {
        id: id.to_string(),
        command: command.to_string(),
        args: strings(args),
        extensions: strings(extensions),
        env: HashMap::new(),
        initialization: None,
        root_markers: strings(root_markers),
        runtime_kind: LspRuntimeKind::ManagedNpm,
        project_local_candidates: strings(project_local_candidates),
        requirements: Some(requirements.to_string()),
        managed_npm: Some(ManagedNpmLspServerConfig {
            package: package.to_string(),
            version: version.to_string(),
            bin: bin.to_string(),
        }),
    }
}

fn package_roots() -> Vec<&'static str> {
    vec![
        "package.json",
        "package-lock.json",
        "pnpm-lock.yaml",
        "yarn.lock",
        "bun.lock",
        "bun.lockb",
    ]
}

fn strings(values: Vec<&str>) -> Vec<String> {
    values.into_iter().map(str::to_string).collect()
}

trait LspServerConfigExt {
    fn with_initialization(self, initialization: serde_json::Value) -> Self;
}

impl LspServerConfigExt for LspServerConfig {
    fn with_initialization(mut self, initialization: serde_json::Value) -> Self {
        self.initialization = Some(initialization);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ConfigToml;
    use crate::config::types::LspToml;
    use crate::config::types::ManagedNpmLspServerToml;
    use crate::features::Features;

    #[test]
    fn resolve_lsp_config_supports_builtin_overrides_and_custom_servers() {
        let cfg = ConfigToml {
            lsp: Some(LspToml {
                enabled: Some(true),
                mode: Some(LspMode::On),
                assume_yes: Some(true),
                servers: HashMap::from([
                    (
                        "rust".to_string(),
                        LspServerToml {
                            env: Some(HashMap::from([(
                                "RUST_LOG".to_string(),
                                "debug".to_string(),
                            )])),
                            ..Default::default()
                        },
                    ),
                    (
                        "custom".to_string(),
                        LspServerToml {
                            command: Some("custom-lsp".to_string()),
                            extensions: Some(vec![".custom".to_string()]),
                            managed_npm: Some(ManagedNpmLspServerToml {
                                package: "custom-pkg".to_string(),
                                version: "1.0.0".to_string(),
                                bin: "custom-lsp".to_string(),
                            }),
                            runtime_kind: Some(LspRuntimeKind::ManagedNpm),
                            ..Default::default()
                        },
                    ),
                ]),
            }),
            ..Default::default()
        };
        let mut features = Features::with_defaults();
        features.enable(Feature::Lsp);
        let features = ManagedFeatures::from_configured(features, None).expect("features");

        let config = resolve_lsp_config(&cfg, &features)
            .expect("resolve LSP config")
            .expect("enabled");
        assert_eq!(config.mode, LspMode::On);
        assert!(config.assume_yes);
        assert!(config.servers.iter().any(|server| server.id == "custom"));
        let rust = config
            .servers
            .iter()
            .find(|server| server.id == "rust")
            .expect("rust server");
        assert_eq!(rust.env.get("RUST_LOG"), Some(&"debug".to_string()));
        let custom = config
            .servers
            .iter()
            .find(|server| server.id == "custom")
            .expect("custom server");
        assert_eq!(custom.runtime_kind, LspRuntimeKind::ManagedNpm);
        assert_eq!(
            custom.managed_npm,
            Some(ManagedNpmLspServerConfig {
                package: "custom-pkg".to_string(),
                version: "1.0.0".to_string(),
                bin: "custom-lsp".to_string(),
            })
        );
    }
}
