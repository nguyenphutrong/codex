use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use serde_json::Value;
use serde_json::json;
use std::path::Path;
use std::path::PathBuf;
#[cfg(test)]
use tokio::time::Duration;
use url::Url;

use crate::client::ServerCapabilities;
use crate::client::TextDocumentChangeKind;
use crate::client::TextDocumentSaveCapabilities;
use crate::client::TextDocumentSyncCapabilities;

#[cfg(test)]
pub(crate) const CLIENT_BACKOFF_FIRST_FAILURE: Duration = Duration::from_secs(30);
#[cfg(test)]
pub(crate) const CLIENT_BACKOFF_SECOND_FAILURE: Duration = Duration::from_secs(120);
#[cfg(test)]
pub(crate) const CLIENT_BACKOFF_MAX_FAILURE: Duration = Duration::from_secs(600);

pub(crate) fn resolve_absolute_path(base_dir: &Path, file_path: &Path) -> PathBuf {
    if file_path.is_absolute() {
        return file_path.to_path_buf();
    }
    base_dir.join(file_path)
}

pub(crate) fn resolve_workspace_root(
    file_path: &Path,
    root_markers: &[String],
    workspace_boundary: &Path,
) -> PathBuf {
    let start_dir = if file_path.is_dir() {
        file_path
    } else {
        file_path.parent().unwrap_or(file_path)
    };
    let stop_at_boundary = start_dir.starts_with(workspace_boundary);
    let mut fallback = start_dir;

    for ancestor in start_dir.ancestors() {
        fallback = ancestor;
        if directory_matches_root_markers(ancestor, root_markers) {
            return ancestor.to_path_buf();
        }
        if stop_at_boundary && ancestor == workspace_boundary {
            return workspace_boundary.to_path_buf();
        }
    }

    fallback.to_path_buf()
}

pub(crate) fn workspace_roots_overlap(left: &Path, right: &Path) -> bool {
    left == right || left.starts_with(right) || right.starts_with(left)
}

fn directory_matches_root_markers(directory: &Path, root_markers: &[String]) -> bool {
    root_markers
        .iter()
        .any(|marker| directory.join(marker).exists())
}

#[cfg(test)]
pub(crate) fn backoff_for_failure(failure_count: u32) -> Duration {
    match failure_count {
        0 | 1 => CLIENT_BACKOFF_FIRST_FAILURE,
        2 => CLIENT_BACKOFF_SECOND_FAILURE,
        _ => CLIENT_BACKOFF_MAX_FAILURE,
    }
}

pub(crate) fn parse_sync_capabilities(
    capabilities: Option<&Value>,
) -> TextDocumentSyncCapabilities {
    let default = TextDocumentSyncCapabilities::default();
    let Some(sync) = capabilities.and_then(|capabilities| capabilities.get("textDocumentSync"))
    else {
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

pub(crate) fn parse_server_capabilities(capabilities: Option<&Value>) -> ServerCapabilities {
    let Some(capabilities) = capabilities else {
        return ServerCapabilities::default();
    };

    ServerCapabilities {
        has_definition: capability_supported(capabilities.get("definitionProvider")),
        has_hover: capability_supported(capabilities.get("hoverProvider")),
        has_references: capability_supported(capabilities.get("referencesProvider")),
        has_diagnostics: capability_supported(capabilities.get("diagnosticProvider")),
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

fn capability_supported(capability: Option<&Value>) -> bool {
    match capability {
        Some(Value::Bool(supported)) => *supported,
        Some(Value::Null) | None => false,
        Some(_) => true,
    }
}

pub(crate) fn resolve_command(command: &str) -> Result<PathBuf> {
    let path = PathBuf::from(command);
    if path.is_absolute() {
        return Ok(path);
    }
    which::which(command).with_context(|| format!("failed to resolve {command}"))
}

pub(crate) fn to_lsp_position(line: usize, character: usize) -> Value {
    json!({
        "line": line.saturating_sub(1),
        "character": character.saturating_sub(1),
    })
}

pub(crate) fn path_to_uri(path: &Path) -> Result<String> {
    Url::from_file_path(path)
        .map_err(|_| anyhow!("failed to convert {} to file URI", path.display()))
        .map(|url| url.to_string())
}

pub(crate) fn uri_to_path(uri: &str) -> Result<PathBuf> {
    Url::parse(uri)
        .context("failed to parse file URI")?
        .to_file_path()
        .map_err(|_| anyhow!("failed to convert URI {uri} to a path"))
}

pub(crate) fn language_id_for_path(path: &Path) -> &'static str {
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

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use tempfile::tempdir;

    #[test]
    fn resolve_workspace_root_prefers_nearest_marker() {
        let tmp = tempdir().expect("tmp");
        let workspace = tmp.path().join("workspace");
        let project = workspace.join("project");
        let nested = project.join("src/nested");
        std::fs::create_dir_all(&nested).expect("nested dir");
        std::fs::write(project.join("Cargo.toml"), "[package]\nname = \"demo\"\n").expect("marker");

        let file_path = nested.join("main.rs");
        std::fs::write(&file_path, "fn main() {}\n").expect("file");

        let root = resolve_workspace_root(&file_path, &["Cargo.toml".to_string()], &workspace);
        assert_eq!(root, project);
    }

    #[test]
    fn resolve_workspace_root_stops_at_workspace_boundary() {
        let tmp = tempdir().expect("tmp");
        let outer = tmp.path().join("outer");
        let workspace = outer.join("workspace");
        let nested = workspace.join("src/nested");
        std::fs::create_dir_all(&nested).expect("nested dir");
        std::fs::write(outer.join("Cargo.toml"), "[package]\nname = \"outer\"\n")
            .expect("outer marker");

        let file_path = nested.join("main.rs");
        std::fs::write(&file_path, "fn main() {}\n").expect("file");

        let root = resolve_workspace_root(&file_path, &["Cargo.toml".to_string()], &workspace);
        assert_eq!(root, workspace);
    }

    #[test]
    fn parse_server_capabilities_reads_supported_requests() {
        let capabilities = parse_server_capabilities(Some(&json!({
            "definitionProvider": true,
            "hoverProvider": { "workDoneProgress": true },
            "referencesProvider": false,
            "diagnosticProvider": {
                "interFileDependencies": false,
            },
        })));

        assert_eq!(
            capabilities,
            ServerCapabilities {
                has_definition: true,
                has_hover: true,
                has_references: false,
                has_diagnostics: true,
            }
        );
    }
}
