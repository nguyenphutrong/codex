use crate::config::Config;
use crate::config::types::LspConfig;
use crate::config::types::LspMode;
use crate::config::types::LspRuntimeKind;
use crate::config::types::LspServerConfig;
use codex_lsp::LspServerAvailability;
use codex_lsp::LspServerSource;
use codex_lsp::ResolvedServerConfig;
use codex_lsp::ServerConfig as RuntimeLspServerConfig;
use codex_lsp::ServerResolution;
use codex_lsp::SessionManager as LspSessionManager;
use codex_lsp::UnavailableServer;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::fs;
use tokio::process::Command;
use tokio::sync::Mutex;
use tracing::info;
use which::which;

const LSP_CACHE_ROOT_RELATIVE: &str = "packages/lsp/npm";

#[derive(Clone)]
pub struct LspRuntimeResolver {
    codex_home: PathBuf,
    mode: LspMode,
    servers: Arc<HashMap<String, LspServerConfig>>,
    install_locks: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
}

impl LspRuntimeResolver {
    pub fn new(codex_home: PathBuf, config: &LspConfig) -> Self {
        Self {
            codex_home,
            mode: config.mode,
            servers: Arc::new(
                config
                    .servers
                    .iter()
                    .cloned()
                    .map(|server| (server.id.clone(), server))
                    .collect(),
            ),
            install_locks: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn resolve(
        &self,
        runtime_server: RuntimeLspServerConfig,
        workspace_root: PathBuf,
    ) -> ServerResolution {
        if self.mode == LspMode::Off {
            return ServerResolution::Unavailable(UnavailableServer {
                availability: LspServerAvailability::Unavailable,
                reason: "LSP is disabled by configuration.".to_string(),
                requirements: None,
                source: None,
            });
        }

        let Some(server) = self.servers.get(&runtime_server.id).cloned() else {
            return self.resolve_configured(runtime_server).await;
        };

        if let Some(project_command) = self
            .resolve_project_command(&server, &workspace_root, &runtime_server.args)
            .await
        {
            return project_command;
        }

        if let Some(managed_command) = self.resolve_managed_cache(&server).await {
            return managed_command;
        }

        if let Ok(global_command) = which(&runtime_server.command) {
            return ServerResolution::Resolved(ResolvedServerConfig {
                command: global_command.to_string_lossy().into_owned(),
                args: runtime_server.args,
                env: runtime_server.env,
                source: if server.runtime_kind == LspRuntimeKind::UserConfigured {
                    LspServerSource::Configured
                } else {
                    LspServerSource::Global
                },
            });
        }

        if matches!(server.runtime_kind, LspRuntimeKind::ManagedNpm)
            && let Some(managed_command) = self.install_managed_npm(&server, &runtime_server).await
        {
            return managed_command;
        }

        let availability = match server.runtime_kind {
            LspRuntimeKind::ToolchainProvided | LspRuntimeKind::ProjectDependency => {
                LspServerAvailability::RequirementsMissing
            }
            _ => LspServerAvailability::Unavailable,
        };
        ServerResolution::Unavailable(UnavailableServer {
            availability,
            reason: format!(
                "No usable LSP runtime found for `{}` in {}.",
                server.id,
                workspace_root.display()
            ),
            requirements: server.requirements.clone(),
            source: None,
        })
    }

    async fn resolve_configured(&self, runtime_server: RuntimeLspServerConfig) -> ServerResolution {
        match which(&runtime_server.command) {
            Ok(command) => ServerResolution::Resolved(ResolvedServerConfig {
                command: command.to_string_lossy().into_owned(),
                args: runtime_server.args,
                env: runtime_server.env,
                source: LspServerSource::Configured,
            }),
            Err(err) => ServerResolution::Unavailable(UnavailableServer {
                availability: LspServerAvailability::Unavailable,
                reason: err.to_string(),
                requirements: None,
                source: Some(LspServerSource::Configured),
            }),
        }
    }

    async fn resolve_project_command(
        &self,
        server: &LspServerConfig,
        workspace_root: &Path,
        args: &[String],
    ) -> Option<ServerResolution> {
        for candidate in &server.project_local_candidates {
            let path = workspace_root.join(candidate);
            if !path.exists() {
                continue;
            }
            return Some(ServerResolution::Resolved(ResolvedServerConfig {
                command: path.to_string_lossy().into_owned(),
                args: args.to_vec(),
                env: HashMap::new(),
                source: LspServerSource::Project,
            }));
        }
        None
    }

    async fn resolve_managed_cache(&self, server: &LspServerConfig) -> Option<ServerResolution> {
        let spec = server.managed_npm.as_ref()?;
        let bin_path = managed_npm_bin_path(&self.codex_home, &server.id, &spec.version, &spec.bin);
        if !bin_path.exists() {
            return None;
        }
        Some(ServerResolution::Resolved(ResolvedServerConfig {
            command: bin_path.to_string_lossy().into_owned(),
            args: server.args.clone(),
            env: server.env.clone(),
            source: LspServerSource::ManagedCache,
        }))
    }

    async fn install_managed_npm(
        &self,
        server: &LspServerConfig,
        runtime_server: &RuntimeLspServerConfig,
    ) -> Option<ServerResolution> {
        let spec = server.managed_npm.as_ref()?;
        let lock = self.install_lock(&server.id, &spec.version).await;
        let _guard = lock.lock().await;

        if let Some(cached) = self.resolve_managed_cache(server).await {
            return Some(cached);
        }

        let root = managed_npm_root(&self.codex_home, &server.id, &spec.version);
        if fs::create_dir_all(&root).await.is_err() {
            return None;
        }
        let package_json = root.join("package.json");
        if !package_json.exists()
            && fs::write(&package_json, "{ \"private\": true }\n")
                .await
                .is_err()
        {
            return None;
        }

        info!(
            server_id = %server.id,
            package = %spec.package,
            version = %spec.version,
            install_root = %root.display(),
            "Installing managed LSP npm package"
        );
        let output = Command::new("npm")
            .args([
                "install",
                "--no-package-lock",
                "--no-save",
                "--fund=false",
                "--audit=false",
                "--loglevel=error",
                &format!("{}@{}", spec.package, spec.version),
            ])
            .current_dir(&root)
            .output()
            .await
            .ok()?;
        if !output.status.success() {
            return Some(ServerResolution::Unavailable(UnavailableServer {
                availability: LspServerAvailability::Unavailable,
                reason: String::from_utf8_lossy(&output.stderr).trim().to_string(),
                requirements: server.requirements.clone(),
                source: Some(LspServerSource::ManagedCache),
            }));
        }

        let bin_path = managed_npm_bin_path(&self.codex_home, &server.id, &spec.version, &spec.bin);
        if !bin_path.exists() {
            return Some(ServerResolution::Unavailable(UnavailableServer {
                availability: LspServerAvailability::Unavailable,
                reason: format!(
                    "Managed install completed but `{}` was not found.",
                    bin_path.display()
                ),
                requirements: server.requirements.clone(),
                source: Some(LspServerSource::ManagedCache),
            }));
        }

        Some(ServerResolution::Resolved(ResolvedServerConfig {
            command: bin_path.to_string_lossy().into_owned(),
            args: runtime_server.args.clone(),
            env: runtime_server.env.clone(),
            source: LspServerSource::ManagedCache,
        }))
    }

    async fn install_lock(&self, server_id: &str, version: &str) -> Arc<Mutex<()>> {
        let mut locks = self.install_locks.lock().await;
        locks
            .entry(format!("{server_id}:{version}"))
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }
}

pub fn build_lsp_session_manager(config: &Config) -> Option<Arc<LspSessionManager>> {
    let lsp = config.lsp.as_ref()?;
    if lsp.mode == LspMode::Off {
        return None;
    }

    let resolver = LspRuntimeResolver::new(config.codex_home.clone(), lsp);
    let runtime_config = codex_lsp::LspConfig {
        servers: lsp
            .servers
            .iter()
            .map(|server| RuntimeLspServerConfig {
                id: server.id.clone(),
                command: server.command.clone(),
                args: server.args.clone(),
                extensions: server.extensions.clone(),
                env: server.env.clone(),
                initialization: server.initialization.clone(),
                root_markers: server.root_markers.clone(),
            })
            .collect(),
    };

    Some(Arc::new(LspSessionManager::with_resolver(
        Some(runtime_config),
        move |server, workspace_root| {
            let resolver = resolver.clone();
            Box::pin(async move { resolver.resolve(server, workspace_root).await })
        },
    )))
}

fn managed_npm_root(codex_home: &Path, server_id: &str, version: &str) -> PathBuf {
    codex_home
        .join(LSP_CACHE_ROOT_RELATIVE)
        .join(server_id)
        .join(version)
}

fn managed_npm_bin_path(codex_home: &Path, server_id: &str, version: &str, bin: &str) -> PathBuf {
    let base = managed_npm_root(codex_home, server_id, version)
        .join("node_modules")
        .join(".bin")
        .join(bin);
    if cfg!(windows) {
        return base.with_extension("cmd");
    }
    base
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::ManagedNpmLspServerConfig;
    use pretty_assertions::assert_eq;
    use tempfile::tempdir;

    fn config_with_server(server: LspServerConfig) -> LspConfig {
        LspConfig {
            mode: LspMode::Auto,
            assume_yes: true,
            servers: vec![server],
        }
    }

    #[tokio::test]
    async fn chooses_project_candidate_before_global() {
        let tmp = tempdir().expect("tmp");
        let project_bin = tmp
            .path()
            .join("node_modules/.bin/typescript-language-server");
        std::fs::create_dir_all(project_bin.parent().expect("parent")).expect("mkdirs");
        std::fs::write(&project_bin, "").expect("bin");

        let resolver = LspRuntimeResolver::new(
            tmp.path().to_path_buf(),
            &config_with_server(LspServerConfig {
                id: "typescript".to_string(),
                command: "typescript-language-server".to_string(),
                args: vec!["--stdio".to_string()],
                extensions: vec![".ts".to_string()],
                env: HashMap::new(),
                initialization: None,
                root_markers: vec!["package.json".to_string()],
                runtime_kind: LspRuntimeKind::ManagedNpm,
                project_local_candidates: vec![
                    "node_modules/.bin/typescript-language-server".to_string(),
                ],
                requirements: Some("req".to_string()),
                managed_npm: Some(ManagedNpmLspServerConfig {
                    package: "typescript-language-server".to_string(),
                    version: "5.1.3".to_string(),
                    bin: "typescript-language-server".to_string(),
                }),
            }),
        );

        let resolution = resolver
            .resolve(
                RuntimeLspServerConfig {
                    id: "typescript".to_string(),
                    command: "typescript-language-server".to_string(),
                    args: vec!["--stdio".to_string()],
                    extensions: vec![".ts".to_string()],
                    env: HashMap::new(),
                    initialization: None,
                    root_markers: vec!["package.json".to_string()],
                },
                tmp.path().to_path_buf(),
            )
            .await;

        assert_eq!(
            resolution,
            ServerResolution::Resolved(ResolvedServerConfig {
                command: project_bin.to_string_lossy().into_owned(),
                args: vec!["--stdio".to_string()],
                env: HashMap::new(),
                source: LspServerSource::Project,
            })
        );
    }

    #[tokio::test]
    async fn chooses_managed_cache_before_global() {
        let tmp = tempdir().expect("tmp");
        let managed_bin =
            managed_npm_bin_path(tmp.path(), "yaml-ls", "1.21.0", "yaml-language-server");
        std::fs::create_dir_all(managed_bin.parent().expect("parent")).expect("mkdirs");
        std::fs::write(&managed_bin, "").expect("bin");

        let resolver = LspRuntimeResolver::new(
            tmp.path().to_path_buf(),
            &config_with_server(LspServerConfig {
                id: "yaml-ls".to_string(),
                command: "yaml-language-server".to_string(),
                args: vec!["--stdio".to_string()],
                extensions: vec![".yaml".to_string()],
                env: HashMap::new(),
                initialization: None,
                root_markers: Vec::new(),
                runtime_kind: LspRuntimeKind::ManagedNpm,
                project_local_candidates: Vec::new(),
                requirements: Some("req".to_string()),
                managed_npm: Some(ManagedNpmLspServerConfig {
                    package: "yaml-language-server".to_string(),
                    version: "1.21.0".to_string(),
                    bin: "yaml-language-server".to_string(),
                }),
            }),
        );

        let resolution = resolver
            .resolve(
                RuntimeLspServerConfig {
                    id: "yaml-ls".to_string(),
                    command: "yaml-language-server".to_string(),
                    args: vec!["--stdio".to_string()],
                    extensions: vec![".yaml".to_string()],
                    env: HashMap::new(),
                    initialization: None,
                    root_markers: Vec::new(),
                },
                tmp.path().to_path_buf(),
            )
            .await;

        assert_eq!(
            resolution,
            ServerResolution::Resolved(ResolvedServerConfig {
                command: managed_bin.to_string_lossy().into_owned(),
                args: vec!["--stdio".to_string()],
                env: HashMap::new(),
                source: LspServerSource::ManagedCache,
            })
        );
    }

    #[tokio::test]
    async fn off_mode_short_circuits_resolution() {
        let mut config = config_with_server(LspServerConfig {
            id: "rust".to_string(),
            command: "rust-analyzer".to_string(),
            args: Vec::new(),
            extensions: vec![".rs".to_string()],
            env: HashMap::new(),
            initialization: None,
            root_markers: vec!["Cargo.toml".to_string()],
            runtime_kind: LspRuntimeKind::ToolchainProvided,
            project_local_candidates: Vec::new(),
            requirements: Some("Requires rust-analyzer".to_string()),
            managed_npm: None,
        });
        config.mode = LspMode::Off;
        let tmp = tempdir().expect("tmp");
        let resolver = LspRuntimeResolver::new(tmp.path().to_path_buf(), &config);
        let resolution = resolver
            .resolve(
                RuntimeLspServerConfig {
                    id: "rust".to_string(),
                    command: "rust-analyzer".to_string(),
                    args: Vec::new(),
                    extensions: vec![".rs".to_string()],
                    env: HashMap::new(),
                    initialization: None,
                    root_markers: vec!["Cargo.toml".to_string()],
                },
                tmp.path().to_path_buf(),
            )
            .await;
        assert_eq!(
            resolution,
            ServerResolution::Unavailable(UnavailableServer {
                availability: LspServerAvailability::Unavailable,
                reason: "LSP is disabled by configuration.".to_string(),
                requirements: None,
                source: None,
            })
        );
    }
}
