use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;

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

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LspServerAvailability {
    Ready,
    Installing,
    Unavailable,
    RequirementsMissing,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LspServerSource {
    Configured,
    Project,
    Global,
    ManagedCache,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct LspStatus {
    pub server: String,
    pub workspace_root: PathBuf,
    pub state: LspClientState,
    pub availability: LspServerAvailability,
    pub source: Option<LspServerSource>,
    pub retry_after_seconds: Option<u64>,
    pub last_error: Option<String>,
    pub requirements: Option<String>,
    pub resolved_command: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct LspDiagnostic {
    pub server: Option<String>,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedServerConfig {
    pub command: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub source: LspServerSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnavailableServer {
    pub availability: LspServerAvailability,
    pub reason: String,
    pub requirements: Option<String>,
    pub source: Option<LspServerSource>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerResolution {
    Resolved(ResolvedServerConfig),
    Unavailable(UnavailableServer),
}
