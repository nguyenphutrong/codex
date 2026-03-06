use crate::config::ConfigToml;
use crate::config::ManagedFeatures;
use crate::config::types::LspConfig;
use crate::config::types::LspServerConfig;
use crate::config::types::LspServerToml;
use crate::features::Feature;
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
            "clangd",
            LspServerConfig {
                id: "clangd".to_string(),
                command: "clangd".to_string(),
                args: Vec::new(),
                extensions: [
                    ".c", ".cc", ".cpp", ".cxx", ".h", ".hh", ".hpp", ".hxx", ".m", ".mm",
                ]
                .into_iter()
                .map(str::to_string)
                .collect(),
                env: HashMap::new(),
                initialization: None,
                root_markers: ["compile_commands.json", ".clangd"]
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
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
                root_markers: ["go.mod", "go.work"]
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
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
                root_markers: ["pyproject.toml", "setup.py", "requirements.txt"]
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
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
                extensions: [".swift", ".m", ".mm"]
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
    }
}
