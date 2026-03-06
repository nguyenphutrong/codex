use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Instant;
use std::time::SystemTime;
use tokio::fs;
use tokio::io::AsyncBufRead;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::process::Child;
use tokio::process::Command;
use tokio::sync::Mutex;
use tokio::sync::Notify;
use tokio::sync::RwLock;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::time::Duration;
use tokio::time::timeout;
use tracing::debug;
use tracing::warn;
use url::Url;

const DIAGNOSTICS_WAIT_TIMEOUT: Duration = Duration::from_secs(3);
const DIAGNOSTICS_SETTLE_DELAY: Duration = Duration::from_millis(150);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(45);
const CLIENT_BACKOFF_FIRST_FAILURE: Duration = Duration::from_secs(30);
const CLIENT_BACKOFF_SECOND_FAILURE: Duration = Duration::from_secs(120);
const CLIENT_BACKOFF_MAX_FAILURE: Duration = Duration::from_secs(600);
const CLIENT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspConfig {
    pub servers: Vec<ServerConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerConfig {
    pub id: String,
    pub command: String,
    pub args: Vec<String>,
    pub extensions: Vec<String>,
    pub env: HashMap<String, String>,
    pub initialization: Option<Value>,
    pub root_markers: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LspClientState {
    Starting,
    Connected,
    Broken,
    Closed,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct LspStatus {
    pub server: String,
    pub workspace_root: PathBuf,
    pub state: LspClientState,
    pub retry_after_seconds: Option<u64>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct LspDiagnostic {
    pub path: PathBuf,
    pub range: LspRange,
    pub severity: Option<u8>,
    pub message: String,
    pub source: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct LspRange {
    pub start: LspPosition,
    pub end: LspPosition,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct LspPosition {
    pub line: usize,
    pub character: usize,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct LspOperationResult {
    pub server: String,
    pub workspace_root: PathBuf,
    pub operation: String,
    pub items: Vec<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PositionRequest {
    pub file_path: PathBuf,
    pub line: usize,
    pub character: usize,
}

#[derive(Debug, Clone, Default)]
pub struct SessionManager {
    config: Option<Arc<LspConfig>>,
    clients: Arc<RwLock<HashMap<ClientKey, Arc<ClientHandle>>>>,
    client_health: Arc<RwLock<HashMap<ClientKey, ClientHealth>>>,
    client_locks: Arc<Mutex<HashMap<ClientKey, Arc<Mutex<()>>>>>,
}

impl SessionManager {
    pub fn new(config: Option<LspConfig>) -> Self {
        Self {
            config: config.map(Arc::new),
            clients: Arc::new(RwLock::new(HashMap::new())),
            client_health: Arc::new(RwLock::new(HashMap::new())),
            client_locks: Arc::new(Mutex::new(HashMap::new())),
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
                    self.mark_client_broken(key, err.to_string()).await;
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
                self.mark_client_broken(key, err.to_string()).await;
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
                    self.mark_client_broken(key, err.to_string()).await;
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
                    self.mark_client_broken(key, err.to_string()).await;
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
                self.mark_client_broken(key.clone(), err.to_string()).await;
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
                    self.mark_client_broken(key.clone(), err.to_string()).await;
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
                    self.mark_client_broken(key, err.to_string()).await;
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
                self.mark_client_broken(key.clone(), err.to_string()).await;
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
                    self.mark_client_broken(key, err.to_string()).await;
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

    async fn client_for_match(&self, server_match: &ServerMatch) -> Result<Arc<ClientHandle>> {
        let key = self.client_key(server_match);
        self.reconcile_client_health(&key).await;
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

        let client = match ClientHandle::spawn(
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
                self.mark_client_broken(key.clone(), err.to_string()).await;
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

    fn client_key(&self, server_match: &ServerMatch) -> ClientKey {
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

    async fn is_retry_blocked(&self, key: &ClientKey) -> bool {
        self.retry_after_seconds(key).await.is_some()
    }

    async fn client_health(&self, key: &ClientKey) -> Option<ClientHealth> {
        self.client_health.read().await.get(key).cloned()
    }

    async fn client_lock(&self, key: &ClientKey) -> Arc<Mutex<()>> {
        let mut locks = self.client_locks.lock().await;
        locks.entry(key.clone())
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

        self.mark_client_broken(key.clone(), "LSP connection closed".to_string())
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
    }

    async fn mark_client_closed(&self, key: &ClientKey) {
        let mut health = self.client_health.write().await;
        let entry = health.entry(key.clone()).or_default();
        entry.state = LspClientState::Closed;
        entry.retry_at = None;
        entry.last_error = None;
    }

    async fn mark_client_broken(&self, key: ClientKey, error: String) {
        if let Some(client) = self.clients.write().await.remove(&key) {
            drop(client);
        }

        let mut health = self.client_health.write().await;
        let entry = health.entry(key).or_default();
        entry.failure_count = entry.failure_count.saturating_add(1);
        entry.state = LspClientState::Broken;
        entry.last_error = Some(error);
        entry.retry_at = Some(Instant::now() + backoff_for_failure(entry.failure_count));
    }

    async fn matching_server_matches(&self, file_path: &Path, base_dir: &Path) -> Vec<ServerMatch> {
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
                workspace_root: resolve_workspace_root(&file_path, &server.root_markers, base_dir),
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

    async fn shutdown(&self) {
        let clients = self
            .clients
            .write()
            .await
            .drain()
            .collect::<Vec<(ClientKey, Arc<ClientHandle>)>>();
        for (key, client) in clients {
            self.mark_client_closed(&key).await;
            client.shutdown().await;
        }
    }
}

#[derive(Debug, Clone)]
struct ServerMatch {
    file_path: PathBuf,
    workspace_root: PathBuf,
    server: ServerConfig,
}

#[derive(Debug)]
struct RawOperationResult {
    server: String,
    workspace_root: PathBuf,
    payload: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ClientKey {
    server_id: String,
    workspace_root: PathBuf,
}

#[derive(Debug, Clone)]
struct ClientHealth {
    state: LspClientState,
    failure_count: u32,
    retry_at: Option<Instant>,
    last_error: Option<String>,
    last_success_at: Option<SystemTime>,
}

impl Default for ClientHealth {
    fn default() -> Self {
        Self {
            state: LspClientState::Closed,
            failure_count: 0,
            retry_at: None,
            last_error: None,
            last_success_at: None,
        }
    }
}

#[derive(Debug)]
struct ClientHandle {
    workspace_root: PathBuf,
    initialization: Option<Value>,
    writer_tx: mpsc::UnboundedSender<Value>,
    state: Arc<ClientState>,
    child: Option<Arc<Mutex<Child>>>,
}

#[derive(Debug)]
struct ClientState {
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>,
    diagnostics: RwLock<HashMap<PathBuf, CachedDiagnostics>>,
    opened_versions: Mutex<HashMap<PathBuf, i32>>,
    sync_capabilities: RwLock<TextDocumentSyncCapabilities>,
    next_request_id: AtomicU64,
    next_state_revision: AtomicU64,
    diagnostics_notify: Notify,
}

#[derive(Debug, Clone, Default)]
struct CachedDiagnostics {
    diagnostics: Vec<LspDiagnostic>,
    last_touch_revision: u64,
    last_publish_revision: u64,
    pending_stale: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TextDocumentChangeKind {
    None,
    Full,
    Incremental,
}

#[derive(Debug, Clone, Copy)]
struct TextDocumentSaveCapabilities {
    supported: bool,
    include_text: bool,
}

impl Default for TextDocumentSaveCapabilities {
    fn default() -> Self {
        Self {
            supported: false,
            include_text: false,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct TextDocumentSyncCapabilities {
    open_close: bool,
    change: TextDocumentChangeKind,
    save: TextDocumentSaveCapabilities,
}

impl Default for TextDocumentSyncCapabilities {
    fn default() -> Self {
        Self {
            open_close: true,
            change: TextDocumentChangeKind::Full,
            save: TextDocumentSaveCapabilities::default(),
        }
    }
}

impl ClientHandle {
    async fn spawn(server: ServerConfig, workspace_root: PathBuf) -> Result<Arc<Self>> {
        let command_path = resolve_command(&server.command)?;
        let mut command = Command::new(command_path);
        command
            .args(&server.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .current_dir(&workspace_root);
        for (key, value) in &server.env {
            command.env(key, value);
        }

        let mut child = command.spawn().context("failed to spawn LSP process")?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("LSP process did not expose stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("LSP process did not expose stdout"))?;

        let client = Self::from_streams(
            server.id,
            workspace_root,
            server.initialization,
            stdin,
            stdout,
            Some(child),
        )
        .await?;
        client.initialize().await?;
        Ok(client)
    }

    async fn from_streams<W, R>(
        _server_id: String,
        workspace_root: PathBuf,
        initialization: Option<Value>,
        writer: W,
        reader: R,
        child: Option<Child>,
    ) -> Result<Arc<Self>>
    where
        W: AsyncWrite + Unpin + Send + 'static,
        R: AsyncRead + Unpin + Send + 'static,
    {
        let (writer_tx, mut writer_rx) = mpsc::unbounded_channel::<Value>();
        let state = Arc::new(ClientState {
            pending: Mutex::new(HashMap::new()),
            diagnostics: RwLock::new(HashMap::new()),
            opened_versions: Mutex::new(HashMap::new()),
            sync_capabilities: RwLock::new(TextDocumentSyncCapabilities::default()),
            next_request_id: AtomicU64::new(0),
            next_state_revision: AtomicU64::new(0),
            diagnostics_notify: Notify::new(),
        });

        tokio::spawn(async move {
            let mut writer = writer;
            while let Some(message) = writer_rx.recv().await {
                if let Err(err) = write_lsp_message(&mut writer, &message).await {
                    debug!("LSP writer exited: {err:#}");
                    break;
                }
            }
        });

        let client = Arc::new(Self {
            workspace_root,
            initialization,
            writer_tx,
            state,
            child: child.map(|child| Arc::new(Mutex::new(child))),
        });

        let reader_client = client.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(reader);
            loop {
                match read_lsp_message(&mut reader).await {
                    Ok(Some(message)) => {
                        if let Err(err) = reader_client.handle_message(message).await {
                            debug!("LSP reader handler exited: {err:#}");
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(err) => {
                        debug!("LSP reader exited: {err:#}");
                        break;
                    }
                }
            }
            reader_client.fail_pending("LSP connection closed").await;
        });

        Ok(client)
    }

    async fn initialize(&self) -> Result<()> {
        let root_uri = path_to_uri(&self.workspace_root)?;
        let initialize_result = timeout(
            INITIALIZE_TIMEOUT,
            self.send_request(
                "initialize",
                json!({
                    "processId": std::process::id(),
                    "rootUri": root_uri,
                    "workspaceFolders": [{
                        "name": "workspace",
                        "uri": path_to_uri(&self.workspace_root)?,
                    }],
                    "initializationOptions": self.initialization.clone(),
                    "capabilities": {
                        "window": {
                            "workDoneProgress": true,
                        },
                        "workspace": {
                            "configuration": true,
                            "didChangeWatchedFiles": {
                                "dynamicRegistration": true,
                            },
                        },
                        "textDocument": {
                            "synchronization": {
                                "didOpen": true,
                                "didChange": true,
                            },
                            "publishDiagnostics": {
                                "versionSupport": true,
                            },
                        },
                    },
                }),
            ),
        )
        .await
        .context("initialize timed out")??;
        self.set_sync_capabilities(parse_sync_capabilities(
            initialize_result.get("capabilities"),
        ))
        .await;

        self.send_notification("initialized", json!({})).await?;
        if let Some(initialization) = self.initialization.clone() {
            self.send_notification(
                "workspace/didChangeConfiguration",
                json!({
                    "settings": initialization,
                }),
            )
            .await?;
        }

        Ok(())
    }

    async fn open_or_change(&self, file_path: &Path) -> Result<u64> {
        let text = fs::read_to_string(file_path)
            .await
            .with_context(|| format!("failed to read {}", file_path.display()))?;
        let uri = path_to_uri(file_path)?;
        let touch_revision = self.mark_touch_pending(file_path).await;
        let sync_capabilities = *self.state.sync_capabilities.read().await;
        let mut opened_versions = self.state.opened_versions.lock().await;

        if let Some(version) = opened_versions.get_mut(file_path) {
            *version += 1;
            self.send_notification(
                "workspace/didChangeWatchedFiles",
                json!({
                    "changes": [{
                        "uri": uri,
                        "type": 2,
                    }],
                }),
            )
            .await?;
            if sync_capabilities.change != TextDocumentChangeKind::None {
                self.send_notification(
                    "textDocument/didChange",
                    json!({
                        "textDocument": {
                            "uri": path_to_uri(file_path)?,
                            "version": *version,
                        },
                        "contentChanges": [{
                            "text": text,
                        }],
                    }),
                )
                .await?;
            }
            if sync_capabilities.save.supported {
                self.send_did_save(file_path, &text, sync_capabilities.save.include_text)
                    .await?;
            }
            return Ok(touch_revision);
        }

        opened_versions.insert(file_path.to_path_buf(), 0);
        self.send_notification(
            "workspace/didChangeWatchedFiles",
            json!({
                "changes": [{
                    "uri": uri,
                    "type": 1,
                }],
            }),
        )
        .await?;
        if sync_capabilities.open_close {
            self.send_notification(
                "textDocument/didOpen",
                json!({
                    "textDocument": {
                        "uri": path_to_uri(file_path)?,
                        "languageId": language_id_for_path(file_path),
                        "version": 0,
                        "text": text,
                    },
                }),
            )
            .await?;
        }
        if sync_capabilities.save.supported {
            self.send_did_save(file_path, &text, sync_capabilities.save.include_text)
                .await?;
        }
        Ok(touch_revision)
    }

    async fn diagnostics_for_path(&self, file_path: &Path) -> Vec<LspDiagnostic> {
        self.state
            .diagnostics
            .read()
            .await
            .get(file_path)
            .filter(|cached| {
                !cached.pending_stale && cached.last_publish_revision > cached.last_touch_revision
            })
            .map(|cached| cached.diagnostics.clone())
            .unwrap_or_default()
    }

    async fn last_publish_revision(&self, file_path: &Path) -> u64 {
        self.state
            .diagnostics
            .read()
            .await
            .get(file_path)
            .map(|cached| cached.last_publish_revision)
            .unwrap_or_default()
    }

    async fn wait_for_diagnostics(&self, file_path: &Path, touch_revision: u64) -> Result<()> {
        let file_path = file_path.to_path_buf();
        timeout(DIAGNOSTICS_WAIT_TIMEOUT, async {
            let mut observed_revision = self.last_publish_revision(&file_path).await;
            loop {
                let cached = self
                    .state
                    .diagnostics
                    .read()
                    .await
                    .get(&file_path)
                    .cloned()
                    .unwrap_or_default();
                if cached.pending_stale || cached.last_publish_revision <= touch_revision {
                    self.state.diagnostics_notify.notified().await;
                    continue;
                }

                observed_revision = cached.last_publish_revision;
                tokio::time::sleep(DIAGNOSTICS_SETTLE_DELAY).await;
                if self.last_publish_revision(&file_path).await == observed_revision {
                    break;
                }
            }
        })
        .await
        .map_err(|_| anyhow!("timed out waiting for diagnostics"))?;
        Ok(())
    }

    async fn send_request(&self, method: &str, params: Value) -> Result<Value> {
        let request_id = self.state.next_request_id.fetch_add(1, Ordering::Relaxed) + 1;
        let (tx, rx) = oneshot::channel();
        self.state.pending.lock().await.insert(request_id, tx);

        self.writer_tx
            .send(json!({
                "jsonrpc": "2.0",
                "id": request_id,
                "method": method,
                "params": params,
            }))
            .map_err(|_| anyhow!("failed to send LSP request {method}"))?;

        timeout(REQUEST_TIMEOUT, rx)
            .await
            .map_err(|_| anyhow!("LSP request {method} timed out"))?
            .map_err(|_| anyhow!("LSP request {method} was canceled"))?
    }

    async fn send_notification(&self, method: &str, params: Value) -> Result<()> {
        self.writer_tx
            .send(json!({
                "jsonrpc": "2.0",
                "method": method,
                "params": params,
            }))
            .map_err(|_| anyhow!("failed to send LSP notification {method}"))?;
        Ok(())
    }

    async fn send_did_save(&self, file_path: &Path, text: &str, include_text: bool) -> Result<()> {
        let mut params = json!({
            "textDocument": {
                "uri": path_to_uri(file_path)?,
            },
        });
        if include_text {
            params["text"] = Value::String(text.to_string());
        }
        self.send_notification("textDocument/didSave", params).await
    }

    async fn handle_message(&self, message: Value) -> Result<()> {
        if message.get("method").is_some() && message.get("id").is_some() {
            self.handle_server_request(message).await?;
            return Ok(());
        }

        if let Some(method) = message.get("method").and_then(Value::as_str) {
            self.handle_notification(method, message.get("params"))
                .await?;
            return Ok(());
        }

        self.handle_response(message).await
    }

    async fn handle_server_request(&self, message: Value) -> Result<()> {
        let id = message
            .get("id")
            .cloned()
            .ok_or_else(|| anyhow!("server request missing id"))?;
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("server request missing method"))?;

        let result = match method {
            "window/workDoneProgress/create"
            | "client/registerCapability"
            | "client/unregisterCapability" => Value::Null,
            "workspace/configuration" => {
                json!([self.initialization.clone().unwrap_or(Value::Null)])
            }
            "workspace/workspaceFolders" => json!([{
                "name": "workspace",
                "uri": path_to_uri(&self.workspace_root)?,
            }]),
            _ => Value::Null,
        };

        self.writer_tx
            .send(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": result,
            }))
            .map_err(|_| anyhow!("failed to respond to LSP server request"))?;
        Ok(())
    }

    async fn handle_notification(&self, method: &str, params: Option<&Value>) -> Result<()> {
        if method != "textDocument/publishDiagnostics" {
            return Ok(());
        }

        let Some(params) = params else {
            return Ok(());
        };
        let Some(uri) = params.get("uri").and_then(Value::as_str) else {
            return Ok(());
        };
        let Ok(path) = uri_to_path(uri) else {
            return Ok(());
        };

        let diagnostics = params
            .get("diagnostics")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|diagnostic| diagnostic_from_value(&path, diagnostic))
            .collect::<Vec<_>>();

        let revision = self.next_state_revision();
        let mut cached = self.state.diagnostics.write().await;
        let entry = cached.entry(path).or_default();
        entry.diagnostics = diagnostics;
        entry.last_publish_revision = revision;
        entry.pending_stale = entry.last_publish_revision <= entry.last_touch_revision;
        self.state.diagnostics_notify.notify_waiters();
        Ok(())
    }

    async fn handle_response(&self, message: Value) -> Result<()> {
        let Some(id) = message.get("id").and_then(Value::as_u64) else {
            return Ok(());
        };

        let pending = self.state.pending.lock().await.remove(&id);
        let Some(pending) = pending else {
            return Ok(());
        };

        let result = if let Some(error) = message.get("error") {
            let error_message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown LSP error");
            Err(anyhow!("LSP request failed: {error_message}"))
        } else {
            Ok(message.get("result").cloned().unwrap_or(Value::Null))
        };

        let _ = pending.send(result);
        Ok(())
    }

    async fn fail_pending(&self, message: &str) {
        let mut pending = self.state.pending.lock().await;
        for (_, sender) in pending.drain() {
            let _ = sender.send(Err(anyhow!(message.to_string())));
        }
    }

    async fn set_sync_capabilities(&self, capabilities: TextDocumentSyncCapabilities) {
        *self.state.sync_capabilities.write().await = capabilities;
    }

    async fn mark_touch_pending(&self, file_path: &Path) -> u64 {
        let revision = self.next_state_revision();
        let mut diagnostics = self.state.diagnostics.write().await;
        let entry = diagnostics.entry(file_path.to_path_buf()).or_default();
        entry.last_touch_revision = revision;
        entry.pending_stale = true;
        revision
    }

    fn next_state_revision(&self) -> u64 {
        self.state.next_state_revision.fetch_add(1, Ordering::Relaxed) + 1
    }

    fn is_running(&self) -> bool {
        let Some(child) = &self.child else {
            return true;
        };
        let Ok(mut child) = child.try_lock() else {
            return true;
        };
        matches!(child.try_wait(), Ok(None))
    }

    async fn shutdown(&self) {
        let _ = timeout(REQUEST_TIMEOUT, self.send_request("shutdown", Value::Null)).await;
        let _ = self.send_notification("exit", Value::Null).await;
        let Some(child) = &self.child else {
            return;
        };
        let Ok(mut child) = timeout(CLIENT_SHUTDOWN_TIMEOUT, child.lock()).await else {
            return;
        };
        if timeout(CLIENT_SHUTDOWN_TIMEOUT, child.wait()).await.is_err() {
            let _ = child.start_kill();
        }
    }
}

impl Drop for SessionManager {
    fn drop(&mut self) {
        let manager = self.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                manager.shutdown().await;
            });
            return;
        }

        let clients = self
            .clients
            .try_write()
            .ok()
            .map(|mut clients| clients.drain().map(|(_, client)| client).collect::<Vec<_>>())
            .unwrap_or_default();
        for client in clients {
            if let Some(child) = client.child.as_ref()
                && let Ok(mut child) = child.try_lock()
            {
                let _ = child.start_kill();
            }
        }
    }
}

impl Drop for ClientHandle {
    fn drop(&mut self) {
        let Some(child) = self.child.clone() else {
            return;
        };
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let writer_tx = self.writer_tx.clone();
            handle.spawn(async move {
                let _ = writer_tx.send(json!({
                    "jsonrpc": "2.0",
                    "method": "exit",
                    "params": {},
                }));
                let Ok(mut child) = timeout(CLIENT_SHUTDOWN_TIMEOUT, child.lock()).await else {
                    return;
                };
                if timeout(CLIENT_SHUTDOWN_TIMEOUT, child.wait()).await.is_err() {
                    let _ = child.start_kill();
                }
            });
        } else if let Ok(mut child) = child.try_lock() {
            let _ = child.start_kill();
        }
    }
}

async fn write_lsp_message<W: AsyncWrite + Unpin>(writer: &mut W, message: &Value) -> Result<()> {
    let body = serde_json::to_vec(message)?;
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    writer.write_all(header.as_bytes()).await?;
    writer.write_all(&body).await?;
    writer.flush().await?;
    Ok(())
}

async fn read_lsp_message<R: AsyncBufRead + Unpin>(reader: &mut R) -> Result<Option<Value>> {
    let mut content_length = None;
    loop {
        let mut line = String::new();
        let bytes_read = reader.read_line(&mut line).await?;
        if bytes_read == 0 {
            return Ok(None);
        }

        if line == "\r\n" {
            break;
        }

        if let Some(value) = line.strip_prefix("Content-Length:") {
            let parsed = value.trim().parse::<usize>()?;
            content_length = Some(parsed);
        }
    }

    let content_length = content_length.ok_or_else(|| anyhow!("missing Content-Length header"))?;
    let mut body = vec![0u8; content_length];
    reader.read_exact(&mut body).await?;
    let message = serde_json::from_slice(&body)?;
    Ok(Some(message))
}

fn resolve_absolute_path(base_dir: &Path, file_path: &Path) -> PathBuf {
    if file_path.is_absolute() {
        return file_path.to_path_buf();
    }
    base_dir.join(file_path)
}

fn resolve_workspace_root(file_path: &Path, root_markers: &[String], base_dir: &Path) -> PathBuf {
    let start_dir = if file_path.is_dir() {
        file_path
    } else {
        file_path.parent().unwrap_or(file_path)
    };

    for ancestor in start_dir.ancestors() {
        if root_markers
            .iter()
            .any(|marker| ancestor.join(marker).exists())
        {
            return ancestor.to_path_buf();
        }
    }

    base_dir.to_path_buf()
}

fn workspace_roots_overlap(left: &Path, right: &Path) -> bool {
    left == right || left.starts_with(right) || right.starts_with(left)
}

fn backoff_for_failure(failure_count: u32) -> Duration {
    match failure_count {
        0 | 1 => CLIENT_BACKOFF_FIRST_FAILURE,
        2 => CLIENT_BACKOFF_SECOND_FAILURE,
        _ => CLIENT_BACKOFF_MAX_FAILURE,
    }
}

fn parse_sync_capabilities(capabilities: Option<&Value>) -> TextDocumentSyncCapabilities {
    let default = TextDocumentSyncCapabilities::default();
    let Some(sync) = capabilities.and_then(|capabilities| capabilities.get("textDocumentSync")) else {
        return default;
    };

    match sync {
        Value::Number(number) => TextDocumentSyncCapabilities {
            change: parse_change_kind(number.as_u64()),
            ..default
        },
        Value::Object(object) => TextDocumentSyncCapabilities {
            open_close: object
                .get("openClose")
                .and_then(Value::as_bool)
                .unwrap_or(default.open_close),
            change: parse_change_kind(object.get("change").and_then(Value::as_u64)),
            save: parse_save_capabilities(object.get("save")),
        },
        _ => default,
    }
}

fn parse_change_kind(change: Option<u64>) -> TextDocumentChangeKind {
    match change {
        Some(0) => TextDocumentChangeKind::None,
        Some(2) => TextDocumentChangeKind::Incremental,
        _ => TextDocumentChangeKind::Full,
    }
}

fn parse_save_capabilities(save: Option<&Value>) -> TextDocumentSaveCapabilities {
    match save {
        Some(Value::Bool(supported)) => TextDocumentSaveCapabilities {
            supported: *supported,
            include_text: false,
        },
        Some(Value::Object(save)) => TextDocumentSaveCapabilities {
            supported: true,
            include_text: save
                .get("includeText")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        },
        _ => TextDocumentSaveCapabilities::default(),
    }
}

fn resolve_command(command: &str) -> Result<PathBuf> {
    let path = PathBuf::from(command);
    if path.is_absolute() {
        return Ok(path);
    }
    which::which(command).with_context(|| format!("failed to resolve {command}"))
}

fn to_lsp_position(line: usize, character: usize) -> Value {
    json!({
        "line": line.saturating_sub(1),
        "character": character.saturating_sub(1),
    })
}

fn path_to_uri(path: &Path) -> Result<String> {
    Url::from_file_path(path)
        .map_err(|_| anyhow!("failed to convert {} to file URI", path.display()))
        .map(|url| url.to_string())
}

fn uri_to_path(uri: &str) -> Result<PathBuf> {
    Url::parse(uri)
        .context("failed to parse file URI")?
        .to_file_path()
        .map_err(|_| anyhow!("failed to convert URI {uri} to a path"))
}

fn language_id_for_path(path: &Path) -> &'static str {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("rs") => "rust",
        Some("ts") => "typescript",
        Some("tsx") => "typescriptreact",
        Some("js") => "javascript",
        Some("jsx") => "javascriptreact",
        Some("mts") => "typescript",
        Some("cts") => "typescript",
        Some("mjs") => "javascript",
        Some("cjs") => "javascript",
        Some("php") => "php",
        Some("py") | Some("pyi") => "python",
        Some("go") => "go",
        Some("c") => "c",
        Some("cc") | Some("cpp") | Some("cxx") | Some("hpp") | Some("hh") | Some("hxx") => "cpp",
        Some("h") => "c",
        Some("swift") => "swift",
        Some("m") => "objective-c",
        Some("mm") => "objective-cpp",
        _ => "plaintext",
    }
}

fn diagnostic_from_value(path: &Path, value: &Value) -> Option<LspDiagnostic> {
    Some(LspDiagnostic {
        path: path.to_path_buf(),
        range: range_from_value(value.get("range")?)?,
        severity: value
            .get("severity")
            .and_then(Value::as_u64)
            .and_then(|severity| u8::try_from(severity).ok()),
        message: value.get("message")?.as_str()?.to_string(),
        source: value
            .get("source")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

fn normalize_location_like(value: &Value) -> Vec<Value> {
    if value.is_null() {
        return Vec::new();
    }

    match value {
        Value::Array(items) => items
            .iter()
            .filter_map(normalize_location_or_link)
            .collect(),
        _ => normalize_location_or_link(value).into_iter().collect(),
    }
}

fn normalize_location_or_link(value: &Value) -> Option<Value> {
    if value.get("uri").is_some() {
        return Some(json!({
            "path": uri_to_path(value.get("uri")?.as_str()?).ok()?,
            "range": range_to_value(&range_from_value(value.get("range")?)?),
        }));
    }

    let range = value
        .get("targetSelectionRange")
        .or_else(|| value.get("targetRange"))?;
    Some(json!({
        "path": uri_to_path(value.get("targetUri")?.as_str()?).ok()?,
        "range": range_to_value(&range_from_value(range)?),
    }))
}

fn normalize_hover(value: &Value) -> Vec<Value> {
    if value.is_null() {
        return Vec::new();
    }

    let contents = flatten_hover_contents(value.get("contents"));
    if contents.is_empty() {
        return Vec::new();
    }

    let mut item = json!({
        "contents": contents,
    });
    if let Some(range) = value.get("range").and_then(range_from_value) {
        item["range"] = range_to_value(&range);
    }
    vec![item]
}

fn flatten_hover_contents(value: Option<&Value>) -> String {
    let Some(value) = value else {
        return String::new();
    };

    match value {
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .map(|item| flatten_hover_contents(Some(item)))
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n"),
        Value::Object(object) => object
            .get("value")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                object
                    .get("language")
                    .and_then(|_| serde_json::to_string(object).ok())
            })
            .unwrap_or_default(),
        _ => String::new(),
    }
}

fn normalize_symbol_information(value: &Value) -> Vec<Value> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| {
            Some(json!({
                "name": item.get("name")?.as_str()?,
                "kind": item.get("kind")?.as_i64()?,
                "detail": item.get("detail").and_then(Value::as_str),
                "container_name": item.get("containerName").and_then(Value::as_str),
                "location": item.get("location").and_then(normalize_location_or_link),
            }))
        })
        .collect()
}

fn normalize_document_symbols(value: &Value) -> Vec<Value> {
    let Some(items) = value.as_array() else {
        return Vec::new();
    };

    if items
        .first()
        .and_then(|item| item.get("location"))
        .is_some()
    {
        return normalize_symbol_information(value);
    }

    let mut output = Vec::new();
    for item in items {
        flatten_document_symbol(item, None, &mut output);
    }
    output
}

fn flatten_document_symbol(item: &Value, container_name: Option<&str>, output: &mut Vec<Value>) {
    let Some(name) = item.get("name").and_then(Value::as_str) else {
        return;
    };
    let Some(kind) = item.get("kind").and_then(Value::as_i64) else {
        return;
    };
    let Some(range) = item.get("range").and_then(range_from_value) else {
        return;
    };

    output.push(json!({
        "name": name,
        "kind": kind,
        "detail": item.get("detail").and_then(Value::as_str),
        "container_name": container_name,
        "location": {
            "range": range_to_value(&range),
        },
    }));

    if let Some(children) = item.get("children").and_then(Value::as_array) {
        for child in children {
            flatten_document_symbol(child, Some(name), output);
        }
    }
}

fn normalize_call_hierarchy_items(value: &Value) -> Vec<Value> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(call_hierarchy_item)
        .collect()
}

fn normalize_call_hierarchy_calls(value: &Value) -> Vec<Value> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let item = entry
                .get("from")
                .or_else(|| entry.get("to"))
                .and_then(call_hierarchy_item)?;
            let mut item = item;
            if let Some(ranges) = entry
                .get("fromRanges")
                .and_then(Value::as_array)
                .map(|ranges| {
                    ranges
                        .iter()
                        .filter_map(range_from_value)
                        .map(|range| range_to_value(&range))
                        .collect::<Vec<_>>()
                })
            {
                item["from_ranges"] = Value::Array(ranges);
            }
            if let Some(ranges) = entry
                .get("toRanges")
                .and_then(Value::as_array)
                .map(|ranges| {
                    ranges
                        .iter()
                        .filter_map(range_from_value)
                        .map(|range| range_to_value(&range))
                        .collect::<Vec<_>>()
                })
            {
                item["to_ranges"] = Value::Array(ranges);
            }
            Some(item)
        })
        .collect()
}

fn call_hierarchy_item(value: &Value) -> Option<Value> {
    Some(json!({
        "name": value.get("name")?.as_str()?,
        "kind": value.get("kind")?.as_i64()?,
        "path": uri_to_path(value.get("uri")?.as_str()?).ok()?,
        "selection_range": range_to_value(&range_from_value(value.get("selectionRange")?)?),
    }))
}

fn range_from_value(value: &Value) -> Option<LspRange> {
    Some(LspRange {
        start: position_from_value(value.get("start")?)?,
        end: position_from_value(value.get("end")?)?,
    })
}

fn position_from_value(value: &Value) -> Option<LspPosition> {
    let line = value.get("line")?.as_u64()?;
    let character = value.get("character")?.as_u64()?;
    Some(LspPosition {
        line: usize::try_from(line).ok()?.saturating_add(1),
        character: usize::try_from(character).ok()?.saturating_add(1),
    })
}

fn range_to_value(range: &LspRange) -> Value {
    json!({
        "start": {
            "line": range.start.line,
            "character": range.start.character,
        },
        "end": {
            "line": range.end.line,
            "character": range.end.character,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::collections::HashMap;
    use tempfile::tempdir;
    use tokio::io::AsyncWriteExt;
    use tokio::io::duplex;

    #[tokio::test]
    async fn read_and_write_lsp_messages_round_trip() -> Result<()> {
        let (mut writer, mut reader) = duplex(4096);
        let payload = json!({
            "jsonrpc": "2.0",
            "method": "test",
            "params": {
                "ok": true,
            },
        });
        let payload_for_writer = payload.clone();
        tokio::spawn(async move {
            let _ = write_lsp_message(&mut writer, &payload_for_writer).await;
            let _ = writer.shutdown().await;
        });
        let round_trip = read_lsp_message(&mut BufReader::new(&mut reader))
            .await?
            .expect("message");
        assert_eq!(round_trip, payload);
        Ok(())
    }

    #[tokio::test]
    async fn diagnostics_are_cached_from_publish_notifications() -> Result<()> {
        let tmp = tempdir()?;
        let file_path = tmp.path().join("main.rs");
        fs::write(&file_path, "fn main() {}\n").await?;

        let (client_stream, server_stream) = duplex(16 * 1024);
        let (client_reader, client_writer) = tokio::io::split(client_stream);
        let (mut server_reader, mut server_writer) = tokio::io::split(server_stream);
        let client = ClientHandle::from_streams(
            "fake".to_string(),
            tmp.path().to_path_buf(),
            None,
            client_writer,
            client_reader,
            None,
        )
        .await?;

        let file_path_for_server = file_path.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(&mut server_reader);
            let message = read_lsp_message(&mut reader).await.expect("initialize");
            let request = message.expect("initialize request");
            let id = request.get("id").cloned().expect("request id");
            write_lsp_message(
                &mut server_writer,
                &json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "capabilities": {},
                    },
                }),
            )
            .await
            .expect("write initialize response");
            let _ = read_lsp_message(&mut reader).await.expect("initialized");
            let uri = path_to_uri(&file_path_for_server).expect("file uri");
            write_lsp_message(
                &mut server_writer,
                &json!({
                    "jsonrpc": "2.0",
                    "method": "textDocument/publishDiagnostics",
                    "params": {
                        "uri": uri,
                        "diagnostics": [{
                            "range": {
                                "start": { "line": 0, "character": 3 },
                                "end": { "line": 0, "character": 7 }
                            },
                            "severity": 1,
                            "message": "boom",
                            "source": "fake",
                        }],
                    },
                }),
            )
            .await
            .expect("write diagnostics");
        });

        client.initialize().await?;
        client.open_or_change(&file_path).await?;
        client.wait_for_diagnostics(&file_path, 0).await?;
        let diagnostics = client.diagnostics_for_path(&file_path).await;
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].message, "boom");
        assert_eq!(diagnostics[0].range.start.line, 1);
        assert_eq!(diagnostics[0].range.start.character, 4);
        Ok(())
    }

    #[tokio::test]
    async fn wait_for_diagnostics_waits_for_settled_publish_notifications() -> Result<()> {
        let tmp = tempdir()?;
        let file_path = tmp.path().join("main.rs");
        fs::write(&file_path, "fn main() {}\n").await?;

        let (client_stream, server_stream) = duplex(16 * 1024);
        let (client_reader, client_writer) = tokio::io::split(client_stream);
        let (mut server_reader, mut server_writer) = tokio::io::split(server_stream);
        let client = ClientHandle::from_streams(
            "fake".to_string(),
            tmp.path().to_path_buf(),
            None,
            client_writer,
            client_reader,
            None,
        )
        .await?;

        let file_path_for_server = file_path.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(&mut server_reader);
            let message = read_lsp_message(&mut reader).await.expect("initialize");
            let request = message.expect("initialize request");
            let id = request.get("id").cloned().expect("request id");
            write_lsp_message(
                &mut server_writer,
                &json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "capabilities": {},
                    },
                }),
            )
            .await
            .expect("write initialize response");
            let _ = read_lsp_message(&mut reader).await.expect("initialized");
            while let Ok(Some(message)) = read_lsp_message(&mut reader).await {
                if message.get("method").and_then(Value::as_str) != Some("textDocument/didOpen") {
                    continue;
                }
                let uri = path_to_uri(&file_path_for_server).expect("file uri");
                write_lsp_message(
                    &mut server_writer,
                    &json!({
                        "jsonrpc": "2.0",
                        "method": "textDocument/publishDiagnostics",
                        "params": {
                            "uri": uri,
                            "diagnostics": [{
                                "range": {
                                    "start": { "line": 0, "character": 0 },
                                    "end": { "line": 0, "character": 4 }
                                },
                                "severity": 1,
                                "message": "syntax",
                            }],
                        },
                    }),
                )
                .await
                .expect("write syntax diagnostics");
                tokio::time::sleep(Duration::from_millis(50)).await;
                write_lsp_message(
                    &mut server_writer,
                    &json!({
                        "jsonrpc": "2.0",
                        "method": "textDocument/publishDiagnostics",
                        "params": {
                            "uri": path_to_uri(&file_path_for_server).expect("file uri"),
                            "diagnostics": [{
                                "range": {
                                    "start": { "line": 0, "character": 5 },
                                    "end": { "line": 0, "character": 9 }
                                },
                                "severity": 1,
                                "message": "semantic",
                            }],
                        },
                    }),
                )
                .await
                .expect("write semantic diagnostics");
                break;
            }
        });

        client.initialize().await?;
        client.open_or_change(&file_path).await?;
        client.wait_for_diagnostics(&file_path, 0).await?;
        let diagnostics = client.diagnostics_for_path(&file_path).await;
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].message, "semantic");
        assert_eq!(diagnostics[0].range.start.character, 6);
        Ok(())
    }

    #[test]
    fn normalize_locations_supports_location_links() {
        let items = normalize_location_like(&json!([{
            "targetUri": "file:///tmp/example.rs",
            "targetRange": {
                "start": { "line": 0, "character": 1 },
                "end": { "line": 0, "character": 4 }
            }
        }]));
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0]["path"],
            Value::String("/tmp/example.rs".to_string())
        );
    }

    #[test]
    fn flatten_document_symbols_adds_container_name() {
        let items = normalize_document_symbols(&json!([{
            "name": "Parent",
            "kind": 5,
            "range": {
                "start": { "line": 0, "character": 0 },
                "end": { "line": 5, "character": 0 }
            },
            "children": [{
                "name": "child",
                "kind": 12,
                "range": {
                    "start": { "line": 1, "character": 0 },
                    "end": { "line": 1, "character": 4 }
                }
            }]
        }]));
        assert_eq!(items.len(), 2);
        assert_eq!(
            items[1]["container_name"],
            Value::String("Parent".to_string())
        );
    }

    #[tokio::test]
    async fn client_for_match_marks_spawn_failures_as_broken() -> Result<()> {
        let tmp = tempdir()?;
        let file_path = tmp.path().join("main.rs");
        fs::write(&file_path, "fn main() {}\n").await?;

        let manager = SessionManager::new(Some(LspConfig {
            servers: vec![ServerConfig {
                id: "broken".to_string(),
                command: tmp.path().to_string_lossy().into_owned(),
                args: Vec::new(),
                extensions: vec![".rs".to_string()],
                env: HashMap::new(),
                initialization: None,
                root_markers: Vec::new(),
            }],
        }));

        let server_match = manager
            .matching_server_matches(&file_path, tmp.path())
            .await
            .into_iter()
            .next()
            .expect("server match");
        let key = manager.client_key(&server_match);

        manager
            .client_for_match(&server_match)
            .await
            .expect_err("spawn failure");
        assert!(manager.is_broken_client(&key).await);

        let retry_error = manager
            .client_for_match(&server_match)
            .await
            .expect_err("broken client");
        assert!(retry_error.to_string().contains("temporarily unavailable"));
        Ok(())
    }

    #[tokio::test]
    async fn client_for_match_marks_initialize_failures_as_broken() -> Result<()> {
        let tmp = tempdir()?;
        let file_path = tmp.path().join("main.rs");
        fs::write(&file_path, "fn main() {}\n").await?;

        let manager = SessionManager::new(Some(LspConfig {
            servers: vec![ServerConfig {
                id: "broken".to_string(),
                command: "true".to_string(),
                args: Vec::new(),
                extensions: vec![".rs".to_string()],
                env: HashMap::new(),
                initialization: None,
                root_markers: Vec::new(),
            }],
        }));

        let server_match = manager
            .matching_server_matches(&file_path, tmp.path())
            .await
            .into_iter()
            .next()
            .expect("server match");
        let key = manager.client_key(&server_match);

        manager
            .client_for_match(&server_match)
            .await
            .expect_err("initialize failure");
        assert!(manager.is_broken_client(&key).await);
        Ok(())
    }

    #[tokio::test]
    async fn broken_client_entries_expire_after_ttl() -> Result<()> {
        let manager = SessionManager::default();
        let key = ClientKey {
            server_id: "server".to_string(),
            workspace_root: PathBuf::from("/tmp/workspace"),
        };
        manager.broken_clients.lock().await.insert(
            key.clone(),
            Instant::now() - BROKEN_CLIENT_TTL - Duration::from_secs(1),
        );

        assert!(!manager.is_broken_client(&key).await);
        assert!(!manager.broken_clients.lock().await.contains_key(&key));
        Ok(())
    }

    #[tokio::test]
    async fn workspace_symbol_requires_active_client_or_file_scope() -> Result<()> {
        let tmp = tempdir()?;
        let manager = SessionManager::new(Some(LspConfig {
            servers: vec![ServerConfig {
                id: "rust".to_string(),
                command: "true".to_string(),
                args: Vec::new(),
                extensions: vec![".rs".to_string()],
                env: HashMap::new(),
                initialization: None,
                root_markers: Vec::new(),
            }],
        }));

        let err = manager
            .workspace_symbol("query", tmp.path())
            .await
            .expect_err("workspace symbol without active clients");
        assert!(
            err.to_string()
                .contains("No active LSP client available for this workspace")
        );
        assert!(manager.broken_clients.lock().await.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn status_for_file_reports_connected_and_broken_state() -> Result<()> {
        let tmp = tempdir()?;
        let file_path = tmp.path().join("main.rs");
        fs::write(&file_path, "fn main() {}\n").await?;
        let workspace_root = tmp.path().to_path_buf();
        let connected_server = ServerConfig {
            id: "connected".to_string(),
            command: "true".to_string(),
            args: Vec::new(),
            extensions: vec![".rs".to_string()],
            env: HashMap::new(),
            initialization: None,
            root_markers: Vec::new(),
        };
        let broken_server = ServerConfig {
            id: "broken".to_string(),
            command: "true".to_string(),
            args: Vec::new(),
            extensions: vec![".rs".to_string()],
            env: HashMap::new(),
            initialization: None,
            root_markers: Vec::new(),
        };
        let manager = SessionManager::new(Some(LspConfig {
            servers: vec![broken_server.clone(), connected_server.clone()],
        }));

        let (stream_a, stream_b) = duplex(1024);
        let (reader, writer) = tokio::io::split(stream_a);
        let _unused = stream_b;
        let client = ClientHandle::from_streams(
            connected_server.id.clone(),
            workspace_root.clone(),
            None,
            writer,
            reader,
            None,
        )
        .await?;
        manager.clients.write().await.insert(
            ClientKey {
                server_id: connected_server.id.clone(),
                workspace_root: workspace_root.clone(),
            },
            client,
        );
        manager.broken_clients.lock().await.insert(
            ClientKey {
                server_id: broken_server.id.clone(),
                workspace_root: workspace_root.clone(),
            },
            Instant::now(),
        );

        let statuses = manager.status_for_file(&file_path, tmp.path()).await;
        assert_eq!(statuses.len(), 2);
        assert_eq!(
            statuses,
            vec![
                LspStatus {
                    server: "broken".to_string(),
                    workspace_root: workspace_root.clone(),
                    connected: false,
                    broken: true,
                },
                LspStatus {
                    server: "connected".to_string(),
                    workspace_root,
                    connected: true,
                    broken: false,
                },
            ]
        );
        Ok(())
    }

    #[tokio::test]
    async fn definition_best_effort_returns_healthy_server_results() -> Result<()> {
        let tmp = tempdir()?;
        let file_path = tmp.path().join("main.rs");
        fs::write(&file_path, "fn main() {}\n").await?;

        let broken_server = ServerConfig {
            id: "broken".to_string(),
            command: "true".to_string(),
            args: Vec::new(),
            extensions: vec![".rs".to_string()],
            env: HashMap::new(),
            initialization: None,
            root_markers: Vec::new(),
        };
        let healthy_server = ServerConfig {
            id: "healthy".to_string(),
            command: "true".to_string(),
            args: Vec::new(),
            extensions: vec![".rs".to_string()],
            env: HashMap::new(),
            initialization: None,
            root_markers: Vec::new(),
        };
        let manager = SessionManager::new(Some(LspConfig {
            servers: vec![broken_server.clone(), healthy_server.clone()],
        }));

        let (broken_client_stream, broken_server_stream) = duplex(16 * 1024);
        let (broken_client_reader, broken_client_writer) = tokio::io::split(broken_client_stream);
        let (mut broken_server_reader, mut broken_server_writer) =
            tokio::io::split(broken_server_stream);
        let broken_client = ClientHandle::from_streams(
            broken_server.id.clone(),
            tmp.path().to_path_buf(),
            None,
            broken_client_writer,
            broken_client_reader,
            None,
        )
        .await?;

        tokio::spawn(async move {
            let mut reader = BufReader::new(&mut broken_server_reader);
            while let Ok(Some(message)) = read_lsp_message(&mut reader).await {
                let Some(id) = message.get("id").cloned() else {
                    continue;
                };
                write_lsp_message(
                    &mut broken_server_writer,
                    &json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {
                            "message": "boom",
                        },
                    }),
                )
                .await
                .expect("write broken response");
            }
        });

        let (healthy_client_stream, healthy_server_stream) = duplex(16 * 1024);
        let (healthy_client_reader, healthy_client_writer) =
            tokio::io::split(healthy_client_stream);
        let (mut healthy_server_reader, mut healthy_server_writer) =
            tokio::io::split(healthy_server_stream);
        let healthy_client = ClientHandle::from_streams(
            healthy_server.id.clone(),
            tmp.path().to_path_buf(),
            None,
            healthy_client_writer,
            healthy_client_reader,
            None,
        )
        .await?;

        let file_uri = path_to_uri(&file_path)?;
        tokio::spawn(async move {
            let mut reader = BufReader::new(&mut healthy_server_reader);
            while let Ok(Some(message)) = read_lsp_message(&mut reader).await {
                let Some(id) = message.get("id").cloned() else {
                    continue;
                };
                write_lsp_message(
                    &mut healthy_server_writer,
                    &json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": [{
                            "uri": file_uri,
                            "range": {
                                "start": { "line": 0, "character": 0 },
                                "end": { "line": 0, "character": 4 }
                            }
                        }],
                    }),
                )
                .await
                .expect("write healthy response");
            }
        });

        let workspace_root = tmp.path().to_path_buf();
        manager.clients.write().await.insert(
            ClientKey {
                server_id: broken_server.id.clone(),
                workspace_root: workspace_root.clone(),
            },
            broken_client,
        );
        manager.clients.write().await.insert(
            ClientKey {
                server_id: healthy_server.id.clone(),
                workspace_root: workspace_root.clone(),
            },
            healthy_client,
        );

        let request = PositionRequest {
            file_path: file_path.clone(),
            line: 1,
            character: 1,
        };

        let results = manager.definition(request.clone(), tmp.path()).await?;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].server, "healthy");
        assert_eq!(
            results[0].items[0]["path"],
            Value::String(file_path.to_string_lossy().into_owned())
        );

        let broken_key = ClientKey {
            server_id: broken_server.id,
            workspace_root: workspace_root.clone(),
        };
        assert!(manager.is_broken_client(&broken_key).await);

        let retried_results = manager.definition(request, tmp.path()).await?;
        assert_eq!(retried_results.len(), 1);
        assert_eq!(retried_results[0].server, "healthy");
        Ok(())
    }
}
