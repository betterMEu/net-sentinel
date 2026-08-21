use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InterceptionProtocol {
    Http,
    Https,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionSurface {
    BrowserPage,
    InAppCard,
    LocalAudio,
    ConnectionBlock,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionExecutionStatus {
    Succeeded,
    Failed,
    Unsupported,
    Skipped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionExecutionSummary {
    pub action_id: String,
    pub kind: String,
    pub status: ActionExecutionStatus,
    pub surface: ActionSurface,
    #[serde(skip)]
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ImagePresentation {
    pub source: ImageSource,
    pub duration_ms: u64,
}

#[derive(Debug, Clone)]
pub enum UiEvent {
    ProxyStatus {
        running: bool,
        detail: String,
    },
    ProtectionUpstreamChecked {
        result: Result<String, String>,
    },
    ProtectionLocalChecked {
        upstream_detail: String,
        result: Result<String, String>,
    },
    Blocked {
        rule_id: String,
        request: String,
        protocol: InterceptionProtocol,
        action_results: Vec<ActionExecutionSummary>,
        image: Option<ImagePresentation>,
    },
    ImportedAudio(PathBuf),
    ImportedImage(PathBuf),
    ImportedHtml(PathBuf),
    ExportStats(PathBuf),
    NetworkProbe(String),
    VerifySystemProxy(String),
    TrayShow,
    TrayToggleProxy,
    TrayQuit,
    Error(String),
}

#[derive(Debug, Clone)]
pub enum ImageSource {
    BuiltinBlocked,
    File(PathBuf),
}
