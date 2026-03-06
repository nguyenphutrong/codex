use codex_protocol::models::FunctionCallOutputBody;
use std::collections::BTreeMap;
use std::collections::HashSet;
use std::path::Path;

use crate::apply_patch;
use crate::apply_patch::InternalApplyPatchInvocation;
use crate::apply_patch::convert_apply_patch_to_protocol;
use crate::client_common::tools::FreeformTool;
use crate::client_common::tools::FreeformToolFormat;
use crate::client_common::tools::ResponsesApiTool;
use crate::client_common::tools::ToolSpec;
use crate::codex::Session;
use crate::codex::TurnContext;
use crate::function_tool::FunctionCallError;
use crate::tools::context::SharedTurnDiffTracker;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::events::ToolEmitter;
use crate::tools::events::ToolEventCtx;
use crate::tools::handlers::parse_arguments;
use crate::tools::orchestrator::ToolOrchestrator;
use crate::tools::registry::ToolHandler;
use crate::tools::registry::ToolKind;
use crate::tools::runtimes::apply_patch::ApplyPatchRequest;
use crate::tools::runtimes::apply_patch::ApplyPatchRuntime;
use crate::tools::sandboxing::ToolCtx;
use crate::tools::spec::ApplyPatchToolArgs;
use crate::tools::spec::JsonSchema;
use async_trait::async_trait;
use codex_apply_patch::ApplyPatchAction;
use codex_apply_patch::ApplyPatchFileChange;
use codex_lsp::LspDiagnostic;
use codex_utils_absolute_path::AbsolutePathBuf;
use std::sync::Arc;

pub struct ApplyPatchHandler;

const APPLY_PATCH_LARK_GRAMMAR: &str = include_str!("tool_apply_patch.lark");
const MAX_LSP_DIAGNOSTIC_FILES: usize = 5;
const MAX_LSP_DIAGNOSTICS_PER_FILE: usize = 20;

fn file_paths_for_action(action: &ApplyPatchAction) -> Vec<AbsolutePathBuf> {
    let mut keys = Vec::new();
    let cwd = action.cwd.as_path();

    for (path, change) in action.changes() {
        if let Some(key) = to_abs_path(cwd, path) {
            keys.push(key);
        }

        if let ApplyPatchFileChange::Update { move_path, .. } = change
            && let Some(dest) = move_path
            && let Some(key) = to_abs_path(cwd, dest)
        {
            keys.push(key);
        }
    }

    keys
}

fn to_abs_path(cwd: &Path, path: &Path) -> Option<AbsolutePathBuf> {
    AbsolutePathBuf::resolve_path_against_base(path, cwd).ok()
}

