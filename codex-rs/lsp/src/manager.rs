use crate::client::ClientHandle;
use crate::normalize::normalize_call_hierarchy_calls;
use crate::normalize::normalize_call_hierarchy_items;
use crate::normalize::normalize_document_symbols;
use crate::normalize::normalize_hover;
use crate::normalize::normalize_location_like;
use crate::normalize::normalize_symbol_information;
use crate::types::LspClientState;
use crate::types::LspConfig;
use crate::types::LspDiagnostic;
use crate::types::LspOperationResult;
use crate::types::LspStatus;
use crate::types::PositionRequest;
use crate::types::ServerConfig;
#[cfg(test)]
use crate::util::backoff_for_failure;
use crate::util::path_to_uri;
use crate::util::resolve_absolute_path;
use crate::util::resolve_command;
use crate::util::resolve_workspace_root;
use crate::util::to_lsp_position;
use crate::util::workspace_roots_overlap;
use anyhow::Result;
use anyhow::anyhow;
use serde_json::Value;
use serde_json::json;
use std::collections::HashMap;
use std::future::Future;
use std::path::Path;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;
use std::time::SystemTime;
use tokio::sync::Mutex;
use tokio::sync::RwLock;
use tokio::time::Duration;
use tracing::warn;

type SpawnClientFuture = Pin<Box<dyn Future<Output = Result<Arc<ClientHandle>>> + Send + 'static>>;
type SpawnClientFn = dyn Fn(ServerConfig, PathBuf) -> SpawnClientFuture + Send + Sync;

const CLIENT_RESTART_BACKOFF: [Duration; 3] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
];

#[derive(Clone)]
pub struct SessionManager {
    config: Option<Arc<LspConfig>>,
    pub(crate) clients: Arc<RwLock<HashMap<ClientKey, Arc<ClientHandle>>>>,
    pub(crate) client_health: Arc<RwLock<HashMap<ClientKey, ClientHealth>>>,
    client_locks: Arc<Mutex<HashMap<ClientKey, Arc<Mutex<()>>>>>,
    workspace_root_cache: Arc<RwLock<HashMap<WorkspaceRootCacheKey, PathBuf>>>,
    drop_guard: Arc<()>,
    spawn_client: Arc<SpawnClientFn>,
    restart_backoff: [Duration; 3],
}

