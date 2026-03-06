use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Instant;
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
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(45);
const BROKEN_CLIENT_TTL: Duration = Duration::from_secs(30);

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

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct LspStatus {
    pub server: String,
    pub workspace_root: PathBuf,
    pub connected: bool,
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
    broken_clients: Arc<Mutex<HashMap<ClientKey, Instant>>>,
}

impl SessionManager {
    pub fn new(config: Option<LspConfig>) -> Self {
        Self {
            config: config.map(Arc::new),
            clients: Arc::new(RwLock::new(HashMap::new())),
            broken_clients: Arc::new(Mutex::new(HashMap::new())),
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
            let previous_revision = client.diagnostic_revision(&server_match.file_path).await;
            if let Err(err) = client.open_or_change(&server_match.file_path).await {
                warn!(
                    "LSP client {} in {} failed to open {}: {err:#}",
                    key.server_id,
                    key.workspace_root.display(),
                    server_match.file_path.display()
                );
                self.mark_client_broken(key).await;
                continue;
            }

            if wait_for_diagnostics {
                let _ = client
                    .wait_for_diagnostics(&server_match.file_path, previous_revision)
                    .await;
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
        self.matching_server_matches(file_path, base_dir)
            .await
            .into_iter()
            .map(|server_match| LspStatus {
                server: server_match.server.id.clone(),
                workspace_root: server_match.workspace_root,
                connected: true,
            })
            .collect()
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
                self.mark_client_broken(key).await;
                continue;
            }

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
                    self.mark_client_broken(key).await;
                    continue;
                }
            };

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
        let matches = self.workspace_server_matches(base_dir).await;
        if matches.is_empty() {
            return Err(anyhow!("No LSP server available for this workspace."));
        }

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
                    self.mark_client_broken(key).await;
                    continue;
                }
            };

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
                self.mark_client_broken(key.clone()).await;
                continue;
            }

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
                    self.mark_client_broken(key.clone()).await;
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
                    self.mark_client_broken(key).await;
                    continue;
                }
            };

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
                self.mark_client_broken(key.clone()).await;
                continue;
            }

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
                    self.mark_client_broken(key).await;
                    continue;
                }
            };
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

        if self.is_broken_client(&key).await {
            return Err(anyhow!(
                "LSP server {} is temporarily unavailable for {}. Retry later.",
                key.server_id,
                key.workspace_root.display()
            ));
        }

        if let Some(existing) = self.clients.read().await.get(&key).cloned() {
            return Ok(existing);
        }

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
                self.mark_client_broken(key.clone()).await;
                return Err(err);
            }
        };

        self.clients
            .write()
            .await
            .insert(key.clone(), client.clone());
        Ok(client)
    }

    fn client_key(&self, server_match: &ServerMatch) -> ClientKey {
        ClientKey {
            server_id: server_match.server.id.clone(),
            workspace_root: server_match.workspace_root.clone(),
        }
    }

    async fn is_broken_client(&self, key: &ClientKey) -> bool {
        let mut broken_clients = self.broken_clients.lock().await;
        match broken_clients.get(key) {
            Some(since) if since.elapsed() <= BROKEN_CLIENT_TTL => true,
            Some(_) => {
                broken_clients.remove(key);
                false
            }
            None => false,
        }
    }

    async fn mark_client_broken(&self, key: ClientKey) {
        self.clients.write().await.remove(&key);
        self.broken_clients.lock().await.insert(key, Instant::now());
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

    async fn workspace_server_matches(&self, base_dir: &Path) -> Vec<ServerMatch> {
        let Some(config) = self.config.as_ref() else {
            return Vec::new();
        };

        let mut matches = Vec::new();
        for server in &config.servers {
            if resolve_command(&server.command).is_err() {
                continue;
            }

            matches.push(ServerMatch {
                file_path: base_dir.to_path_buf(),
                workspace_root: resolve_workspace_root(base_dir, &server.root_markers, base_dir),
                server: server.clone(),
            });
        }

        matches.sort_by(|a, b| a.server.id.cmp(&b.server.id));
        matches
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

#[derive(Debug)]
struct ClientHandle {
    workspace_root: PathBuf,
    initialization: Option<Value>,
    writer_tx: mpsc::UnboundedSender<Value>,
    state: Arc<ClientState>,
    child: Option<Mutex<Child>>,
}

#[derive(Debug)]
struct ClientState {
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>,
    diagnostics: RwLock<HashMap<PathBuf, Vec<LspDiagnostic>>>,
    diagnostics_revision_by_path: Mutex<HashMap<PathBuf, u64>>,
    opened_versions: Mutex<HashMap<PathBuf, i32>>,
    next_request_id: AtomicU64,
    next_diagnostics_revision: AtomicU64,
    diagnostics_notify: Notify,
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
            diagnostics_revision_by_path: Mutex::new(HashMap::new()),
            opened_versions: Mutex::new(HashMap::new()),
            next_request_id: AtomicU64::new(0),
            next_diagnostics_revision: AtomicU64::new(0),
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
            child: child.map(Mutex::new),
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
        timeout(
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

    async fn open_or_change(&self, file_path: &Path) -> Result<()> {
        let text = fs::read_to_string(file_path)
            .await
            .with_context(|| format!("failed to read {}", file_path.display()))?;
        let uri = path_to_uri(file_path)?;
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
            return Ok(());
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
        .await
    }

    async fn diagnostics_for_path(&self, file_path: &Path) -> Vec<LspDiagnostic> {
        self.state
            .diagnostics
            .read()
            .await
            .get(file_path)
            .cloned()
            .unwrap_or_default()
    }

    async fn diagnostic_revision(&self, file_path: &Path) -> u64 {
        self.state
            .diagnostics_revision_by_path
            .lock()
            .await
            .get(file_path)
            .copied()
            .unwrap_or_default()
    }

    async fn wait_for_diagnostics(&self, file_path: &Path, previous_revision: u64) -> Result<()> {
        let file_path = file_path.to_path_buf();
        timeout(DIAGNOSTICS_WAIT_TIMEOUT, async {
            loop {
                let current_revision = self.diagnostic_revision(&file_path).await;
                if current_revision > previous_revision {
                    break;
                }
                self.state.diagnostics_notify.notified().await;
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

        self.state
            .diagnostics
            .write()
            .await
            .insert(path.clone(), diagnostics);
        let revision = self
            .state
            .next_diagnostics_revision
            .fetch_add(1, Ordering::Relaxed)
            + 1;
        self.state
            .diagnostics_revision_by_path
            .lock()
            .await
            .insert(path, revision);
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
}

impl Drop for ClientHandle {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut()
            && let Ok(mut child) = child.try_lock()
        {
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
