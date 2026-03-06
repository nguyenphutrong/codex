use crate::function_tool::FunctionCallError;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::ToolHandler;
use crate::tools::registry::ToolKind;
use async_trait::async_trait;
use codex_lsp::PositionRequest;
use codex_protocol::models::FunctionCallOutputBody;
use serde::Deserialize;
use std::path::PathBuf;

pub struct LspHandler;

#[derive(Debug, Deserialize)]
struct LspArgs {
    operation: LspOperation,
    #[serde(default)]
    file_path: Option<String>,
    #[serde(default)]
    line: Option<usize>,
    #[serde(default)]
    character: Option<usize>,
    #[serde(default)]
    query: Option<String>,
}

#[derive(Debug, Deserialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
enum LspOperation {
    GoToDefinition,
    FindReferences,
    Hover,
    DocumentSymbol,
    WorkspaceSymbol,
    Status,
    GoToImplementation,
    PrepareCallHierarchy,
    IncomingCalls,
    OutgoingCalls,
}

#[async_trait]
impl ToolHandler for LspHandler {
    fn kind(&self) -> ToolKind {
        ToolKind::Function
    }

    async fn handle(&self, invocation: ToolInvocation) -> Result<ToolOutput, FunctionCallError> {
        let ToolInvocation {
            session,
            turn,
            payload,
            ..
        } = invocation;

        let arguments = match payload {
            ToolPayload::Function { arguments } => arguments,
            _ => {
                return Err(FunctionCallError::RespondToModel(
                    "lsp handler received unsupported payload".to_string(),
                ));
            }
        };

        let args: LspArgs = parse_arguments(&arguments)?;
        let manager = session
            .services
            .lsp_manager
            .as_ref()
            .ok_or_else(|| {
                FunctionCallError::RespondToModel(
                    "No LSP server available for this file type.".to_string(),
                )
            })?
            .clone();
        let base_dir = turn.cwd.clone();

        let body = match args.operation {
            LspOperation::GoToDefinition => serialize_lsp_results(
                manager
                    .definition(position_request(&args, &base_dir)?, &base_dir)
                    .await,
            ),
            LspOperation::FindReferences => serialize_lsp_results(
                manager
                    .references(position_request(&args, &base_dir)?, &base_dir)
                    .await,
            ),
            LspOperation::Hover => serialize_lsp_results(
                manager
                    .hover(position_request(&args, &base_dir)?, &base_dir)
                    .await,
            ),
            LspOperation::DocumentSymbol => {
                let file_path = file_path(&args, &base_dir)?;
                serialize_lsp_results(manager.document_symbol(&file_path, &base_dir).await)
            }
            LspOperation::WorkspaceSymbol => {
                let query = args
                    .query
                    .as_deref()
                    .filter(|query| !query.is_empty())
                    .ok_or_else(|| {
                        FunctionCallError::RespondToModel(
                            "query is required for workspace_symbol".to_string(),
                        )
                    })?;
                if args.file_path.is_some() {
                    let file_path = file_path(&args, &base_dir)?;
                    serialize_lsp_results(
                        manager
                            .workspace_symbol_for_file(query, &file_path, &base_dir)
                            .await,
                    )
                } else {
                    serialize_lsp_results(manager.workspace_symbol(query, &base_dir).await)
                }
            }
            LspOperation::Status => {
                let file_path = file_path(&args, &base_dir)?;
                serialize_json_value(manager.status_for_file(&file_path, &base_dir).await)
            }
            LspOperation::GoToImplementation => serialize_lsp_results(
                manager
                    .implementation(position_request(&args, &base_dir)?, &base_dir)
                    .await,
            ),
            LspOperation::PrepareCallHierarchy => serialize_lsp_results(
                manager
                    .prepare_call_hierarchy(position_request(&args, &base_dir)?, &base_dir)
                    .await,
            ),
            LspOperation::IncomingCalls => serialize_lsp_results(
                manager
                    .incoming_calls(position_request(&args, &base_dir)?, &base_dir)
                    .await,
            ),
            LspOperation::OutgoingCalls => serialize_lsp_results(
                manager
                    .outgoing_calls(position_request(&args, &base_dir)?, &base_dir)
                    .await,
            ),
        }?;

        Ok(ToolOutput::Function {
            body: FunctionCallOutputBody::Text(body),
            success: Some(true),
        })
    }
}