#[async_trait]
impl ToolHandler for ApplyPatchHandler {
    fn kind(&self) -> ToolKind {
        ToolKind::Function
    }

    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(
            payload,
            ToolPayload::Function { .. } | ToolPayload::Custom { .. }
        )
    }

    async fn is_mutating(&self, _invocation: &ToolInvocation) -> bool {
        true
    }

    async fn handle(&self, invocation: ToolInvocation) -> Result<ToolOutput, FunctionCallError> {
        let ToolInvocation {
            session,
            turn,
            tracker,
            call_id,
            tool_name,
            payload,
            ..
        } = invocation;

        let patch_input = match payload {
            ToolPayload::Function { arguments } => {
                let args: ApplyPatchToolArgs = parse_arguments(&arguments)?;
                args.input
            }
            ToolPayload::Custom { input } => input,
            _ => {
                return Err(FunctionCallError::RespondToModel(
                    "apply_patch handler received unsupported payload".to_string(),
                ));
            }
        };

        // Re-parse and verify the patch so we can compute changes and approval.
        // Avoid building temporary ExecParams/command vectors; derive directly from inputs.
        let cwd = turn.cwd.clone();
        let command = vec!["apply_patch".to_string(), patch_input.clone()];
        match codex_apply_patch::maybe_parse_apply_patch_verified(&command, &cwd) {
            codex_apply_patch::MaybeApplyPatchVerified::Body(changes) => {
                let touched_files = file_paths_for_action(&changes);
                match apply_patch::apply_patch(turn.as_ref(), changes).await {
                    InternalApplyPatchInvocation::Output(item) => {
                        let content = item?;
                        let content = enrich_apply_patch_output_with_lsp_diagnostics(
                            session.as_ref(),
                            turn.as_ref(),
                            content,
                            &touched_files,
                        )
                        .await;
                        Ok(ToolOutput::Function {
                            body: FunctionCallOutputBody::Text(content),
                            success: Some(true),
                        })
                    }
                    InternalApplyPatchInvocation::DelegateToExec(apply) => {
                        let changes = convert_apply_patch_to_protocol(&apply.action);
                        let file_paths = file_paths_for_action(&apply.action);
                        let emitter =
                            ToolEmitter::apply_patch(changes.clone(), apply.auto_approved);
                        let event_ctx = ToolEventCtx::new(
                            session.as_ref(),
                            turn.as_ref(),
                            &call_id,
                            Some(&tracker),
                        );
                        emitter.begin(event_ctx).await;

                        let req = ApplyPatchRequest {
                            action: apply.action,
                            file_paths: file_paths.clone(),
                            changes,
                            exec_approval_requirement: apply.exec_approval_requirement,
                            timeout_ms: None,
                            codex_exe: turn.codex_linux_sandbox_exe.clone(),
                        };

                        let mut orchestrator = ToolOrchestrator::new();
                        let mut runtime = ApplyPatchRuntime::new();
                        let tool_ctx = ToolCtx {
                            session: session.clone(),
                            turn: turn.clone(),
                            call_id: call_id.clone(),
                            tool_name: tool_name.to_string(),
                        };
                        let out = orchestrator
                            .run(
                                &mut runtime,
                                &req,
                                &tool_ctx,
                                turn.as_ref(),
                                turn.approval_policy.value(),
                            )
                            .await
                            .map(|result| result.output);
                        let event_ctx = ToolEventCtx::new(
                            session.as_ref(),
                            turn.as_ref(),
                            &call_id,
                            Some(&tracker),
                        );
                        let content = emitter.finish(event_ctx, out).await?;
                        let content = enrich_apply_patch_output_with_lsp_diagnostics(
                            session.as_ref(),
                            turn.as_ref(),
                            content,
                            &file_paths,
                        )
                        .await;
                        Ok(ToolOutput::Function {
                            body: FunctionCallOutputBody::Text(content),
                            success: Some(true),
                        })
                    }
                }
            }
            codex_apply_patch::MaybeApplyPatchVerified::CorrectnessError(parse_error) => {
                Err(FunctionCallError::RespondToModel(format!(
                    "apply_patch verification failed: {parse_error}"
                )))
            }
            codex_apply_patch::MaybeApplyPatchVerified::ShellParseError(error) => {
                tracing::trace!("Failed to parse apply_patch input, {error:?}");
                Err(FunctionCallError::RespondToModel(
                    "apply_patch handler received invalid patch input".to_string(),
                ))
            }
            codex_apply_patch::MaybeApplyPatchVerified::NotApplyPatch => {
                Err(FunctionCallError::RespondToModel(
                    "apply_patch handler received non-apply_patch input".to_string(),
                ))
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn intercept_apply_patch(
    command: &[String],
    cwd: &Path,
    timeout_ms: Option<u64>,
    session: Arc<Session>,
    turn: Arc<TurnContext>,
    tracker: Option<&SharedTurnDiffTracker>,
    call_id: &str,
    tool_name: &str,
) -> Result<Option<ToolOutput>, FunctionCallError> {
    match codex_apply_patch::maybe_parse_apply_patch_verified(command, cwd) {
        codex_apply_patch::MaybeApplyPatchVerified::Body(changes) => {
            let touched_files = file_paths_for_action(&changes);
            session
                .record_model_warning(
                    format!(
                        "apply_patch was requested via {tool_name}. Use the apply_patch tool instead of exec_command."
                    ),
                    turn.as_ref(),
                )
                .await;
            match apply_patch::apply_patch(turn.as_ref(), changes).await {
                InternalApplyPatchInvocation::Output(item) => {
                    let content = item?;
                    let content = enrich_apply_patch_output_with_lsp_diagnostics(
                        session.as_ref(),
                        turn.as_ref(),
                        content,
                        &touched_files,
                    )
                    .await;
                    Ok(Some(ToolOutput::Function {
                        body: FunctionCallOutputBody::Text(content),
                        success: Some(true),
                    }))
                }
                InternalApplyPatchInvocation::DelegateToExec(apply) => {
                    let changes = convert_apply_patch_to_protocol(&apply.action);
                    let approval_keys = file_paths_for_action(&apply.action);
                    let emitter = ToolEmitter::apply_patch(changes.clone(), apply.auto_approved);
                    let event_ctx = ToolEventCtx::new(
                        session.as_ref(),
                        turn.as_ref(),
                        call_id,
                        tracker.as_ref().copied(),
                    );
                    emitter.begin(event_ctx).await;

                    let req = ApplyPatchRequest {
                        action: apply.action,
                        file_paths: approval_keys.clone(),
                        changes,
                        exec_approval_requirement: apply.exec_approval_requirement,
                        timeout_ms,
                        codex_exe: turn.codex_linux_sandbox_exe.clone(),
                    };

                    let mut orchestrator = ToolOrchestrator::new();
                    let mut runtime = ApplyPatchRuntime::new();
                    let tool_ctx = ToolCtx {
                        session: session.clone(),
                        turn: turn.clone(),
                        call_id: call_id.to_string(),
                        tool_name: tool_name.to_string(),
                    };
                    let out = orchestrator
                        .run(
                            &mut runtime,
                            &req,
                            &tool_ctx,
                            turn.as_ref(),
                            turn.approval_policy.value(),
                        )
                        .await
                        .map(|result| result.output);
                    let event_ctx = ToolEventCtx::new(
                        session.as_ref(),
                        turn.as_ref(),
                        call_id,
                        tracker.as_ref().copied(),
                    );
                    let content = emitter.finish(event_ctx, out).await?;
                    let content = enrich_apply_patch_output_with_lsp_diagnostics(
                        session.as_ref(),
                        turn.as_ref(),
                        content,
                        &approval_keys,
                    )
                    .await;
                    Ok(Some(ToolOutput::Function {
                        body: FunctionCallOutputBody::Text(content),
                        success: Some(true),
                    }))
                }
            }
        }
        codex_apply_patch::MaybeApplyPatchVerified::CorrectnessError(parse_error) => {
            Err(FunctionCallError::RespondToModel(format!(
                "apply_patch verification failed: {parse_error}"
            )))
        }
        codex_apply_patch::MaybeApplyPatchVerified::ShellParseError(error) => {
            tracing::trace!("Failed to parse apply_patch input, {error:?}");
            Ok(None)
        }
        codex_apply_patch::MaybeApplyPatchVerified::NotApplyPatch => Ok(None),
    }
}

async fn enrich_apply_patch_output_with_lsp_diagnostics(
    session: &Session,
    turn: &TurnContext,
    content: String,
    touched_files: &[AbsolutePathBuf],
) -> String {
    let Some(lsp_manager) = session.services.lsp_manager.as_ref() else {
        return content;
    };

    let candidate_files = touched_files
        .iter()
        .map(AbsolutePathBuf::to_path_buf)
        .filter(|path| path.is_file())
        .collect::<Vec<_>>();
    if candidate_files.is_empty() {
        return content;
    }

    for file_path in &candidate_files {
        let _ = lsp_manager.touch_file(file_path, &turn.cwd, true).await;
    }

    let formatted = format_lsp_diagnostics(
        &turn.cwd,
        lsp_manager
            .diagnostics_for_paths(&candidate_files, &turn.cwd)
            .await,
    );
    if formatted.is_empty() {
        return content;
    }

    format!("{content}\n\n{formatted}")
}

fn format_lsp_diagnostics(
    cwd: &Path,
    diagnostics: std::collections::HashMap<std::path::PathBuf, Vec<LspDiagnostic>>,
) -> String {
    let mut seen = HashSet::new();
    let mut entries = diagnostics
        .into_iter()
        .filter_map(|(path, values)| {
            let errors = values
                .into_iter()
                .filter(|diagnostic| diagnostic.severity == Some(1))
                .filter_map(|diagnostic| {
                    let key = (
                        path.clone(),
                        diagnostic.range.start.line,
                        diagnostic.range.start.character,
                        diagnostic.message.clone(),
                    );
                    seen.insert(key).then_some(diagnostic)
                })
                .take(MAX_LSP_DIAGNOSTICS_PER_FILE)
                .collect::<Vec<_>>();
            (!errors.is_empty()).then_some((path, errors))
        })
        .collect::<Vec<_>>();
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    entries.truncate(MAX_LSP_DIAGNOSTIC_FILES);

    entries
        .into_iter()
        .map(|(path, errors)| {
            let label = path
                .strip_prefix(cwd)
                .ok()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| path.display().to_string());
            let body = errors
                .iter()
                .map(|diagnostic| {
                    format!(
                        "ERROR [{}:{}] {}",
                        diagnostic.range.start.line, diagnostic.range.start.character, diagnostic.message
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            format!(
                "LSP errors detected in {label}, please fix:\n<diagnostics file=\"{}\">\n{body}\n</diagnostics>",
                path.display()
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Returns a custom tool that can be used to edit files. Well-suited for GPT-5 models
/// https://platform.openai.com/docs/guides/function-calling#custom-tools
pub(crate) fn create_apply_patch_freeform_tool() -> ToolSpec {
    ToolSpec::Freeform(FreeformTool {
        name: "apply_patch".to_string(),
        description: "Use the `apply_patch` tool to edit files. This is a FREEFORM tool, so do not wrap the patch in JSON.".to_string(),
        format: FreeformToolFormat {
            r#type: "grammar".to_string(),
            syntax: "lark".to_string(),
            definition: APPLY_PATCH_LARK_GRAMMAR.to_string(),
        },
    })
}

/// Returns a json tool that can be used to edit files. Should only be used with gpt-oss models
pub(crate) fn create_apply_patch_json_tool() -> ToolSpec {
    let mut properties = BTreeMap::new();
    properties.insert(
        "input".to_string(),
        JsonSchema::String {
            description: Some(r#"The entire contents of the apply_patch command"#.to_string()),
        },
    );

    ToolSpec::Function(ResponsesApiTool {
        name: "apply_patch".to_string(),
        description: r#"Use the `apply_patch` tool to edit files.
Your patch language is a stripped‑down, file‑oriented diff format designed to be easy to parse and safe to apply. You can think of it as a high‑level envelope:

*** Begin Patch
[ one or more file sections ]
*** End Patch

Within that envelope, you get a sequence of file operations.
You MUST include a header to specify the action you are taking.
Each operation starts with one of three headers:

*** Add File: <path> - create a new file. Every following line is a + line (the initial contents).
*** Delete File: <path> - remove an existing file. Nothing follows.
*** Update File: <path> - patch an existing file in place (optionally with a rename).

May be immediately followed by *** Move to: <new path> if you want to rename the file.
Then one or more “hunks”, each introduced by @@ (optionally followed by a hunk header).
Within a hunk each line starts with:

For instructions on [context_before] and [context_after]:
- By default, show 3 lines of code immediately above and 3 lines immediately below each change. If a change is within 3 lines of a previous change, do NOT duplicate the first change’s [context_after] lines in the second change’s [context_before] lines.
- If 3 lines of context is insufficient to uniquely identify the snippet of code within the file, use the @@ operator to indicate the class or function to which the snippet belongs. For instance, we might have:
@@ class BaseClass
[3 lines of pre-context]
- [old_code]
+ [new_code]
[3 lines of post-context]

- If a code block is repeated so many times in a class or function such that even a single `@@` statement and 3 lines of context cannot uniquely identify the snippet of code, you can use multiple `@@` statements to jump to the right context. For instance:

@@ class BaseClass
@@ 	 def method():
[3 lines of pre-context]
- [old_code]
+ [new_code]
[3 lines of post-context]

The full grammar definition is below:
Patch := Begin { FileOp } End
Begin := "*** Begin Patch" NEWLINE
End := "*** End Patch" NEWLINE
FileOp := AddFile | DeleteFile | UpdateFile
AddFile := "*** Add File: " path NEWLINE { "+" line NEWLINE }
DeleteFile := "*** Delete File: " path NEWLINE
UpdateFile := "*** Update File: " path NEWLINE [ MoveTo ] { Hunk }
MoveTo := "*** Move to: " newPath NEWLINE
Hunk := "@@" [ header ] NEWLINE { HunkLine } [ "*** End of File" NEWLINE ]
HunkLine := (" " | "-" | "+") text NEWLINE

A full patch can combine several operations:

*** Begin Patch
*** Add File: hello.txt
+Hello world
*** Update File: src/app.py
*** Move to: src/main.py
@@ def greet():
-print("Hi")
+print("Hello, world!")
*** Delete File: obsolete.txt
*** End Patch

It is important to remember:

- You must include a header with your intended action (Add/Delete/Update)
- You must prefix new lines with `+` even when creating a new file
- File references can only be relative, NEVER ABSOLUTE.
"#
            .to_string(),
        strict: false,
        parameters: JsonSchema::Object {
            properties,
            required: Some(vec!["input".to_string()]),
            additional_properties: Some(false.into()),
        },
    })
}

#[cfg(test)]
mod lsp_output_tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::collections::HashMap;
    use tempfile::tempdir;

    #[test]
    fn format_lsp_diagnostics_only_includes_errors() {
        let cwd = tempdir().expect("cwd");
        let file_path = cwd.path().join("src/main.rs");
        let output = format_lsp_diagnostics(
            cwd.path(),
            HashMap::from([(
                file_path.clone(),
                vec![
                    LspDiagnostic {
                        path: file_path.clone(),
                        range: codex_lsp::LspRange {
                            start: codex_lsp::LspPosition {
                                line: 3,
                                character: 7,
                            },
                            end: codex_lsp::LspPosition {
                                line: 3,
                                character: 9,
                            },
                        },
                        severity: Some(2),
                        message: "warning".to_string(),
                        source: None,
                    },
                    LspDiagnostic {
                        path: file_path.clone(),
                        range: codex_lsp::LspRange {
                            start: codex_lsp::LspPosition {
                                line: 4,
                                character: 2,
                            },
                            end: codex_lsp::LspPosition {
                                line: 4,
                                character: 4,
                            },
                        },
                        severity: Some(1),
                        message: "boom".to_string(),
                        source: None,
                    },
                ],
            )]),
        );

        assert_eq!(
            output,
            format!(
                "LSP errors detected in src/main.rs, please fix:\n<diagnostics file=\"{}\">\nERROR [4:2] boom\n</diagnostics>",
                file_path.display()
            )
        );
    }

    #[test]
    fn format_lsp_diagnostics_deduplicates_errors() {
        let cwd = tempdir().expect("cwd");
        let file_path = cwd.path().join("src/main.rs");
        let output = format_lsp_diagnostics(
            cwd.path(),
            HashMap::from([(
                file_path.clone(),
                vec![
                    LspDiagnostic {
                        path: file_path.clone(),
                        range: codex_lsp::LspRange {
                            start: codex_lsp::LspPosition {
                                line: 4,
                                character: 2,
                            },
                            end: codex_lsp::LspPosition {
                                line: 4,
                                character: 3,
                            },
                        },
                        severity: Some(1),
                        message: "duplicate".to_string(),
                        source: None,
                    },
                    LspDiagnostic {
                        path: file_path.clone(),
                        range: codex_lsp::LspRange {
                            start: codex_lsp::LspPosition {
                                line: 4,
                                character: 2,
                            },
                            end: codex_lsp::LspPosition {
                                line: 4,
                                character: 3,
                            },
                        },
                        severity: Some(1),
                        message: "duplicate".to_string(),
                        source: None,
                    },
                    LspDiagnostic {
                        path: file_path,
                        range: codex_lsp::LspRange {
                            start: codex_lsp::LspPosition {
                                line: 5,
                                character: 1,
                            },
                            end: codex_lsp::LspPosition {
                                line: 5,
                                character: 2,
                            },
                        },
                        severity: Some(1),
                        message: "next".to_string(),
                        source: None,
                    },
                ],
            )]),
        );

        assert_eq!(output.matches("ERROR [4:2] duplicate").count(), 1);
        assert!(output.contains("ERROR [5:1] next"));
    }

    #[test]
    fn format_lsp_diagnostics_limits_error_count_per_file() {
        let cwd = tempdir().expect("cwd");
        let file_path = cwd.path().join("src/main.rs");

        let mut diagnostics = Vec::new();
        for line in 1..=25 {
            diagnostics.push(LspDiagnostic {
                path: file_path.clone(),
                range: codex_lsp::LspRange {
                    start: codex_lsp::LspPosition { line, character: 1 },
                    end: codex_lsp::LspPosition { line, character: 2 },
                },
                severity: Some(1),
                message: format!("error-{line}"),
                source: None,
            });
        }

        let output = format_lsp_diagnostics(cwd.path(), HashMap::from([(file_path, diagnostics)]));
        let visible_errors = output
            .lines()
            .filter(|line| line.starts_with("ERROR "))
            .count();
        assert_eq!(visible_errors, MAX_LSP_DIAGNOSTICS_PER_FILE);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_apply_patch::MaybeApplyPatchVerified;
    use pretty_assertions::assert_eq;
    use tempfile::TempDir;

    #[test]
    fn approval_keys_include_move_destination() {
        let tmp = TempDir::new().expect("tmp");
        let cwd = tmp.path();
        std::fs::create_dir_all(cwd.join("old")).expect("create old dir");
        std::fs::create_dir_all(cwd.join("renamed/dir")).expect("create dest dir");
        std::fs::write(cwd.join("old/name.txt"), "old content\n").expect("write old file");
        let patch = r#"*** Begin Patch
*** Update File: old/name.txt
*** Move to: renamed/dir/name.txt
@@
-old content
+new content
*** End Patch"#;
        let argv = vec!["apply_patch".to_string(), patch.to_string()];
        let action = match codex_apply_patch::maybe_parse_apply_patch_verified(&argv, cwd) {
            MaybeApplyPatchVerified::Body(action) => action,
            other => panic!("expected patch body, got: {other:?}"),
        };

        let keys = file_paths_for_action(&action);
        assert_eq!(keys.len(), 2);
    }
}
