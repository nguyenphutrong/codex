use anyhow::Result;
use clap::Parser;
use codex_core::config::Config;
use codex_core::config::ConfigOverrides;
use codex_core::lsp_runtime::build_lsp_session_manager;
use codex_tui::Cli as TuiCli;
use codex_utils_cli::CliConfigOverrides;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::path::Path;
use std::path::PathBuf;

#[derive(Debug, Parser)]
pub struct LspCli {
    #[clap(flatten)]
    pub config_overrides: CliConfigOverrides,

    #[command(subcommand)]
    pub subcommand: LspSubcommand,
}

#[derive(Debug, clap::Subcommand)]
pub enum LspSubcommand {
    Status,
    Diagnostics(DiagnosticsArgs),
}

#[derive(Debug, Parser)]
pub struct DiagnosticsArgs {
    #[arg(long = "file", value_name = "PATH")]
    pub file: Option<PathBuf>,
}

impl LspCli {
    pub async fn run(self, interactive: &TuiCli) -> Result<()> {
        match self.subcommand {
            LspSubcommand::Status => run_status(&self.config_overrides, interactive).await,
            LspSubcommand::Diagnostics(args) => {
                run_diagnostics(&self.config_overrides, interactive, args).await
            }
        }
    }
}

async fn run_status(config_overrides: &CliConfigOverrides, interactive: &TuiCli) -> Result<()> {
    let config = load_config(config_overrides, interactive).await?;
    let Some(lsp) = config.lsp.as_ref() else {
        println!("LSP integration is disabled.");
        return Ok(());
    };
    if lsp.mode == codex_core::config::types::LspMode::Off {
        println!("LSP mode is off.");
        return Ok(());
    }
    let Some(manager) = build_lsp_session_manager(&config) else {
        println!("LSP integration is disabled.");
        return Ok(());
    };

    let files = collect_workspace_lsp_files(&config);
    if files.is_empty() {
        println!(
            "No LSP-eligible files detected in {}.",
            config.cwd.display()
        );
        return Ok(());
    }

    let mut statuses = BTreeMap::new();
    for file_path in files {
        for status in manager.status_for_file(&file_path, &config.cwd).await {
            statuses
                .entry((status.server.clone(), status.workspace_root.clone()))
                .or_insert(status);
        }
    }

    for status in statuses.into_values() {
        let source = status
            .source
            .map(|source| format!("{source:?}").to_lowercase())
            .unwrap_or_else(|| "unknown".to_string());
        println!(
            "{}\tavailability={:?}\tstate={:?}\tsource={}\troot={}",
            status.server,
            status.availability,
            status.state,
            source,
            status.workspace_root.display()
        );
        if let Some(command) = status.resolved_command.as_deref() {
            println!("  command={command}");
        }
        if let Some(requirements) = status.requirements.as_deref() {
            println!("  requirements={requirements}");
        }
        if let Some(error) = status.last_error.as_deref() {
            println!("  last_error={error}");
        }
    }

    Ok(())
}

async fn run_diagnostics(
    config_overrides: &CliConfigOverrides,
    interactive: &TuiCli,
    args: DiagnosticsArgs,
) -> Result<()> {
    let config = load_config(config_overrides, interactive).await?;
    let Some(lsp) = config.lsp.as_ref() else {
        println!("LSP integration is disabled.");
        return Ok(());
    };
    if lsp.mode == codex_core::config::types::LspMode::Off {
        println!("LSP mode is off.");
        return Ok(());
    }
    let Some(manager) = build_lsp_session_manager(&config) else {
        println!("LSP integration is disabled.");
        return Ok(());
    };

    let files = if let Some(file_path) = args.file {
        let file_path = resolve_input_path(&config.cwd, &file_path);
        vec![file_path]
    } else {
        collect_workspace_lsp_files(&config)
    };
    if files.is_empty() {
        println!(
            "No LSP-eligible files detected in {}.",
            config.cwd.display()
        );
        return Ok(());
    }

    for file_path in &files {
        let _ = manager.touch_file(file_path, &config.cwd, true).await;
        let _ = manager.did_save_file(file_path, &config.cwd).await;
    }
    let diagnostics = manager.diagnostics_for_paths(&files, &config.cwd).await;
    if diagnostics.is_empty() {
        println!("No diagnostics.");
        return Ok(());
    }

    let mut entries = diagnostics.into_iter().collect::<Vec<_>>();
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    for (path, values) in entries {
        println!("{}", path.display());
        for diagnostic in values {
            let server = diagnostic.server.unwrap_or_else(|| "unknown".to_string());
            println!(
                "  [{}] {}:{} {}",
                server,
                diagnostic.range.start.line,
                diagnostic.range.start.character,
                diagnostic.message
            );
        }
    }

    Ok(())
}

async fn load_config(
    config_overrides: &CliConfigOverrides,
    interactive: &TuiCli,
) -> Result<Config> {
    let cli_kv_overrides = config_overrides
        .parse_overrides()
        .map_err(anyhow::Error::msg)?;
    Ok(Config::load_with_cli_overrides_and_harness_overrides(
        cli_kv_overrides,
        ConfigOverrides {
            config_profile: interactive.config_profile.clone(),
            cwd: interactive.cwd.clone(),
            ..Default::default()
        },
    )
    .await?)
}

fn collect_workspace_lsp_files(config: &Config) -> Vec<PathBuf> {
    let Some(lsp) = config.lsp.as_ref() else {
        return Vec::new();
    };
    let extensions = lsp
        .servers
        .iter()
        .flat_map(|server| server.extensions.iter().cloned())
        .collect::<BTreeSet<_>>();
    let mut files = Vec::new();
    collect_files_recursive(config.cwd.as_path(), &extensions, &mut files, 256);
    files
}

fn collect_files_recursive(
    root: &Path,
    extensions: &BTreeSet<String>,
    files: &mut Vec<PathBuf>,
    limit: usize,
) {
    if files.len() >= limit {
        return;
    }
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        if files.len() >= limit {
            return;
        }
        let path = entry.path();
        let file_name = entry.file_name();
        if should_skip_dir(path.as_path(), file_name.as_os_str()) {
            continue;
        }
        if path.is_dir() {
            collect_files_recursive(path.as_path(), extensions, files, limit);
            continue;
        }
        let extension = path
            .extension()
            .map(|ext| format!(".{}", ext.to_string_lossy()))
            .unwrap_or_default();
        if extensions.contains(&extension) {
            files.push(path);
        }
    }
}

fn should_skip_dir(path: &Path, file_name: &OsStr) -> bool {
    path.is_dir()
        && matches!(
            file_name.to_string_lossy().as_ref(),
            ".git" | "node_modules" | "target" | ".next" | "dist" | "build"
        )
}

fn resolve_input_path(cwd: &Path, file_path: &Path) -> PathBuf {
    if file_path.is_absolute() {
        return file_path.to_path_buf();
    }
    cwd.join(file_path)
}
