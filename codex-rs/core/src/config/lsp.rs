use crate::config::ConfigToml;
use crate::config::ManagedFeatures;
use crate::config::types::LspConfig;
use crate::config::types::LspServerConfig;
use crate::config::types::LspServerToml;
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
    servers.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(Some(LspConfig { servers }))
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
    })
}

fn built_in_servers() -> HashMap<String, LspServerConfig> {
    [
        (
            "astro",
            LspServerConfig {
                id: "astro".to_string(),
                command: "astro-ls".to_string(),
                args: vec!["--stdio".to_string()],
                extensions: [".astro"].into_iter().map(str::to_string).collect(),
                env: HashMap::new(),
                initialization: None,
                root_markers: [
                    "package.json",
                    "package-lock.json",
                    "pnpm-lock.yaml",
                    "yarn.lock",
                    "bun.lock",
                    "bun.lockb",
                ]
                .into_iter()
                .map(str::to_string)
                .collect(),
            },
        ),
        (
            "bash",
            LspServerConfig {
                id: "bash".to_string(),
                command: "bash-language-server".to_string(),
                args: vec!["start".to_string()],
                extensions: [".sh", ".bash", ".zsh", ".ksh"]
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
                env: HashMap::new(),
                initialization: None,
                root_markers: Vec::new(),
            },
        ),
        (
            "clangd",
            LspServerConfig {
                id: "clangd".to_string(),
                command: "clangd".to_string(),
                args: vec!["--background-index".to_string(), "--clang-tidy".to_string()],
                extensions: [
                    ".c", ".cc", ".cpp", ".cxx", ".c++", ".h", ".hh", ".hpp", ".hxx", ".h++", ".m",
                    ".mm",
                ]
                .into_iter()
                .map(str::to_string)
                .collect(),
                env: HashMap::new(),
                initialization: None,
                root_markers: [
                    "compile_commands.json",
                    "compile_flags.txt",
                    ".clangd",
                    "CMakeLists.txt",
                    "Makefile",
                ]
                .into_iter()
                .map(str::to_string)
                .collect(),
            },
        ),
        (
            "clojure-lsp",
            LspServerConfig {
                id: "clojure-lsp".to_string(),
                command: "clojure-lsp".to_string(),
                args: vec!["listen".to_string()],
                extensions: [".clj", ".cljs", ".cljc", ".edn"]
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
                env: HashMap::new(),
                initialization: None,
                root_markers: [
                    "deps.edn",
                    "project.clj",
                    "shadow-cljs.edn",
                    "bb.edn",
                    "build.boot",
                ]
                .into_iter()
                .map(str::to_string)
                .collect(),
            },
        ),
        (
            "csharp",
            LspServerConfig {
                id: "csharp".to_string(),
                command: "csharp-ls".to_string(),
                args: Vec::new(),
                extensions: [".cs"].into_iter().map(str::to_string).collect(),
                env: HashMap::new(),
                initialization: None,
                root_markers: [".slnx", ".sln", ".csproj", "global.json"]
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
            },
        ),
        (
            "dart",
            LspServerConfig {
                id: "dart".to_string(),
                command: "dart".to_string(),
                args: vec!["language-server".to_string(), "--lsp".to_string()],
                extensions: [".dart"].into_iter().map(str::to_string).collect(),
                env: HashMap::new(),
                initialization: None,
                root_markers: ["pubspec.yaml", "analysis_options.yaml"]
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
            },
        ),
        (
            "deno",
            LspServerConfig {
                id: "deno".to_string(),
                command: "deno".to_string(),
                args: vec!["lsp".to_string()],
                extensions: [".ts", ".tsx", ".js", ".jsx", ".mjs"]
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
                env: HashMap::new(),
                initialization: None,
                root_markers: ["deno.json", "deno.jsonc"]
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
            },
        ),
        (
            "elixir-ls",
            LspServerConfig {
                id: "elixir-ls".to_string(),
                command: "elixir-ls".to_string(),
                args: Vec::new(),
                extensions: [".ex", ".exs"].into_iter().map(str::to_string).collect(),
                env: HashMap::new(),
                initialization: None,
                root_markers: ["mix.exs", "mix.lock"]
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
            },
        ),
        (
            "eslint",
            LspServerConfig {
                id: "eslint".to_string(),
                command: "vscode-eslint-language-server".to_string(),
                args: vec!["--stdio".to_string()],
                extensions: [
                    ".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".mts", ".cts", ".vue",
                ]
                .into_iter()
                .map(str::to_string)
                .collect(),
                env: HashMap::new(),
                initialization: None,
                root_markers: [
                    "package.json",
                    "package-lock.json",
                    "pnpm-lock.yaml",
                    "yarn.lock",
                    "bun.lock",
                    "bun.lockb",
                ]
                .into_iter()
                .map(str::to_string)
                .collect(),
            },
        ),
        (
            "fsharp",
            LspServerConfig {
                id: "fsharp".to_string(),
                command: "fsautocomplete".to_string(),
                args: Vec::new(),
                extensions: [".fs", ".fsi", ".fsx", ".fsscript"]
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
                env: HashMap::new(),
                initialization: None,
                root_markers: [".slnx", ".sln", ".fsproj", "global.json"]
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
            },
        ),
        (
            "gleam",
            LspServerConfig {
                id: "gleam".to_string(),
                command: "gleam".to_string(),
                args: vec!["lsp".to_string()],
                extensions: [".gleam"].into_iter().map(str::to_string).collect(),
                env: HashMap::new(),
                initialization: None,
                root_markers: ["gleam.toml"].into_iter().map(str::to_string).collect(),
            },
        ),
        (
            "gopls",
            LspServerConfig {
                id: "gopls".to_string(),
                command: "gopls".to_string(),
                args: Vec::new(),
                extensions: [".go"].into_iter().map(str::to_string).collect(),
                env: HashMap::new(),
                initialization: None,
                root_markers: ["go.mod", "go.work", "go.sum"]
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
            },
        ),
        (
            "hls",
            LspServerConfig {
                id: "hls".to_string(),
                command: "haskell-language-server-wrapper".to_string(),
                args: vec!["--lsp".to_string()],
                extensions: [".hs", ".lhs"].into_iter().map(str::to_string).collect(),
                env: HashMap::new(),
                initialization: None,
                root_markers: ["stack.yaml", "cabal.project", "hie.yaml"]
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
            },
        ),
        (
            "intelephense",
            LspServerConfig {
                id: "intelephense".to_string(),
                command: "intelephense".to_string(),
                args: vec!["--stdio".to_string()],
                extensions: [".php"].into_iter().map(str::to_string).collect(),
                env: HashMap::new(),
                initialization: Some(json!({
                    "telemetry": {
                        "enabled": false,
                    },
                })),
                root_markers: ["composer.json", "composer.lock", ".php-version", ".git"]
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
            },
        ),
        (
            "jdtls",
            LspServerConfig {
                id: "jdtls".to_string(),
                command: "jdtls".to_string(),
                args: Vec::new(),
                extensions: [".java"].into_iter().map(str::to_string).collect(),
                env: HashMap::new(),
                initialization: None,
                root_markers: [
                    "pom.xml",
                    "build.gradle",
                    "build.gradle.kts",
                    ".project",
                    ".classpath",
                ]
                .into_iter()
                .map(str::to_string)
                .collect(),
            },
        ),
        (
            "julials",
            LspServerConfig {
                id: "julials".to_string(),
                command: "julia".to_string(),
                args: vec![
                    "--startup-file=no".to_string(),
                    "--history-file=no".to_string(),
                    "-e".to_string(),
                    "using LanguageServer; runserver()".to_string(),
                ],
                extensions: [".jl"].into_iter().map(str::to_string).collect(),
                env: HashMap::new(),
                initialization: None,
                root_markers: ["Project.toml", "Manifest.toml"]
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
            },
        ),
        (
            "kotlin-ls",
            LspServerConfig {
                id: "kotlin-ls".to_string(),
                command: "kotlin-lsp".to_string(),
                args: vec!["--stdio".to_string()],
                extensions: [".kt", ".kts"].into_iter().map(str::to_string).collect(),
                env: HashMap::new(),
                initialization: None,
                root_markers: [
                    "settings.gradle.kts",
                    "settings.gradle",
                    "build.gradle.kts",
                    "build.gradle",
                    "pom.xml",
                ]
                .into_iter()
                .map(str::to_string)
                .collect(),
            },
        ),
        (
            "lua-ls",
            LspServerConfig {
                id: "lua-ls".to_string(),
                command: "lua-language-server".to_string(),
                args: Vec::new(),
                extensions: [".lua"].into_iter().map(str::to_string).collect(),
                env: HashMap::new(),
                initialization: None,
                root_markers: [
                    ".luarc.json",
                    ".luarc.jsonc",
                    ".luacheckrc",
                    ".stylua.toml",
                    "stylua.toml",
                    "selene.toml",
                    "selene.yml",
                ]
                .into_iter()
                .map(str::to_string)
                .collect(),
            },
        ),
        (
            "nixd",
            LspServerConfig {
                id: "nixd".to_string(),
                command: "nixd".to_string(),
                args: Vec::new(),
                extensions: [".nix"].into_iter().map(str::to_string).collect(),
                env: HashMap::new(),
                initialization: None,
                root_markers: ["flake.nix", "default.nix", "shell.nix"]
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
            },
        ),
        (
            "ocaml-lsp",
            LspServerConfig {
                id: "ocaml-lsp".to_string(),
                command: "ocamllsp".to_string(),
                args: Vec::new(),
                extensions: [".ml", ".mli"].into_iter().map(str::to_string).collect(),
                env: HashMap::new(),
                initialization: None,
                root_markers: ["dune-project", "dune-workspace", ".merlin", "opam"]
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
            },
        ),
        (
            "prisma",
            LspServerConfig {
                id: "prisma".to_string(),
                command: "prisma".to_string(),
                args: vec!["language-server".to_string()],
                extensions: [".prisma"].into_iter().map(str::to_string).collect(),
                env: HashMap::new(),
                initialization: None,
                root_markers: ["schema.prisma"].into_iter().map(str::to_string).collect(),
            },
        ),
        (
            "pyright",
            LspServerConfig {
                id: "pyright".to_string(),
                command: "pyright-langserver".to_string(),
                args: vec!["--stdio".to_string()],
                extensions: [".py", ".pyi"].into_iter().map(str::to_string).collect(),
                env: HashMap::new(),
                initialization: None,
                root_markers: [
                    "pyproject.toml",
                    "setup.py",
                    "setup.cfg",
                    "requirements.txt",
                    "Pipfile",
                    "pyrightconfig.json",
                ]
                .into_iter()
                .map(str::to_string)
                .collect(),
            },
        ),
        (
            "ruby-lsp",
            LspServerConfig {
                id: "ruby-lsp".to_string(),
                command: "rubocop".to_string(),
                args: vec!["--lsp".to_string()],
                extensions: [".rb", ".rake", ".gemspec", ".ru"]
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
                env: HashMap::new(),
                initialization: None,
                root_markers: ["Gemfile"].into_iter().map(str::to_string).collect(),
            },
        ),
        (
            "rust",
            LspServerConfig {
                id: "rust".to_string(),
                command: "rust-analyzer".to_string(),
                args: Vec::new(),
                extensions: [".rs"].into_iter().map(str::to_string).collect(),
                env: HashMap::new(),
                initialization: None,
                root_markers: ["Cargo.toml", "rust-project.json"]
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
            },
        ),
        (
            "sourcekit",
            LspServerConfig {
                id: "sourcekit".to_string(),
                command: "sourcekit-lsp".to_string(),
                args: Vec::new(),
                extensions: [".swift", ".m", ".mm", ".objc", ".objcpp"]
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
                env: HashMap::new(),
                initialization: None,
                root_markers: ["Package.swift", ".xcodeproj", ".xcworkspace"]
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
            },
        ),
        (
            "svelte",
            LspServerConfig {
                id: "svelte".to_string(),
                command: "svelteserver".to_string(),
                args: vec!["--stdio".to_string()],
                extensions: [".svelte"].into_iter().map(str::to_string).collect(),
                env: HashMap::new(),
                initialization: None,
                root_markers: [
                    "package.json",
                    "package-lock.json",
                    "pnpm-lock.yaml",
                    "yarn.lock",
                    "bun.lock",
                    "bun.lockb",
                ]
                .into_iter()
                .map(str::to_string)
                .collect(),
            },
        ),
        (
            "terraform",
            LspServerConfig {
                id: "terraform".to_string(),
                command: "terraform-ls".to_string(),
                args: vec!["serve".to_string()],
                extensions: [".tf", ".tfvars"].into_iter().map(str::to_string).collect(),
                env: HashMap::new(),
                initialization: Some(json!({
                    "experimentalFeatures": {
                        "prefillRequiredFields": true,
                        "validateOnSave": true,
                    },
                })),
                root_markers: [".terraform.lock.hcl", "terraform.tfstate"]
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
            },
        ),
        (
            "tinymist",
            LspServerConfig {
                id: "tinymist".to_string(),
                command: "tinymist".to_string(),
                args: Vec::new(),
                extensions: [".typ", ".typc"].into_iter().map(str::to_string).collect(),
                env: HashMap::new(),
                initialization: None,
                root_markers: ["typst.toml"].into_iter().map(str::to_string).collect(),
            },
        ),
        (
            "typescript",
            LspServerConfig {
                id: "typescript".to_string(),
                command: "typescript-language-server".to_string(),
                args: vec!["--stdio".to_string()],
                extensions: [".ts", ".tsx", ".js", ".jsx", ".mts", ".cts", ".mjs", ".cjs"]
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
                env: HashMap::new(),
                initialization: None,
                root_markers: [
                    "package.json",
                    "package-lock.json",
                    "pnpm-lock.yaml",
                    "yarn.lock",
                    "bun.lock",
                    "bun.lockb",
                ]
                .into_iter()
                .map(str::to_string)
                .collect(),
            },
        ),
        (
            "vue",
            LspServerConfig {
                id: "vue".to_string(),
                command: "vue-language-server".to_string(),
                args: vec!["--stdio".to_string()],
                extensions: [".vue"].into_iter().map(str::to_string).collect(),
                env: HashMap::new(),
                initialization: None,
                root_markers: [
                    "package.json",
                    "package-lock.json",
                    "pnpm-lock.yaml",
                    "yarn.lock",
                    "bun.lock",
                    "bun.lockb",
                ]
                .into_iter()
                .map(str::to_string)
                .collect(),
            },
        ),
        (
            "yaml-ls",
            LspServerConfig {
                id: "yaml-ls".to_string(),
                command: "yaml-language-server".to_string(),
                args: vec!["--stdio".to_string()],
                extensions: [".yaml", ".yml"].into_iter().map(str::to_string).collect(),
                env: HashMap::new(),
                initialization: None,
                root_markers: Vec::new(),
            },
        ),
        (
            "zls",
            LspServerConfig {
                id: "zls".to_string(),
                command: "zls".to_string(),
                args: Vec::new(),
                extensions: [".zig", ".zon"].into_iter().map(str::to_string).collect(),
                env: HashMap::new(),
                initialization: None,
                root_markers: ["build.zig"].into_iter().map(str::to_string).collect(),
            },
        ),
    ]
    .into_iter()
    .map(|(id, server)| (id.to_string(), server))
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ConfigToml;
    use crate::config::types::LspServerToml;
    use crate::config::types::LspToml;
    use crate::features::Features;

    #[test]
    fn resolve_lsp_config_supports_builtin_overrides_and_custom_servers() {
        let cfg = ConfigToml {
            lsp: Some(LspToml {
                enabled: Some(true),
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
        assert!(config.servers.iter().any(|server| server.id == "custom"));
        assert!(
            config
                .servers
                .iter()
                .find(|server| server.id == "rust")
                .expect("rust server")
                .env
                .contains_key("RUST_LOG")
        );

        let intelephense = config
            .servers
            .iter()
            .find(|server| server.id == "intelephense")
            .expect("intelephense built-in");
        assert_eq!(intelephense.command, "intelephense");
        assert_eq!(intelephense.args, vec!["--stdio"]);
        assert_eq!(intelephense.extensions, vec![".php"]);
        assert_eq!(
            intelephense.initialization,
            Some(json!({
                "telemetry": {
                    "enabled": false,
                },
            }))
        );
        assert!(
            intelephense
                .root_markers
                .iter()
                .any(|marker| marker == ".php-version")
        );

        let sourcekit = config
            .servers
            .iter()
            .find(|server| server.id == "sourcekit")
            .expect("sourcekit server");
        assert!(sourcekit.extensions.iter().any(|ext| ext == ".objc"));
        assert!(sourcekit.extensions.iter().any(|ext| ext == ".objcpp"));
    }
}