fn serialize_lsp_results<T>(results: anyhow::Result<Vec<T>>) -> Result<String, FunctionCallError>
where
    T: serde::Serialize,
{
    let results = results.map_err(|err| FunctionCallError::RespondToModel(err.to_string()))?;
    let value = match results.as_slice() {
        [single] => serde_json::to_value(single),
        _ => serde_json::to_value(&results),
    }
    .map_err(|err| {
        FunctionCallError::RespondToModel(format!("failed to serialize lsp result: {err}"))
    })?;
    serialize_json_value(value)
}

fn serialize_json_value<T>(value: T) -> Result<String, FunctionCallError>
where
    T: serde::Serialize,
{
    serde_json::to_string_pretty(&value).map_err(|err| {
        FunctionCallError::RespondToModel(format!("failed to serialize lsp result: {err}"))
    })
}

fn position_request(
    args: &LspArgs,
    base_dir: &std::path::Path,
) -> Result<PositionRequest, FunctionCallError> {
    let file_path = file_path(args, base_dir)?;
    let line = args.line.ok_or_else(|| {
        FunctionCallError::RespondToModel("line is required for this LSP operation".to_string())
    })?;
    let character = args.character.ok_or_else(|| {
        FunctionCallError::RespondToModel(
            "character is required for this LSP operation".to_string(),
        )
    })?;

    if line == 0 {
        return Err(FunctionCallError::RespondToModel(
            "line must be greater than zero".to_string(),
        ));
    }
    if character == 0 {
        return Err(FunctionCallError::RespondToModel(
            "character must be greater than zero".to_string(),
        ));
    }

    Ok(PositionRequest {
        file_path,
        line,
        character,
    })
}

fn file_path(args: &LspArgs, base_dir: &std::path::Path) -> Result<PathBuf, FunctionCallError> {
    let file_path = args.file_path.as_deref().ok_or_else(|| {
        FunctionCallError::RespondToModel(
            "file_path is required for this LSP operation".to_string(),
        )
    })?;
    let file_path = crate::util::resolve_path(base_dir, &PathBuf::from(file_path));
    if !file_path.exists() {
        return Err(FunctionCallError::RespondToModel(format!(
            "File not found: {}",
            file_path.display()
        )));
    }
    Ok(file_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use tempfile::tempdir;

    #[test]
    fn position_request_accepts_one_based_coordinates() {
        let tmp = tempdir().expect("tmp");
        let file_path = tmp.path().join("main.rs");
        std::fs::write(&file_path, "fn main() {}\n").expect("write file");
        let args = LspArgs {
            operation: LspOperation::Hover,
            file_path: Some("main.rs".to_string()),
            line: Some(1),
            character: Some(1),
            query: None,
        };

        let request = position_request(&args, tmp.path()).expect("valid request");
        assert_eq!(
            request,
            PositionRequest {
                file_path,
                line: 1,
                character: 1,
            }
        );
    }

    #[test]
    fn position_request_rejects_zero_based_coordinates() {
        let tmp = tempdir().expect("tmp");
        let file_path = tmp.path().join("main.rs");
        std::fs::write(&file_path, "fn main() {}\n").expect("write file");

        let zero_line = LspArgs {
            operation: LspOperation::Hover,
            file_path: Some("main.rs".to_string()),
            line: Some(0),
            character: Some(1),
            query: None,
        };
        assert_eq!(
            position_request(&zero_line, tmp.path()).expect_err("zero line"),
            FunctionCallError::RespondToModel("line must be greater than zero".to_string())
        );

        let zero_character = LspArgs {
            operation: LspOperation::Hover,
            file_path: Some("main.rs".to_string()),
            line: Some(1),
            character: Some(0),
            query: None,
        };
        assert_eq!(
            position_request(&zero_character, tmp.path()).expect_err("zero character"),
            FunctionCallError::RespondToModel("character must be greater than zero".to_string())
        );
    }

    #[test]
    fn file_path_rejects_missing_file() {
        let tmp = tempdir().expect("tmp");
        let args = LspArgs {
            operation: LspOperation::Hover,
            file_path: Some("missing.rs".to_string()),
            line: Some(1),
            character: Some(1),
            query: None,
        };

        assert_eq!(
            file_path(&args, tmp.path()).expect_err("missing file"),
            FunctionCallError::RespondToModel(format!(
                "File not found: {}",
                tmp.path().join("missing.rs").display()
            ))
        );
    }
}
