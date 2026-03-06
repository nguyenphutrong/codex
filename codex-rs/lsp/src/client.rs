use crate::normalize::diagnostic_from_value;
use crate::protocol::read_lsp_message;
use crate::protocol::write_lsp_message;
use crate::types::LspDiagnostic;
use crate::util::language_id_for_path;
use crate::util::parse_server_capabilities;
use crate::util::parse_sync_capabilities;
use crate::util::path_to_uri;
use crate::util::resolve_command;
use crate::util::uri_to_path;
use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use serde_json::Value;
use serde_json::json;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use tokio::fs;
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;
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

use crate::types::ServerConfig;

pub(crate) const DIAGNOSTICS_WAIT_TIMEOUT: Duration = Duration::from_secs(3);
pub(crate) const DIAGNOSTICS_SETTLE_DELAY: Duration = Duration::from_millis(150);
pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
pub(crate) const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(45);
pub(crate) const CLIENT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TextDocumentChangeKind {
    None,
    Full,
    Incremental,
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct TextDocumentSaveCapabilities {
    pub(crate) supported: bool,
    pub(crate) include_text: bool,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct TextDocumentSyncCapabilities {
    pub(crate) open_close: bool,
    pub(crate) change: TextDocumentChangeKind,
    pub(crate) save: TextDocumentSaveCapabilities,
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ServerCapabilities {
    pub(crate) has_definition: bool,
    pub(crate) has_hover: bool,
    pub(crate) has_references: bool,
    pub(crate) has_diagnostics: bool,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct CachedDiagnostics {
    pub(crate) diagnostics: Vec<LspDiagnostic>,
    pub(crate) last_touch_revision: u64,
    pub(crate) last_publish_revision: u64,
    pub(crate) pending_stale: bool,
}

#[derive(Debug)]
pub(crate) struct ClientState {
    pub(crate) pending: Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>,
    pub(crate) diagnostics: RwLock<HashMap<PathBuf, CachedDiagnostics>>,
    pub(crate) opened_versions: Mutex<HashMap<PathBuf, i32>>,
    pub(crate) sync_capabilities: RwLock<TextDocumentSyncCapabilities>,
    pub(crate) server_capabilities: RwLock<ServerCapabilities>,
    pub(crate) next_request_id: AtomicU64,
    pub(crate) next_state_revision: AtomicU64,
    pub(crate) diagnostics_notify: Notify,
}

#[derive(Debug)]
pub(crate) struct ClientHandle {
    pub(crate) workspace_root: PathBuf,
    pub(crate) initialization: Option<Value>,
    pub(crate) writer_tx: mpsc::UnboundedSender<Value>,
    pub(crate) state: Arc<ClientState>,
    pub(crate) child: Option<Arc<Mutex<Child>>>,
}

impl ClientHandle {
    pub(crate) async fn spawn(server: ServerConfig, workspace_root: PathBuf) -> Result<Arc<Self>> {
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

    pub(crate) async fn from_streams<W, R>(
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
            server_capabilities: RwLock::new(ServerCapabilities::default()),
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

    pub(crate) async fn initialize(&self) -> Result<()> {
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
        self.set_server_capabilities(parse_server_capabilities(
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

    pub(crate) async fn open_or_change(&self, file_path: &Path) -> Result<u64> {
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
        Ok(touch_revision)
    }

    pub(crate) async fn did_save(&self, file_path: &Path) -> Result<()> {
        if !self
            .state
            .opened_versions
            .lock()
            .await
            .contains_key(file_path)
        {
            return Ok(());
        }

        let save_capabilities = self.state.sync_capabilities.read().await.save;
        if !save_capabilities.supported {
            return Ok(());
        }

        let text = if save_capabilities.include_text {
            Some(
                fs::read_to_string(file_path)
                    .await
                    .with_context(|| format!("failed to read {}", file_path.display()))?,
            )
        } else {
            None
        };
        self.send_did_save(file_path, text.as_deref()).await
    }

    pub(crate) async fn ensure_definition_support(&self) -> Result<()> {
        self.ensure_capability(
            self.state.server_capabilities.read().await.has_definition,
            "definition",
        )
    }

    pub(crate) async fn ensure_hover_support(&self) -> Result<()> {
        self.ensure_capability(
            self.state.server_capabilities.read().await.has_hover,
            "hover",
        )
    }

    pub(crate) async fn ensure_references_support(&self) -> Result<()> {
        self.ensure_capability(
            self.state.server_capabilities.read().await.has_references,
            "references",
        )
    }

    pub(crate) async fn diagnostics_for_path(&self, file_path: &Path) -> Vec<LspDiagnostic> {
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

    pub(crate) async fn tracked_documents(&self) -> Vec<PathBuf> {
        let mut documents = self
            .state
            .opened_versions
            .lock()
            .await
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        documents.sort();
        documents
    }

    pub(crate) async fn last_publish_revision(&self, file_path: &Path) -> u64 {
        self.state
            .diagnostics
            .read()
            .await
            .get(file_path)
            .map(|cached| cached.last_publish_revision)
            .unwrap_or_default()
    }

    pub(crate) async fn wait_for_diagnostics(
        &self,
        file_path: &Path,
        touch_revision: u64,
    ) -> Result<()> {
        let file_path = file_path.to_path_buf();
        timeout(DIAGNOSTICS_WAIT_TIMEOUT, async {
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

                let observed_revision = cached.last_publish_revision;
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

    pub(crate) async fn send_request(&self, method: &str, params: Value) -> Result<Value> {
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

    pub(crate) async fn send_notification(&self, method: &str, params: Value) -> Result<()> {
        self.writer_tx
            .send(json!({
                "jsonrpc": "2.0",
                "method": method,
                "params": params,
            }))
            .map_err(|_| anyhow!("failed to send LSP notification {method}"))?;
        Ok(())
    }

    async fn send_did_save(&self, file_path: &Path, text: Option<&str>) -> Result<()> {
        let mut params = json!({
            "textDocument": {
                "uri": path_to_uri(file_path)?,
            },
        });
        if let Some(text) = text {
            params["text"] = Value::String(text.to_string());
        }
        self.send_notification("textDocument/didSave", params).await
    }

    pub(crate) async fn handle_message(&self, message: Value) -> Result<()> {
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

    pub(crate) async fn fail_pending(&self, message: &str) {
        let mut pending = self.state.pending.lock().await;
        for (_, sender) in pending.drain() {
            let _ = sender.send(Err(anyhow!(message.to_string())));
        }
    }

    pub(crate) async fn set_sync_capabilities(&self, capabilities: TextDocumentSyncCapabilities) {
        *self.state.sync_capabilities.write().await = capabilities;
    }

    pub(crate) async fn set_server_capabilities(&self, capabilities: ServerCapabilities) {
        *self.state.server_capabilities.write().await = capabilities;
    }

    pub(crate) async fn mark_touch_pending(&self, file_path: &Path) -> u64 {
        let revision = self.next_state_revision();
        let mut diagnostics = self.state.diagnostics.write().await;
        let entry = diagnostics.entry(file_path.to_path_buf()).or_default();
        entry.last_touch_revision = revision;
        entry.pending_stale = true;
        revision
    }

    pub(crate) fn next_state_revision(&self) -> u64 {
        self.state
            .next_state_revision
            .fetch_add(1, Ordering::Relaxed)
            + 1
    }

    pub(crate) fn is_running(&self) -> bool {
        let Some(child) = &self.child else {
            return true;
        };
        let Ok(mut child) = child.try_lock() else {
            return true;
        };
        matches!(child.try_wait(), Ok(None))
    }

    pub(crate) async fn shutdown(&self) {
        let _ = timeout(REQUEST_TIMEOUT, self.send_request("shutdown", Value::Null)).await;
        let _ = self.send_notification("exit", Value::Null).await;
        let Some(child) = &self.child else {
            return;
        };
        let Ok(mut child) = timeout(CLIENT_SHUTDOWN_TIMEOUT, child.lock()).await else {
            return;
        };
        if timeout(CLIENT_SHUTDOWN_TIMEOUT, child.wait())
            .await
            .is_err()
        {
            let _ = child.start_kill();
        }
    }

    fn ensure_capability(&self, supported: bool, capability_name: &str) -> Result<()> {
        if supported {
            return Ok(());
        }
        Err(anyhow!(
            "LSP server does not support {capability_name} requests."
        ))
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
                if timeout(CLIENT_SHUTDOWN_TIMEOUT, child.wait())
                    .await
                    .is_err()
                {
                    let _ = child.start_kill();
                }
            });
        } else if let Ok(mut child) = child.try_lock() {
            let _ = child.start_kill();
        }
    }
}
