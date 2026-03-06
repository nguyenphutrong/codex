use crate::codex::Session;
use crate::codex::TurnContext;
use codex_lsp::LspDiagnostic;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use tokio::process::Command;
use tokio::time::Duration;
use tokio::time::timeout;

pub(crate) const MAX_LSP_DIAGNOSTIC_FILES: usize = 5;
pub(crate) const MAX_LSP_DIAGNOSTICS_PER_FILE: usize = 20;
const GIT_STATUS_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GitStatusSnapshot {
    pub repo_root: PathBuf,
    records: BTreeMap<PathBuf, String>,
}

pub(crate) async fn enrich_output_with_lsp_diagnostics(
    session: &Session,
    turn: &TurnContext,
    content: String,
    touched_files: &[PathBuf],
) -> String {
    let Some(lsp_manager) = session.services.lsp_manager.as_ref() else {
        return content;
    };

    let candidate_files = diagnostic_candidate_files(touched_files);
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

pub(crate) fn diagnostic_candidate_files(touched_files: &[PathBuf]) -> Vec<PathBuf> {
    let mut candidate_files = touched_files
        .iter()
        .filter(|path| path.is_file())
        .cloned()
        .collect::<Vec<_>>();
    candidate_files.sort();
    candidate_files.dedup();
    candidate_files.truncate(MAX_LSP_DIAGNOSTIC_FILES);
    candidate_files
}

pub(crate) async fn capture_git_status_snapshot(cwd: &Path) -> Option<GitStatusSnapshot> {
    let repo_root = run_git(cwd, &["rev-parse", "--show-toplevel"])
        .await
        .and_then(|stdout| {
            let root = String::from_utf8(stdout).ok()?;
            let root = root.trim();
            (!root.is_empty()).then(|| PathBuf::from(root))
        })?;

    let status_output =
        run_git(cwd, &["status", "--porcelain=v1", "-z", "--untracked-files=all"]).await?;
    Some(GitStatusSnapshot {
        repo_root: repo_root.clone(),
        records: parse_git_status_records(&repo_root, &status_output),
    })
}

pub(crate) fn diff_git_status_snapshots(
    before: &GitStatusSnapshot,
    after: &GitStatusSnapshot,
) -> Vec<PathBuf> {
    if before.repo_root != after.repo_root {
        return Vec::new();
    }

    let all_paths = before
        .records
        .keys()
        .chain(after.records.keys())
        .cloned()
        .collect::<BTreeSet<_>>();

    all_paths
        .into_iter()
        .filter(|path| before.records.get(path) != after.records.get(path))
        .collect()
}

async fn run_git(cwd: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let output = timeout(
        GIT_STATUS_TIMEOUT,
        Command::new("git")
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output(),
    )
    .await
    .ok()?
    .ok()?;

    output.status.success().then_some(output.stdout)
}

fn parse_git_status_records(repo_root: &Path, stdout: &[u8]) -> BTreeMap<PathBuf, String> {
    let mut entries = stdout
        .split(|byte| *byte == b'\0')
        .filter(|entry| !entry.is_empty())
        .peekable();
    let mut records = BTreeMap::new();

    while let Some(entry) = entries.next() {
        if entry.len() < 4 {
            continue;
        }
        let status = String::from_utf8_lossy(&entry[..2]).to_string();
        let path = repo_root.join(String::from_utf8_lossy(&entry[3..]).into_owned());
        records.insert(path, status.clone());

        let first_status = status.as_bytes().first().copied();
        if matches!(first_status, Some(b'R') | Some(b'C'))
            && let Some(extra_path) = entries.next()
        {
            let extra = repo_root.join(String::from_utf8_lossy(extra_path).into_owned());
            records.insert(extra, status.clone());
        }
    }

    records
}

fn format_lsp_diagnostics(
    cwd: &Path,
    diagnostics: std::collections::HashMap<std::path::PathBuf, Vec<LspDiagnostic>>,
) -> String {
    let mut seen = HashSet::new();
    let mut entries = diagnostics
        .into_iter()
        .filter_map(|(path, values)| {
            let mut errors = values
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
                .collect::<Vec<_>>();
            errors.sort_by(|left, right| {
                (
                    left.range.start.line,
                    left.range.start.character,
                    left.message.as_str(),
                )
                    .cmp(&(
                        right.range.start.line,
                        right.range.start.character,
                        right.message.as_str(),
                    ))
            });
            errors.truncate(MAX_LSP_DIAGNOSTICS_PER_FILE);
            (!errors.is_empty()).then_some((path, errors))
        })
        .collect::<Vec<_>>();
    entries.sort_by(|left, right| left.0.cmp(&right.0));
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
                        diagnostic.range.start.line,
                        diagnostic.range.start.character,
                        diagnostic.message
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

#[cfg(test)]
mod tests {
    use super::*;
    use codex_utils_absolute_path::AbsolutePathBuf;
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
    fn format_lsp_diagnostics_deduplicates_and_sorts_errors() {
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

    #[test]
    fn diagnostic_candidate_files_deduplicates_and_limits_files() {
        let cwd = tempdir().expect("cwd");
        let mut touched_files = Vec::new();
        for idx in 0..7 {
            let file_path = cwd.path().join(format!("src/file-{idx}.rs"));
            std::fs::create_dir_all(file_path.parent().expect("parent")).expect("create dir");
            std::fs::write(&file_path, "fn main() {}\n").expect("write file");
            touched_files.push(file_path.clone());
            if idx == 0 {
                touched_files.push(file_path);
            }
        }

        let files = diagnostic_candidate_files(&touched_files);
        assert_eq!(files.len(), MAX_LSP_DIAGNOSTIC_FILES);
        assert_eq!(files[0], cwd.path().join("src/file-0.rs"));
        assert_eq!(files[4], cwd.path().join("src/file-4.rs"));
    }

    #[test]
    fn diff_git_status_snapshots_only_returns_changed_records() {
        let repo_root = PathBuf::from("/tmp/repo");
        let before = GitStatusSnapshot {
            repo_root: repo_root.clone(),
            records: BTreeMap::from([
                (repo_root.join("dirty.rs"), "M ".to_string()),
                (repo_root.join("stable.rs"), "M ".to_string()),
            ]),
        };
        let after = GitStatusSnapshot {
            repo_root: repo_root.clone(),
            records: BTreeMap::from([
                (repo_root.join("dirty.rs"), "M ".to_string()),
                (repo_root.join("new.rs"), "??".to_string()),
            ]),
        };

        assert_eq!(
            diff_git_status_snapshots(&before, &after),
            vec![repo_root.join("new.rs"), repo_root.join("stable.rs")]
        );
    }

    #[test]
    fn parse_git_status_records_tracks_renames_and_copies() {
        let repo_root = PathBuf::from("/tmp/repo");
        let stdout = b"R  old.rs\0new.rs\0?? scratch.rs\0";
        let records = parse_git_status_records(&repo_root, stdout);
        assert_eq!(
            records,
            BTreeMap::from([
                (repo_root.join("new.rs"), "R ".to_string()),
                (repo_root.join("old.rs"), "R ".to_string()),
                (repo_root.join("scratch.rs"), "??".to_string()),
            ])
        );
    }

    #[test]
    fn absolute_pathbuf_callers_can_reuse_candidate_logic() {
        let cwd = tempdir().expect("cwd");
        let file_path = cwd.path().join("file.rs");
        std::fs::write(&file_path, "fn main() {}\n").expect("write file");
        let abs = AbsolutePathBuf::try_from(file_path.clone()).expect("abs");
        let paths = vec![abs.to_path_buf()];
        assert_eq!(diagnostic_candidate_files(&paths), vec![file_path]);
    }
}
