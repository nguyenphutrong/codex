mod client;
mod manager;
mod normalize;
mod protocol;
mod types;
mod util;

pub use manager::SessionManager;
pub use types::LspClientState;
pub use types::LspConfig;
pub use types::LspDiagnostic;
pub use types::LspOperationResult;
pub use types::LspPosition;
pub use types::LspRange;
pub use types::LspStatus;
pub use types::PositionRequest;
pub use types::ServerConfig;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::ClientHandle;
    use crate::manager::ClientKey;
    use crate::protocol::read_lsp_message;
    use crate::protocol::write_lsp_message;
    use crate::util::path_to_uri;
    use anyhow::Result;
    use pretty_assertions::assert_eq;
    use serde_json::Value;
    use serde_json::json;
    use std::collections::HashMap;
    use std::sync::Arc;
    use tempfile::tempdir;
    use tokio::fs;
    use tokio::io::AsyncWriteExt;
    use tokio::io::BufReader;
    use tokio::io::duplex;
    use tokio::sync::Mutex;
    use tokio::time::Duration;

    fn test_session_manager(config: Option<LspConfig>) -> SessionManager {
        SessionManager::with_options(
            config,
            |server, workspace_root| Box::pin(ClientHandle::spawn(server, workspace_root)),
            [
                Duration::from_millis(10),
                Duration::from_millis(20),
                Duration::from_millis(40),
            ],
        )
    }

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
            // Wait for didOpen before publishing diagnostics.
            while let Ok(Some(message)) = read_lsp_message(&mut reader).await {
                if message.get("method").and_then(Value::as_str) != Some("textDocument/didOpen") {
                    continue;
                }
                break;
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
        let touch_rev = client.open_or_change(&file_path).await?;
        client.wait_for_diagnostics(&file_path, touch_rev).await?;
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
        let items = normalize::normalize_location_like(&json!([{
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
        let items = normalize::normalize_document_symbols(&json!([{
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

        let manager = test_session_manager(Some(LspConfig {
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

        tokio::time::sleep(Duration::from_millis(100)).await;

        let health = manager
            .client_health
            .read()
            .await
            .get(&key)
            .cloned()
            .expect("health entry");
        assert_eq!(health.state, LspClientState::Broken);
        assert!(health.permanent_broken);

        let retry_error = manager
            .client_for_match(&server_match)
            .await
            .expect_err("broken client");
        assert!(retry_error.to_string().contains("permanently broken"));
        Ok(())
    }

    #[tokio::test]
    async fn client_for_match_marks_initialize_failures_as_broken() -> Result<()> {
        let tmp = tempdir()?;
        let file_path = tmp.path().join("main.rs");
        fs::write(&file_path, "fn main() {}\n").await?;

        let manager = test_session_manager(Some(LspConfig {
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

        tokio::time::sleep(Duration::from_millis(100)).await;

        let health = manager
            .client_health
            .read()
            .await
            .get(&key)
            .cloned()
            .expect("health entry");
        assert_eq!(health.state, LspClientState::Broken);
        assert!(health.permanent_broken);
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
        let connected_key = ClientKey {
            server_id: connected_server.id.clone(),
            workspace_root: workspace_root.clone(),
        };
        manager
            .clients
            .write()
            .await
            .insert(connected_key.clone(), client);
        {
            let mut health = manager.client_health.write().await;
            let entry = health.entry(connected_key).or_default();
            entry.state = LspClientState::Connected;
        }
        manager
            .mark_client_broken(
                ClientKey {
                    server_id: broken_server.id.clone(),
                    workspace_root: workspace_root.clone(),
                },
                "test".to_string(),
            )
            .await;

        let statuses = manager.status_for_file(&file_path, tmp.path()).await;
        assert_eq!(statuses.len(), 2);

        let broken_status = statuses.iter().find(|s| s.server == "broken").unwrap();
        assert_eq!(broken_status.state, LspClientState::Broken);

        let connected_status = statuses.iter().find(|s| s.server == "connected").unwrap();
        assert_eq!(connected_status.state, LspClientState::Connected);
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
        let health = manager
            .client_health
            .read()
            .await
            .get(&broken_key)
            .cloned()
            .expect("health");
        assert!(matches!(
            health.state,
            LspClientState::Broken | LspClientState::Starting
        ));

        let retried_results = manager.definition(request, tmp.path()).await?;
        assert_eq!(retried_results.len(), 1);
        assert_eq!(retried_results[0].server, "healthy");
        Ok(())
    }

    #[tokio::test]
    async fn file_open_change_versioning_increments() -> Result<()> {
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
                    "result": { "capabilities": {} },
                }),
            )
            .await
            .expect("write initialize response");
            while let Ok(Some(_)) = read_lsp_message(&mut reader).await {}
        });

        client.initialize().await?;
        let rev1 = client.open_or_change(&file_path).await?;
        let rev2 = client.open_or_change(&file_path).await?;
        assert!(rev2 > rev1);

        let versions = client.state.opened_versions.lock().await;
        let version = versions.get(&file_path).expect("opened version");
        assert_eq!(*version, 1);
        Ok(())
    }

    #[tokio::test]
    async fn server_crash_restarts_and_reopens_tracked_documents() -> Result<()> {
        let tmp = tempdir()?;
        let file_path = tmp.path().join("main.rs");
        let helper_path = tmp.path().join("helper.rs");
        fs::write(&file_path, "fn main() {}\n").await?;
        fs::write(&helper_path, "pub fn helper() {}\n").await?;
        let restarted_documents = Arc::new(Mutex::new(Vec::new()));
        let spawn_count = Arc::new(Mutex::new(0usize));
        let restarted_documents_for_spawn = restarted_documents.clone();
        let spawn_count_for_spawn = spawn_count.clone();

        let manager = SessionManager::with_options(
            Some(LspConfig {
                servers: vec![ServerConfig {
                    id: "crasher".to_string(),
                    command: "true".to_string(),
                    args: Vec::new(),
                    extensions: vec![".rs".to_string()],
                    env: HashMap::new(),
                    initialization: None,
                    root_markers: Vec::new(),
                }],
            }),
            move |server, workspace_root| {
                let restarted_documents = restarted_documents_for_spawn.clone();
                let spawn_count = spawn_count_for_spawn.clone();
                Box::pin(async move {
                    let spawn_attempt = {
                        let mut count = spawn_count.lock().await;
                        *count += 1;
                        *count
                    };
                    let (client_stream, server_stream) = duplex(16 * 1024);
                    let (client_reader, client_writer) = tokio::io::split(client_stream);
                    let (mut server_reader, mut server_writer) = tokio::io::split(server_stream);
                    let client = ClientHandle::from_streams(
                        server.id,
                        workspace_root,
                        server.initialization,
                        client_writer,
                        client_reader,
                        None,
                    )
                    .await?;

                    tokio::spawn(async move {
                        let mut reader = BufReader::new(&mut server_reader);
                        let request = read_lsp_message(&mut reader)
                            .await
                            .expect("initialize")
                            .expect("initialize request");
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

                        let mut opened = Vec::new();
                        while let Ok(Some(message)) = read_lsp_message(&mut reader).await {
                            if message.get("method").and_then(Value::as_str)
                                != Some("textDocument/didOpen")
                            {
                                continue;
                            }
                            let uri = message["params"]["textDocument"]["uri"]
                                .as_str()
                                .expect("didOpen uri")
                                .to_string();
                            opened.push(uri);

                            if spawn_attempt == 1 && opened.len() == 2 {
                                break;
                            }

                            if spawn_attempt == 2 && opened.len() == 2 {
                                *restarted_documents.lock().await = opened.clone();
                            }
                        }
                    });

                    client.initialize().await?;

                    Ok(client)
                })
            },
            [
                Duration::from_millis(10),
                Duration::from_millis(20),
                Duration::from_millis(40),
            ],
        );

        manager.touch_file(&file_path, tmp.path(), false).await?;
        manager.touch_file(&helper_path, tmp.path(), false).await?;
        let key = ClientKey {
            server_id: "crasher".to_string(),
            workspace_root: tmp.path().to_path_buf(),
        };
        manager
            .mark_client_broken_and_restart(key.clone(), "simulated crash".to_string())
            .await;

        tokio::time::sleep(Duration::from_millis(100)).await;

        let reopened = restarted_documents.lock().await.clone();
        let mut expected = vec![path_to_uri(&file_path)?, path_to_uri(&helper_path)?];
        expected.sort();
        let mut reopened_sorted = reopened;
        reopened_sorted.sort();
        assert_eq!(reopened_sorted, expected);

        let health = manager
            .client_health
            .read()
            .await
            .get(&key)
            .cloned()
            .expect("health");
        assert_eq!(health.state, LspClientState::Connected);
        assert_eq!(*spawn_count.lock().await, 2);
        Ok(())
    }

    #[tokio::test]
    async fn malformed_payload_does_not_crash_reader() -> Result<()> {
        let tmp = tempdir()?;

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
                    "result": { "capabilities": {} },
                }),
            )
            .await
            .expect("write init response");
            let _ = read_lsp_message(&mut reader).await;

            let garbage = b"not valid json at all";
            let header = format!("Content-Length: {}\r\n\r\n", garbage.len());
            use tokio::io::AsyncWriteExt;
            server_writer
                .write_all(header.as_bytes())
                .await
                .expect("write header");
            server_writer
                .write_all(garbage)
                .await
                .expect("write garbage");
            server_writer.shutdown().await.expect("shutdown");
        });

        client.initialize().await?;
        tokio::time::sleep(Duration::from_millis(100)).await;

        let pending = client.state.pending.lock().await;
        assert!(pending.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn workspace_folders_server_request_returns_workspace() -> Result<()> {
        let tmp = tempdir()?;

        let (client_stream, server_stream) = duplex(16 * 1024);
        let (client_reader, client_writer) = tokio::io::split(client_stream);
        let (mut server_reader, mut server_writer) = tokio::io::split(server_stream);
        let workspace_root = tmp.path().to_path_buf();
        let workspace_root_for_assert = workspace_root.clone();
        let client = ClientHandle::from_streams(
            "fake".to_string(),
            workspace_root,
            None,
            client_writer,
            client_reader,
            None,
        )
        .await?;

        let (tx, rx) = tokio::sync::oneshot::channel();
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
                    "result": { "capabilities": {} },
                }),
            )
            .await
            .expect("write init response");
            let _ = read_lsp_message(&mut reader).await;

            write_lsp_message(
                &mut server_writer,
                &json!({
                    "jsonrpc": "2.0",
                    "id": 999,
                    "method": "workspace/workspaceFolders",
                }),
            )
            .await
            .expect("write workspace folders request");

            let response = read_lsp_message(&mut reader)
                .await
                .expect("read response")
                .expect("response");
            let _ = tx.send(response);
        });

        client.initialize().await?;
        let response = rx.await?;
        let result = response.get("result").expect("result");
        let folders = result.as_array().expect("folders array");
        assert_eq!(folders.len(), 1);
        let uri = folders[0]["uri"].as_str().expect("uri");
        let expected_uri = path_to_uri(&workspace_root_for_assert)?;
        assert_eq!(uri, expected_uri);
        Ok(())
    }
}
