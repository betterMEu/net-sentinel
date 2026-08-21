use std::{
    collections::BTreeMap,
    fs,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub proxy: ProxyConfig,
    pub startup: StartupConfig,
    pub analytics: AnalyticsConfig,
    pub actions: Vec<ActionDefinition>,
    pub blacklist: Vec<Rule>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ProxyConfig {
    pub listen: SocketAddr,
    pub network_mode: NetworkMode,
    pub upstream_proxy: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NetworkMode {
    #[default]
    Auto,
    Direct,
    Http,
    Socks5,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct StartupConfig {
    pub start_minimized: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AnalyticsConfig {
    pub detailed_logging: bool,
    pub detailed_retention_days: u32,
    pub aggregate_retention_days: u32,
}

impl Default for AnalyticsConfig {
    fn default() -> Self {
        Self {
            detailed_logging: true,
            detailed_retention_days: 7,
            aggregate_retention_days: 90,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionDefinition {
    pub id: String,
    pub kind: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub params: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rule {
    pub id: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub target: MatchTarget,
    #[serde(default)]
    pub mode: MatchMode,
    pub pattern: String,
    #[serde(default)]
    pub methods: Vec<String>,
    #[serde(default)]
    pub header_name: Option<String>,
    #[serde(default)]
    pub actions: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MatchTarget {
    #[default]
    Host,
    Url,
    Path,
    Header,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MatchMode {
    Exact,
    #[default]
    Contains,
    Glob,
    Regex,
}

fn default_true() -> bool {
    true
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            listen: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8877),
            network_mode: NetworkMode::Auto,
            upstream_proxy: None,
        }
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        let mut image = BTreeMap::new();
        image.insert("source".into(), "builtin:blocked".into());
        image.insert("duration_ms".into(), "4500".into());

        let mut chime = BTreeMap::new();
        chime.insert("source".into(), "builtin:soft-chime".into());
        chime.insert("volume".into(), "0.7".into());

        let mut custom = BTreeMap::new();
        custom.insert("source".into(), "".into());
        custom.insert("volume".into(), "0.7".into());

        let mut html = BTreeMap::new();
        html.insert("source".into(), "builtin:mini-game".into());

        Self {
            proxy: ProxyConfig::default(),
            startup: StartupConfig::default(),
            analytics: AnalyticsConfig::default(),
            actions: vec![
                ActionDefinition {
                    id: "blocked-picture".into(),
                    kind: "popup_image".into(),
                    enabled: true,
                    params: image,
                },
                ActionDefinition {
                    id: "blocked-sound".into(),
                    kind: "play_audio".into(),
                    enabled: true,
                    params: chime,
                },
                ActionDefinition {
                    id: "imported-music".into(),
                    kind: "play_audio".into(),
                    enabled: false,
                    params: custom,
                },
                ActionDefinition {
                    id: "blocked-game".into(),
                    kind: "serve_html".into(),
                    enabled: true,
                    params: html,
                },
            ],
            blacklist: vec![
                Rule {
                    id: "demo-blocked-path".into(),
                    enabled: true,
                    target: MatchTarget::Url,
                    mode: MatchMode::Contains,
                    pattern: "example.com/blocked".into(),
                    methods: vec![],
                    header_name: None,
                    actions: vec!["blocked-picture".into(), "blocked-sound".into()],
                },
                Rule {
                    id: "social-sites-example".into(),
                    enabled: false,
                    target: MatchTarget::Host,
                    mode: MatchMode::Glob,
                    pattern: "*.example-social.test".into(),
                    methods: vec![],
                    header_name: None,
                    actions: vec!["blocked-picture".into(), "blocked-sound".into()],
                },
            ],
        }
    }
}

impl AppConfig {
    pub fn load_or_create(path: &Path) -> Result<Self> {
        if !path.exists() {
            let config = Self::default();
            config.save(path)?;
            return Ok(config);
        }
        let text =
            fs::read_to_string(path).with_context(|| format!("无法读取配置 {}", path.display()))?;
        let mut config: Self =
            toml::from_str(&text).with_context(|| format!("配置格式错误 {}", path.display()))?;
        if !config
            .actions
            .iter()
            .any(|action| action.kind == "serve_html")
        {
            let mut params = BTreeMap::new();
            params.insert("source".into(), "builtin:mini-game".into());
            config.actions.push(ActionDefinition {
                id: "blocked-game".into(),
                kind: "serve_html".into(),
                enabled: true,
                params,
            });
        }
        config.save(path)?;
        Ok(config)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let text = toml::to_string_pretty(self)?;
        fs::write(path, text).with_context(|| format!("无法保存配置 {}", path.display()))
    }

    pub fn action(&self, id: &str) -> Option<&ActionDefinition> {
        self.actions.iter().find(|action| action.id == id)
    }
}

pub fn app_data_dir() -> PathBuf {
    ProjectDirs::from("dev", "NetSentinel", "Net Sentinel")
        .map(|dirs| dirs.config_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn config_path() -> PathBuf {
    app_data_dir().join("config.toml")
}