impl SessionManager {
    pub fn new(config: Option<LspConfig>) -> Self {
        Self {
            config: config.map(Arc::new),
            clients: Arc::new(RwLock::new(HashMap::new())),
            client_health: Arc::new(RwLock::new(HashMap::new())),
            client_locks: Arc::new(Mutex::new(HashMap::new())),
            workspace_root_cache: Arc::new(RwLock::new(HashMap::new())),
            drop_guard: Arc::new(()),
            spawn_client: Arc::new(|server, workspace_root| {
                Box::pin(ClientHandle::spawn(server, workspace_root))
            }),
            restart_backoff: CLIENT_RESTART_BACKOFF,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_options<F>(
        config: Option<LspConfig>,
        spawn_client: F,
        restart_backoff: [Duration; 3],
    ) -> Self
    where
        F: Fn(ServerConfig, PathBuf) -> SpawnClientFuture + Send + Sync + 'static,
    {
        Self {
            config: config.map(Arc::new),
            clients: Arc::new(RwLock::new(HashMap::new())),
            client_health: Arc::new(RwLock::new(HashMap::new())),
            client_locks: Arc::new(Mutex::new(HashMap::new())),
            workspace_root_cache: Arc::new(RwLock::new(HashMap::new())),
            drop_guard: Arc::new(()),
            spawn_client: Arc::new(spawn_client),
            restart_backoff,
        }
    }

    pub async fn has_server_for_file(&self, file_path: &Path, base_dir: &Path) -> bool {
        !self
            .matching_server_matches(file_path, base_dir)
            .await
            .is_empty()
    }

    pub async fn touch_file(
        &self,
        file_path: &Path,
        base_dir: &Path,
        wait_for_diagnostics: bool,
    ) -> Result<()> {
        let matches = self.matching_server_matches(file_path, base_dir).await;
        if matches.is_empty() {
            return Ok(());
        }

        for server_match in matches {
            let key = self.client_key(&server_match);
            let Ok(client) = self.client_for_match(&server_match).await else {
                continue;
            };
            let touch_revision = match client.open_or_change(&server_match.file_path).await {
                Ok(touch_revision) => touch_revision,
                Err(err) => {
                    warn!(
                        "LSP client {} in {} failed to open {}: {err:#}",
                        key.server_id,
                        key.workspace_root.display(),
                        server_match.file_path.display()
                    );
                    self.mark_client_broken_and_restart(key, err.to_string())
                        .await;
                    continue;
                }
            };
            self.mark_client_success(&server_match.server.id, &server_match.workspace_root)
                .await;

            if wait_for_diagnostics
                && let Err(err) = client
                    .wait_for_diagnostics(&server_match.file_path, touch_revision)
                    .await
            {
                warn!(
                    "LSP client {} in {} did not receive fresh diagnostics for {}: {err:#}",
                    key.server_id,
                    key.workspace_root.display(),
                    server_match.file_path.display()
                );
            }
        }

        Ok(())
    }

    pub async fn diagnostics_for_paths(
        &self,
        paths: &[PathBuf],
        base_dir: &Path,
    ) -> HashMap<PathBuf, Vec<LspDiagnostic>> {
        let mut aggregated = HashMap::new();

        for file_path in paths {
            let matches = self.matching_server_matches(file_path, base_dir).await;
            for server_match in matches {
                let Ok(client) = self.client_for_match(&server_match).await else {
                    continue;
                };
                let diagnostics = client.diagnostics_for_path(&server_match.file_path).await;
                if diagnostics.is_empty() {
                    continue;
                }

                aggregated
                    .entry(server_match.file_path.clone())
                    .or_insert_with(Vec::new)
                    .extend(diagnostics);
            }
        }

        aggregated
    }

    pub async fn status_for_file(&self, file_path: &Path, base_dir: &Path) -> Vec<LspStatus> {
        let matches = self.matching_server_matches(file_path, base_dir).await;
        let mut statuses = Vec::new();

        for server_match in matches {
            let key = self.client_key(&server_match);
            self.reconcile_client_health(&key).await;
            let health = self.client_health(&key).await.unwrap_or_default();
            let retry_after_seconds = health.retry_at.and_then(|retry_at| {
                retry_at
                    .checked_duration_since(Instant::now())
                    .map(|duration| duration.as_secs())
            });
            statuses.push(LspStatus {
                server: server_match.server.id.clone(),
                workspace_root: server_match.workspace_root,
                state: health.state,
                retry_after_seconds,
                last_error: health.last_error,
            });
        }

        statuses
    }

    pub async fn definition(
        &self,
        request: PositionRequest,
        base_dir: &Path,
    ) -> Result<Vec<LspOperationResult>> {
        self.run_position_operation(
            "go_to_definition",
            request,
            base_dir,
            |client, req| async move {
                client
                    .send_request(
                        "textDocument/definition",
                        json!({
                            "textDocument": {
                                "uri": path_to_uri(&req.file_path)?,
                            },
                            "position": to_lsp_position(req.line, req.character),
                        }),
                    )
                    .await
            },
        )
        .await
        .map(|results| {
            results
                .into_iter()
                .map(|result| LspOperationResult {
                    server: result.server,
                    workspace_root: result.workspace_root,
                    operation: "go_to_definition".to_string(),
                    items: normalize_location_like(&result.payload),
                })
                .collect()
        })
    }

    pub async fn references(
        &self,
        request: PositionRequest,
        base_dir: &Path,
    ) -> Result<Vec<LspOperationResult>> {
        self.run_position_operation(
            "find_references",
            request,
            base_dir,
            |client, req| async move {
                client
                    .send_request(
                        "textDocument/references",
                        json!({
                            "textDocument": {
                                "uri": path_to_uri(&req.file_path)?,
                            },
                            "position": to_lsp_position(req.line, req.character),
                            "context": {
                                "includeDeclaration": true,
                            },
                        }),
                    )
                    .await
            },
        )
        .await
        .map(|results| {
            results
                .into_iter()
                .map(|result| LspOperationResult {
                    server: result.server,
                    workspace_root: result.workspace_root,
                    operation: "find_references".to_string(),
                    items: normalize_location_like(&result.payload),
                })
                .collect()
        })
    }

    pub async fn hover(
        &self,
        request: PositionRequest,
        base_dir: &Path,
    ) -> Result<Vec<LspOperationResult>> {
        self.run_position_operation("hover", request, base_dir, |client, req| async move {
            client
                .send_request(
                    "textDocument/hover",
                    json!({
                        "textDocument": {
                            "uri": path_to_uri(&req.file_path)?,
                        },
                        "position": to_lsp_position(req.line, req.character),
                    }),
                )
                .await
        })
        .await
        .map(|results| {
            results
                .into_iter()
                .map(|result| LspOperationResult {
                    server: result.server,
                    workspace_root: result.workspace_root,
                    operation: "hover".to_string(),
                    items: normalize_hover(&result.payload).into_iter().collect(),
                })
                .collect()
        })
    }

    pub async fn implementation(
        &self,
        request: PositionRequest,
        base_dir: &Path,
    ) -> Result<Vec<LspOperationResult>> {
        self.run_position_operation(
            "go_to_implementation",
            request,
            base_dir,
            |client, req| async move {
                client
                    .send_request(
                        "textDocument/implementation",
                        json!({
                            "textDocument": {
                                "uri": path_to_uri(&req.file_path)?,
                            },
                            "position": to_lsp_position(req.line, req.character),
                        }),
                    )
                    .await
            },
        )
        .await
        .map(|results| {
            results
                .into_iter()
                .map(|result| LspOperationResult {
                    server: result.server,
                    workspace_root: result.workspace_root,
                    operation: "go_to_implementation".to_string(),
                    items: normalize_location_like(&result.payload),
                })
                .collect()
        })
    }

    pub async fn document_symbol(
        &self,
        file_path: &Path,
        base_dir: &Path,
    ) -> Result<Vec<LspOperationResult>> {
        let matches = self.matching_server_matches(file_path, base_dir).await;
        if matches.is_empty() {
            return Err(anyhow!("No LSP server available for this file type."));
        }

        let mut results = Vec::new();
        for server_match in matches {
            let key = self.client_key(&server_match);
            let client = match self.client_for_match(&server_match).await {
                Ok(client) => client,
                Err(err) => {
                    warn!(
                        "LSP server {} in {} could not be reused for document_symbol: {err:#}",
                        key.server_id,
                        key.workspace_root.display()
                    );
                    continue;
                }
            };

            if let Err(err) = client.open_or_change(&server_match.file_path).await {
                warn!(
                    "LSP server {} in {} failed to open {} for document_symbol: {err:#}",
                    key.server_id,
                    key.workspace_root.display(),
                    server_match.file_path.display()
                );
                self.mark_client_broken_and_restart(key, err.to_string())
                    .await;
                continue;
            }
            self.mark_client_success(&server_match.server.id, &server_match.workspace_root)
                .await;

            let payload = match client
                .send_request(
                    "textDocument/documentSymbol",
                    json!({
                        "textDocument": {
                            "uri": path_to_uri(&server_match.file_path)?,
                        },
                    }),
                )
                .await
            {
                Ok(payload) => payload,
                Err(err) => {
                    warn!(
                        "LSP request textDocument/documentSymbol failed for {} in {}: {err:#}",
                        key.server_id,
                        key.workspace_root.display()
                    );
                    self.mark_client_broken_and_restart(key, err.to_string())
                        .await;
                    continue;
                }
            };
            self.mark_client_success(&server_match.server.id, &server_match.workspace_root)
                .await;

            results.push(LspOperationResult {
                server: server_match.server.id.clone(),
                workspace_root: server_match.workspace_root,
                operation: "document_symbol".to_string(),
                items: normalize_document_symbols(&payload),
            });
        }

        if results.is_empty() {
            return Err(anyhow!(
                "No LSP server was able to process document_symbol for this file."
            ));
        }

        Ok(results)
    }

    pub async fn workspace_symbol(
        &self,
        query: &str,
        base_dir: &Path,
    ) -> Result<Vec<LspOperationResult>> {
        let matches = self.active_workspace_matches(base_dir).await;
        if matches.is_empty() {
            return Err(anyhow!(
                "No active LSP client available for this workspace. Read or query a file first, or provide file_path to scope workspace_symbol."
            ));
        }

        self.workspace_symbol_for_matches(query, matches).await
    }

    pub async fn workspace_symbol_for_file(
        &self,
        query: &str,
        file_path: &Path,
        base_dir: &Path,
    ) -> Result<Vec<LspOperationResult>> {
        let matches = self.matching_server_matches(file_path, base_dir).await;
        if matches.is_empty() {
            return Err(anyhow!("No LSP server available for this file type."));
        }

        self.workspace_symbol_for_matches(query, matches).await
    }

    async fn workspace_symbol_for_matches(
        &self,
        query: &str,
        matches: Vec<ServerMatch>,
    ) -> Result<Vec<LspOperationResult>> {
        let mut results = Vec::new();
        for server_match in matches {
            let key = self.client_key(&server_match);
            let client = match self.client_for_match(&server_match).await {
                Ok(client) => client,
                Err(err) => {
                    warn!(
                        "LSP server {} in {} could not be reused for workspace_symbol: {err:#}",
                        key.server_id,
                        key.workspace_root.display()
                    );
                    continue;
                }
            };

            let payload = match client
                .send_request(
                    "workspace/symbol",
                    json!({
                        "query": query,
                    }),
                )
                .await
            {
                Ok(payload) => payload,
                Err(err) => {
                    warn!(
                        "LSP request workspace/symbol failed for {} in {}: {err:#}",
                        key.server_id,
                        key.workspace_root.display()
                    );
                    self.mark_client_broken_and_restart(key, err.to_string())
                        .await;
                    continue;
                }
            };
            self.mark_client_success(&server_match.server.id, &server_match.workspace_root)
                .await;

            results.push(LspOperationResult {
                server: server_match.server.id.clone(),
                workspace_root: server_match.workspace_root,
                operation: "workspace_symbol".to_string(),
                items: normalize_symbol_information(&payload),
            });
        }

        if results.is_empty() {
            return Err(anyhow!(
                "No LSP server was able to process workspace_symbol for this workspace."
            ));
        }

        Ok(results)
    }

    pub async fn prepare_call_hierarchy(
        &self,
        request: PositionRequest,
        base_dir: &Path,
    ) -> Result<Vec<LspOperationResult>> {
        self.run_position_operation(
            "prepare_call_hierarchy",
            request,
            base_dir,
            |client, req| async move {
                client
                    .send_request(
                        "textDocument/prepareCallHierarchy",
                        json!({
                            "textDocument": {
                                "uri": path_to_uri(&req.file_path)?,
                            },
                            "position": to_lsp_position(req.line, req.character),
                        }),
                    )
                    .await
            },
        )
        .await
        .map(|results| {
            results
                .into_iter()
                .map(|result| LspOperationResult {
                    server: result.server,
                    workspace_root: result.workspace_root,
                    operation: "prepare_call_hierarchy".to_string(),
                    items: normalize_call_hierarchy_items(&result.payload),
                })
                .collect()
        })
    }

    pub async fn incoming_calls(
        &self,
        request: PositionRequest,
        base_dir: &Path,
    ) -> Result<Vec<LspOperationResult>> {
        self.run_call_hierarchy_follow_up(
            "incoming_calls",
            request,
            base_dir,
            "callHierarchy/incomingCalls",
        )
        .await
    }

    pub async fn outgoing_calls(
        &self,
        request: PositionRequest,
        base_dir: &Path,
    ) -> Result<Vec<LspOperationResult>> {
        self.run_call_hierarchy_follow_up(
            "outgoing_calls",
            request,
            base_dir,
            "callHierarchy/outgoingCalls",
        )
        .await
    }

    async fn run_call_hierarchy_follow_up(
        &self,
        operation: &str,
        request: PositionRequest,
        base_dir: &Path,
        method: &str,
    ) -> Result<Vec<LspOperationResult>> {
        let matches = self
            .matching_server_matches(&request.file_path, base_dir)
            .await;
        if matches.is_empty() {
            return Err(anyhow!("No LSP server available for this file type."));
        }

        let mut results = Vec::new();
        for server_match in matches {
            let key = self.client_key(&server_match);
            let client = match self.client_for_match(&server_match).await {
                Ok(client) => client,
                Err(err) => {
                    warn!(
                        "LSP server {} in {} could not be reused for {}: {err:#}",
                        key.server_id,
                        key.workspace_root.display(),
                        operation
                    );
                    continue;
                }
            };

            if let Err(err) = client.open_or_change(&server_match.file_path).await {
                warn!(
                    "LSP server {} in {} failed to open {} for {}: {err:#}",
                    key.server_id,
                    key.workspace_root.display(),
                    server_match.file_path.display(),
                    operation
                );
                self.mark_client_broken_and_restart(key.clone(), err.to_string())
                    .await;
                continue;
            }
            self.mark_client_success(&server_match.server.id, &server_match.workspace_root)
                .await;

            let prepared = match client
                .send_request(
                    "textDocument/prepareCallHierarchy",
                    json!({
                    "textDocument": {
                        "uri": path_to_uri(&server_match.file_path)?,
                    },
                        "position": to_lsp_position(request.line, request.character),
                    }),
                )
                .await
            {
                Ok(payload) => payload,
                Err(err) => {
                    warn!(
                        "LSP prepareCallHierarchy request failed for {} in {}: {err:#}",
                        key.server_id,
                        key.workspace_root.display(),
                    );
                    self.mark_client_broken_and_restart(key.clone(), err.to_string())
                        .await;
                    continue;
                }
            };

            let Some(first_item) = prepared.as_array().and_then(|items| items.first()).cloned()
            else {
                results.push(LspOperationResult {
                    server: server_match.server.id.clone(),
                    workspace_root: server_match.workspace_root,
                    operation: operation.to_string(),
                    items: Vec::new(),
                });
                continue;
            };

            let payload = match client
                .send_request(
                    method,
                    json!({
                        "item": first_item,
                    }),
                )
                .await
            {
                Ok(payload) => payload,
                Err(err) => {
                    warn!(
                        "LSP request {method} failed for {} in {}: {err:#}",
                        key.server_id,
                        key.workspace_root.display(),
                    );
                    self.mark_client_broken_and_restart(key, err.to_string())
                        .await;
                    continue;
                }
            };
            self.mark_client_success(&server_match.server.id, &server_match.workspace_root)
                .await;

            results.push(LspOperationResult {
                server: server_match.server.id.clone(),
                workspace_root: server_match.workspace_root,
                operation: operation.to_string(),
                items: normalize_call_hierarchy_calls(&payload),
            });
        }

        if results.is_empty() {
            return Err(anyhow!(
                "No LSP server was able to process this call hierarchy operation."
            ));
        }

        Ok(results)
    }

    async fn run_position_operation<F, Fut>(
        &self,
        operation_name: &str,
        request: PositionRequest,
        base_dir: &Path,
        operation: F,
    ) -> Result<Vec<RawOperationResult>>
    where
        F: Fn(Arc<ClientHandle>, PositionRequest) -> Fut + Copy,
        Fut: std::future::Future<Output = Result<Value>>,
    {
        let matches = self
            .matching_server_matches(&request.file_path, base_dir)
            .await;
        if matches.is_empty() {
            return Err(anyhow!("No LSP server available for this file type."));
        }

        let mut results = Vec::new();
        for server_match in matches {
            let key = self.client_key(&server_match);
            let client = match self.client_for_match(&server_match).await {
                Ok(client) => client,
                Err(err) => {
                    warn!(
                        "LSP server {} in {} could not be reused for {}: {err:#}",
                        key.server_id,
                        key.workspace_root.display(),
                        operation_name
                    );
                    continue;
                }
            };

            if let Err(err) = client.open_or_change(&server_match.file_path).await {
                warn!(
                    "LSP server {} in {} failed to open {} for {}: {err:#}",
                    key.server_id,
                    key.workspace_root.display(),
                    server_match.file_path.display(),
                    operation_name
                );
                self.mark_client_broken_and_restart(key.clone(), err.to_string())
                    .await;
                continue;
            }
            self.mark_client_success(&server_match.server.id, &server_match.workspace_root)
                .await;

            let payload = match operation(
                client,
                PositionRequest {
                    file_path: server_match.file_path.clone(),
                    line: request.line,
                    character: request.character,
                },
            )
            .await
            {
                Ok(payload) => payload,
                Err(err) => {
                    warn!(
                        "LSP request for {operation_name} failed for {} in {}: {err:#}",
                        key.server_id,
                        key.workspace_root.display(),
                    );
                    self.mark_client_broken_and_restart(key, err.to_string())
                        .await;
                    continue;
                }
            };
            self.mark_client_success(&server_match.server.id, &server_match.workspace_root)
                .await;
            results.push(RawOperationResult {
                server: server_match.server.id.clone(),
                workspace_root: server_match.workspace_root,
                payload,
            });
        }

        if results.is_empty() {
            return Err(anyhow!("No LSP server was able to process this operation."));
        }

        Ok(results)
    }

    pub(crate) async fn client_for_match(
        &self,
        server_match: &ServerMatch,
    ) -> Result<Arc<ClientHandle>> {
        let key = self.client_key(server_match);
        self.reconcile_client_health(&key).await;
        if self.is_permanently_broken(&key).await {
            return Err(anyhow!(
                "LSP server {} is permanently broken for {}.",
                key.server_id,
                key.workspace_root.display(),
            ));
        }
        if let Some(retry_after_seconds) = self.retry_after_seconds(&key).await {
            return Err(anyhow!(
                "LSP server {} is temporarily unavailable for {}. Retry later in {}s.",
                key.server_id,
                key.workspace_root.display(),
                retry_after_seconds,
            ));
        }

        if let Some(existing) = self.clients.read().await.get(&key).cloned() {
            return Ok(existing);
        }

        let client_lock = self.client_lock(&key).await;
        let _guard = client_lock.lock().await;

        self.reconcile_client_health(&key).await;
        if self.is_permanently_broken(&key).await {
            return Err(anyhow!(
                "LSP server {} is permanently broken for {}.",
                key.server_id,
                key.workspace_root.display(),
            ));
        }
        if let Some(retry_after_seconds) = self.retry_after_seconds(&key).await {
            return Err(anyhow!(
                "LSP server {} is temporarily unavailable for {}. Retry later in {}s.",
                key.server_id,
                key.workspace_root.display(),
                retry_after_seconds,
            ));
        }

        if let Some(existing) = self.clients.read().await.get(&key).cloned() {
            return Ok(existing);
        }

        self.mark_client_starting(&server_match.server.id, &server_match.workspace_root)
            .await;

        let client = match (self.spawn_client)(
            server_match.server.clone(),
            key.workspace_root.clone(),
        )
        .await
        {
            Ok(client) => client,
            Err(err) => {
                warn!(
                    "failed to start LSP server {} in {}: {err:#}",
                    key.server_id,
                    key.workspace_root.display()
                );
                self.mark_client_broken_and_restart(key.clone(), err.to_string())
                    .await;
                return Err(err);
            }
        };

        self.clients
            .write()
            .await
            .insert(key.clone(), client.clone());
        self.mark_client_success(&server_match.server.id, &server_match.workspace_root)
            .await;
        Ok(client)
    }

    pub(crate) fn client_key(&self, server_match: &ServerMatch) -> ClientKey {
        ClientKey {
            server_id: server_match.server.id.clone(),
            workspace_root: server_match.workspace_root.clone(),
        }
    }

    async fn retry_after_seconds(&self, key: &ClientKey) -> Option<u64> {
        let health = self.client_health(key).await?;
        let retry_at = health.retry_at?;
        retry_at
            .checked_duration_since(Instant::now())
            .map(|duration| duration.as_secs())
    }

    async fn client_health(&self, key: &ClientKey) -> Option<ClientHealth> {
        self.client_health.read().await.get(key).cloned()
    }

    async fn is_permanently_broken(&self, key: &ClientKey) -> bool {
        self.client_health(key)
            .await
            .map(|health| health.permanent_broken)
            .unwrap_or(false)
    }

    async fn client_lock(&self, key: &ClientKey) -> Arc<Mutex<()>> {
        let mut locks = self.client_locks.lock().await;
        locks
            .entry(key.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    async fn reconcile_client_health(&self, key: &ClientKey) {
        let Some(client) = self.clients.read().await.get(key).cloned() else {
            return;
        };
        if client.is_running() {
            return;
        }

        self.mark_client_broken_and_restart(key.clone(), "LSP connection closed".to_string())
            .await;
    }

    async fn mark_client_starting(&self, server_id: &str, workspace_root: &Path) {
        let key = ClientKey {
            server_id: server_id.to_string(),
            workspace_root: workspace_root.to_path_buf(),
        };
        let mut health = self.client_health.write().await;
        let entry = health.entry(key).or_default();
        entry.state = LspClientState::Starting;
        entry.retry_at = None;
    }

    async fn mark_client_success(&self, server_id: &str, workspace_root: &Path) {
        let key = ClientKey {
            server_id: server_id.to_string(),
            workspace_root: workspace_root.to_path_buf(),
        };
        let mut health = self.client_health.write().await;
        let entry = health.entry(key).or_default();
        entry.state = LspClientState::Connected;
        entry.failure_count = 0;
        entry.retry_at = None;
        entry.last_error = None;
        entry.last_success_at = Some(SystemTime::now());
        entry.restart_in_progress = false;
        entry.permanent_broken = false;
    }

    #[cfg(test)]
    pub(crate) async fn mark_client_broken(&self, key: ClientKey, error: String) {
        let tracked_documents = if let Some(client) = self.clients.read().await.get(&key).cloned() {
            client.tracked_documents().await
        } else {
            self.client_health(&key)
                .await
                .map(|health| health.tracked_documents)
                .unwrap_or_default()
        };

        if let Some(client) = self.clients.write().await.remove(&key) {
            drop(client);
        }

        let mut health = self.client_health.write().await;
        let entry = health.entry(key).or_default();
        entry.failure_count = entry.failure_count.saturating_add(1);
        entry.state = LspClientState::Broken;
        entry.last_error = Some(error);
        entry.retry_at = Some(Instant::now() + backoff_for_failure(entry.failure_count));
        entry.tracked_documents = tracked_documents;
    }

    pub(crate) async fn mark_client_broken_and_restart(&self, key: ClientKey, error: String) {
        let tracked_documents = if let Some(client) = self.clients.read().await.get(&key).cloned() {
            client.tracked_documents().await
        } else {
            self.client_health(&key)
                .await
                .map(|health| health.tracked_documents)
                .unwrap_or_default()
        };

        if let Some(client) = self.clients.write().await.remove(&key) {
            drop(client);
        }

        let mut should_restart = false;
        {
            let mut health = self.client_health.write().await;
            let entry = health.entry(key.clone()).or_default();
            entry.state = LspClientState::Broken;
            entry.last_error = Some(error);
            entry.retry_at = None;
            entry.tracked_documents = tracked_documents;
            if !entry.permanent_broken && !entry.restart_in_progress {
                entry.restart_in_progress = true;
                should_restart = true;
            }
        }

        if should_restart {
            let manager = self.clone();
            tokio::spawn(async move {
                manager.restart_client(key).await;
            });
        }
    }

    async fn restart_client(&self, key: ClientKey) {
        let Some(server) = self
            .config
            .as_ref()
            .and_then(|config| {
                config
                    .servers
                    .iter()
                    .find(|server| server.id == key.server_id)
            })
            .cloned()
        else {
            let mut health = self.client_health.write().await;
            let entry = health.entry(key).or_default();
            entry.state = LspClientState::Broken;
            entry.last_error = Some("missing LSP server configuration".to_string());
            entry.retry_at = None;
            entry.restart_in_progress = false;
            entry.permanent_broken = true;
            return;
        };

        let client_lock = self.client_lock(&key).await;
        let _guard = client_lock.lock().await;

        if self.clients.read().await.contains_key(&key) {
            let mut health = self.client_health.write().await;
            let entry = health.entry(key).or_default();
            entry.restart_in_progress = false;
            return;
        }

        let tracked_documents = self
            .client_health(&key)
            .await
            .map(|health| health.tracked_documents)
            .unwrap_or_default();
        let mut last_error = None;

        for (attempt, delay) in self.restart_backoff.iter().copied().enumerate() {
            {
                let mut health = self.client_health.write().await;
                let entry = health.entry(key.clone()).or_default();
                entry.state = LspClientState::Starting;
                entry.failure_count = attempt as u32 + 1;
                entry.retry_at = Some(Instant::now() + delay);
            }

            tokio::time::sleep(delay).await;

            let client = match (self.spawn_client)(server.clone(), key.workspace_root.clone()).await
            {
                Ok(client) => client,
                Err(err) => {
                    last_error = Some(err.to_string());
                    continue;
                }
            };

            let reopen_result = async {
                for file_path in &tracked_documents {
                    client.open_or_change(file_path).await?;
                }
                Ok::<(), anyhow::Error>(())
            }
            .await;
            if let Err(err) = reopen_result {
                last_error = Some(err.to_string());
                drop(client);
                continue;
            }

            self.clients.write().await.insert(key.clone(), client);
            self.mark_client_success(&server.id, &key.workspace_root)
                .await;
            return;
        }

        warn!(
            "LSP server {} in {} is permanently broken after {} restart attempts",
            key.server_id,
            key.workspace_root.display(),
            self.restart_backoff.len()
        );

        let mut health = self.client_health.write().await;
        let entry = health.entry(key).or_default();
        entry.state = LspClientState::Broken;
        entry.failure_count = self.restart_backoff.len() as u32;
        entry.last_error = last_error;
        entry.retry_at = None;
        entry.restart_in_progress = false;
        entry.permanent_broken = true;
    }

    pub(crate) async fn matching_server_matches(
        &self,
        file_path: &Path,
        base_dir: &Path,
    ) -> Vec<ServerMatch> {
        let Some(config) = self.config.as_ref() else {
            return Vec::new();
        };
        let file_path = resolve_absolute_path(base_dir, file_path);
        let extension = file_path
            .extension()
            .map(|ext| format!(".{}", ext.to_string_lossy()))
            .unwrap_or_default();

        let mut matches = Vec::new();
        for server in &config.servers {
            if !server
                .extensions
                .iter()
                .any(|candidate| candidate == &extension)
            {
                continue;
            }
            if resolve_command(&server.command).is_err() {
                continue;
            }

            matches.push(ServerMatch {
                file_path: file_path.clone(),
                workspace_root: self
                    .workspace_root_for_file(&file_path, &server.root_markers, base_dir)
                    .await,
                server: server.clone(),
            });
        }

        matches.sort_by(|a, b| a.server.id.cmp(&b.server.id));
        matches
    }

    async fn active_workspace_matches(&self, base_dir: &Path) -> Vec<ServerMatch> {
        let Some(config) = self.config.as_ref() else {
            return Vec::new();
        };

        let active_keys = self
            .clients
            .read()
            .await
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        let mut matches = Vec::new();
        for key in active_keys {
            if !workspace_roots_overlap(&key.workspace_root, base_dir) {
                continue;
            }
            self.reconcile_client_health(&key).await;
            let Some(server) = config
                .servers
                .iter()
                .find(|server| server.id == key.server_id)
            else {
                continue;
            };

            matches.push(ServerMatch {
                file_path: base_dir.to_path_buf(),
                workspace_root: key.workspace_root,
                server: server.clone(),
            });
        }

        matches.sort_by(|a, b| a.server.id.cmp(&b.server.id));
        matches
    }

    async fn workspace_root_for_file(
        &self,
        file_path: &Path,
        root_markers: &[String],
        base_dir: &Path,
    ) -> PathBuf {
        let directory = if file_path.is_dir() {
            file_path.to_path_buf()
        } else {
            file_path.parent().unwrap_or(file_path).to_path_buf()
        };
        let key = WorkspaceRootCacheKey {
            directory,
            workspace_boundary: base_dir.to_path_buf(),
            root_markers: root_markers.to_vec(),
        };
        if let Some(workspace_root) = self.workspace_root_cache.read().await.get(&key).cloned() {
            return workspace_root;
        }

        let workspace_root = resolve_workspace_root(file_path, root_markers, base_dir);
        self.workspace_root_cache
            .write()
            .await
            .insert(key, workspace_root.clone());
        workspace_root
    }
}

impl Drop for SessionManager {
    fn drop(&mut self) {
        if Arc::strong_count(&self.drop_guard) > 1 {
            return;
        }

        let clients = Arc::clone(&self.clients);
        let client_health = Arc::clone(&self.client_health);
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let drained = clients
                    .write()
                    .await
                    .drain()
                    .collect::<Vec<(ClientKey, Arc<ClientHandle>)>>();
                for (key, client) in drained {
                    let mut health = client_health.write().await;
                    let entry = health.entry(key).or_default();
                    entry.state = LspClientState::Closed;
                    entry.retry_at = None;
                    entry.last_error = None;
                    drop(health);
                    client.shutdown().await;
                }
            });
            return;
        }

        let drained = self
            .clients
            .try_write()
            .ok()
            .map(|mut clients| {
                clients
                    .drain()
                    .map(|(_, client)| client)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for client in drained {
            if let Some(child) = client.child.as_ref()
                && let Ok(mut child) = child.try_lock()
            {
                let _ = child.start_kill();
            }
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ServerMatch {
    pub(crate) file_path: PathBuf,
    pub(crate) workspace_root: PathBuf,
    pub(crate) server: ServerConfig,
}

#[derive(Debug)]
struct RawOperationResult {
    server: String,
    workspace_root: PathBuf,
    payload: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ClientKey {
    pub(crate) server_id: String,
    pub(crate) workspace_root: PathBuf,
}

#[derive(Debug, Clone)]
pub(crate) struct ClientHealth {
    pub(crate) state: LspClientState,
    pub(crate) failure_count: u32,
    pub(crate) retry_at: Option<Instant>,
    pub(crate) last_error: Option<String>,
    pub(crate) last_success_at: Option<SystemTime>,
    pub(crate) tracked_documents: Vec<PathBuf>,
    pub(crate) restart_in_progress: bool,
    pub(crate) permanent_broken: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct WorkspaceRootCacheKey {
    directory: PathBuf,
    workspace_boundary: PathBuf,
    root_markers: Vec<String>,
}

impl Default for ClientHealth {
    fn default() -> Self {
        Self {
            state: LspClientState::Closed,
            failure_count: 0,
            retry_at: None,
            last_error: None,
            last_success_at: None,
            tracked_documents: Vec::new(),
            restart_in_progress: false,
            permanent_broken: false,
        }
    }
}
