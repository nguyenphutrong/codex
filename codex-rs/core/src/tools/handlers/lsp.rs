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

        let results = match args.operation {
            LspOperation::GoToDefinition => {
                manager
                    .definition(position_request(&args, &base_dir)?, &base_dir)
                    .await
            }
            LspOperation::FindReferences => {
                manager
                    .references(position_request(&args, &base_dir)?, &base_dir)
                    .await
            }
            LspOperation::Hover => {
                manager
                    .hover(position_request(&args, &base_dir)?, &base_dir)
                    .await
            }
            LspOperation::DocumentSymbol => {
                let file_path = file_path(&args, &base_dir)?;
                manager.document_symbol(&file_path, &base_dir).await
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
                manager.workspace_symbol(query, &base_dir).await
            }
            LspOperation::GoToImplementation => {
                manager
                    .implementation(position_request(&args, &base_dir)?, &base_dir)
                    .await
            }
            LspOperation::PrepareCallHierarchy => {
                manager
                    .prepare_call_hierarchy(position_request(&args, &base_dir)?, &base_dir)
                    .await
            }
            LspOperation::IncomingCalls => {
                manager
                    .incoming_calls(position_request(&args, &base_dir)?, &base_dir)
                    .await
            }
            LspOperation::OutgoingCalls => {
                manager
                    .outgoing_calls(position_request(&args, &base_dir)?, &base_dir)
                    .await
            }
        }
        .map_err(|err| FunctionCallError::RespondToModel(err.to_string()))?;

        let body = match results.as_slice() {
            [single] => serde_json::to_string_pretty(single),
            _ => serde_json::to_string_pretty(&results),
        }
        .map_err(|err| {
            FunctionCallError::RespondToModel(format!("failed to serialize lsp result: {err}"))
        })?;

        Ok(ToolOutput::Function {
            body: FunctionCallOutputBody::Text(body),
            success: Some(true),
        })
    }
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
