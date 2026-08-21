#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod actions;
mod analytics;
mod assets;
mod config;
mod events;
mod proxy;
mod rules;
mod system;
mod text_input;

use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
    process::Command,
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};

use actions::{ActionContext, ActionRegistry};
use analytics::{ActionEventGroup, Analytics, ChartRange, DashboardStats, SeriesPoint};
use anyhow::{Context as _, Result};
use async_channel::{Receiver, Sender};
use config::{AppConfig, MatchMode, MatchTarget, NetworkMode, Rule, app_data_dir, config_path};
use events::{
    ActionExecutionStatus, ActionExecutionSummary, ActionSurface, ImagePresentation,
    ImageSource as ActionImageSource, InterceptionProtocol, UiEvent,
};
use gpui::{
    App, Application, Bounds, ClickEvent, Context, Entity, Global, StatefulInteractiveElement,
    StyledImage, Task, Timer, TitlebarOptions, Window, WindowBounds, WindowControlArea,
    WindowOptions, canvas, div, img, point, prelude::*, px, rgb, rgba, size,
};
use proxy::{ProxyService, SharedConfig};
use rules::{RequestFacts, matches as rule_matches};
use text_input::TextInput;
use tray_icon::{
    Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent,
    menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem},
};
use url::Url;

const BG: u32 = 0xfafafa;
const PANEL: u32 = 0xffffff;
const PANEL_ALT: u32 = 0xeeeeee;
const CHROME: u32 = 0xe3e3e3;
const BORDER: u32 = 0xd4d4d4;
const TEXT: u32 = 0x202124;
const MUTED: u32 = 0x62656a;
const ACCENT: u32 = 0x496b8d;
const SUCCESS: u32 = 0x367a50;
const DANGER: u32 = 0xb45151;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Overview,
    Rules,
    Actions,
    Settings,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SettingsSection {
    Windows,
    Network,
    Data,
    Advanced,
}

struct TrayGlobal {
    icon: TrayIcon,
}

impl Global for TrayGlobal {}

#[derive(Clone)]
struct ActionToast {
    group: ActionEventGroup,
    image: Option<ImagePresentation>,
}

struct AppView {
    config: SharedConfig,
    config_path: PathBuf,
    actions: Arc<ActionRegistry>,
    analytics: Arc<Analytics>,
    proxy: ProxyService,
    proxy_running: bool,
    proxy_detail: String,
    protection_pending: bool,
    preserve_stop_detail: bool,
    system_proxy_active: bool,
    startup_enabled: bool,
    page: Page,
    chart_range: ChartRange,
    stats: DashboardStats,
    action_groups: Vec<ActionEventGroup>,
    action_toast: Option<ActionToast>,
    action_notice_keys: HashMap<String, Instant>,
    unread_action_groups: u32,
    toast_generation: u64,
    window_hidden: bool,
    selected_rule: Option<usize>,
    settings_section: SettingsSection,
    rule_id_input: Entity<TextInput>,
    rule_pattern_input: Entity<TextInput>,
    rule_test_input: Entity<TextInput>,
    upstream_input: Entity<TextInput>,
    draft_target: MatchTarget,
    draft_mode: MatchMode,
    draft_enabled: bool,
    draft_actions: Vec<String>,
    rule_test_result: String,
    html_help_visible: bool,
    wave_phase: f32,
    recent: VecDeque<String>,
    ui_tx: Sender<UiEvent>,
    _event_task: Task<()>,
    _wave_task: Task<()>,
}

impl AppView {
    #[allow(clippy::too_many_arguments)]
    fn new(
        config: SharedConfig,
        config_path: PathBuf,
        actions: Arc<ActionRegistry>,
        analytics: Arc<Analytics>,
        proxy: ProxyService,
        ui_tx: Sender<UiEvent>,
        ui_rx: Receiver<UiEvent>,
        initially_hidden: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        let listen = config.read().expect("config lock").proxy.listen;
        let task = cx.spawn(async move |this, cx| {
            while let Ok(event) = ui_rx.recv().await {
                if this
                    .update(cx, |view, cx| view.handle_event(event, cx))
                    .is_err()
                {
                    break;
                }
            }
        });
        let wave_task = cx.spawn(async move |this, cx| {
            loop {
                Timer::after(Duration::from_millis(40)).await;
                if this
                    .update(cx, |view, cx| {
                        if view.system_proxy_active || view.protection_pending {
                            view.wave_phase = (view.wave_phase + 0.08) % std::f32::consts::TAU;
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        let rule_id_input = cx.new(|cx| TextInput::new("规则名称", cx));
        let rule_pattern_input = cx.new(|cx| TextInput::new("匹配内容", cx));
        let rule_test_input = cx.new(|cx| TextInput::new("http://example.com/path", cx));
        let upstream_input = cx.new(|cx| TextInput::new("127.0.0.1:7890", cx));
        let initial_upstream = config
            .read()
            .expect("config lock")
            .proxy
            .upstream_proxy
            .clone()
            .unwrap_or_default();
        upstream_input.update(cx, |input, cx| input.set_value(initial_upstream, cx));
        let mut view = Self {
            config,
            config_path,
            actions,
            analytics,
            proxy,
            proxy_running: false,
            proxy_detail: "就绪".into(),
            protection_pending: false,
            preserve_stop_detail: false,
            system_proxy_active: system::is_our_proxy(&listen.to_string()),
            startup_enabled: system::startup_enabled(),
            page: Page::Overview,
            chart_range: ChartRange::Day,
            stats: DashboardStats::default(),
            action_groups: Vec::new(),
            action_toast: None,
            action_notice_keys: HashMap::new(),
            unread_action_groups: 0,
            toast_generation: 0,
            window_hidden: initially_hidden,
            selected_rule: None,
            settings_section: SettingsSection::Windows,
            rule_id_input,
            rule_pattern_input,
            rule_test_input,
            upstream_input,
            draft_target: MatchTarget::Host,
            draft_mode: MatchMode::Contains,
            draft_enabled: true,
            draft_actions: Vec::new(),
            rule_test_result: "输入 URL 后可在保存前验证规则".into(),
            html_help_visible: false,
            wave_phase: 0.0,
            recent: VecDeque::new(),
            ui_tx,
            _event_task: task,
            _wave_task: wave_task,
        };
        view.reload_stats_inner();
        view.reload_action_groups_inner();
        if !view
            .config
            .read()
            .expect("config lock")
            .blacklist
            .is_empty()
        {
            view.select_rule(0, cx);
        }
        view
    }

    fn handle_event(&mut self, event: UiEvent, cx: &mut Context<Self>) {
        match event {
            UiEvent::ProxyStatus { running, detail } => {
                self.proxy_running = running;
                if !running && detail == "代理已停止" && self.preserve_stop_detail {
                    self.preserve_stop_detail = false;
                } else if !(running && self.protection_pending) {
                    self.proxy_detail = detail;
                }
                if !running && self.system_proxy_active {
                    match system::restore_system_proxy() {
                        Ok(()) => {
                            self.system_proxy_active = false;
                            self.push_recent("本机监听意外停止，已立即恢复 Clash 系统代理".into());
                        }
                        Err(error) => self.push_recent(format!(
                            "严重：监听停止后恢复 Clash 系统代理失败：{error:#}"
                        )),
                    }
                }
            }
            UiEvent::ProtectionUpstreamChecked { result } => match result {
                Ok(upstream_detail) => {
                    if let Err(error) = self.proxy.start() {
                        self.protection_pending = false;
                        self.proxy_detail = format!("启动本机监听失败：{error:#}");
                        self.push_recent(format!(
                            "启动保护已取消，Clash 与系统代理保持原状：{error:#}"
                        ));
                    } else {
                        self.proxy_detail = "正在验证本机转发…".into();
                        let listen = self.config.read().expect("config lock").proxy.listen;
                        let tx = self.ui_tx.clone();
                        std::thread::spawn(move || {
                            let result = proxy::probe_local_proxy(listen)
                                .map_err(|error| format!("{error:#}"));
                            let _ = tx.send_blocking(UiEvent::ProtectionLocalChecked {
                                upstream_detail,
                                result,
                            });
                        });
                    }
                }
                Err(error) => {
                    self.protection_pending = false;
                    self.proxy_detail = format!("Clash 出站检查失败：{error}");
                    self.push_recent(format!("启动保护已取消，Clash 与系统代理保持原状：{error}"));
                }
            },
            UiEvent::ProtectionLocalChecked {
                upstream_detail,
                result,
            } => match result {
                Ok(local_detail) => {
                    let listen = self.config.read().expect("config lock").proxy.listen;
                    match self.enable_system_proxy_checked(&listen.to_string()) {
                        Ok(()) => {
                            self.protection_pending = false;
                            self.proxy_detail =
                                format!("保护已启动 · {upstream_detail}；{local_detail}");
                            self.push_recent(self.proxy_detail.clone());
                        }
                        Err(error) => {
                            self.preserve_stop_detail = true;
                            self.proxy.stop();
                            self.protection_pending = false;
                            self.proxy_detail = format!("系统代理接管失败：{error:#}");
                            self.push_recent(format!(
                                "启动保护已取消，本机监听已停止，Clash 保持原状：{error:#}"
                            ));
                        }
                    }
                }
                Err(error) => {
                    self.preserve_stop_detail = true;
                    self.proxy.stop();
                    self.protection_pending = false;
                    self.proxy_detail = format!("本机转发验证失败：{error}");
                    self.push_recent(format!(
                        "启动保护已取消，本机监听已停止，Clash 与系统代理保持原状：{error}"
                    ));
                }
            },
            UiEvent::Blocked {
                rule_id,
                request,
                protocol,
                action_results,
                image,
            } => {
                self.reload_stats_inner();
                self.reload_action_groups_inner();
                if let Some(latest) = self.action_groups.first_mut() {
                    latest.action_results = action_results.clone();
                }
                let key = action_notice_key(&rule_id, &request, protocol, &action_results);
                let now = Instant::now();
                self.action_notice_keys
                    .retain(|_, seen| now.duration_since(*seen) <= Duration::from_secs(60));
                let first_in_window = self
                    .action_notice_keys
                    .get(&key)
                    .is_none_or(|seen| now.duration_since(*seen) > Duration::from_secs(30));
                self.action_notice_keys.insert(key, now);
                if self.page == Page::Actions && !self.window_hidden {
                    self.clear_action_unread(cx);
                } else if let Some(group) = self.action_groups.first().cloned() {
                    if first_in_window {
                        self.unread_action_groups = self.unread_action_groups.saturating_add(1);
                        if !self.window_hidden {
                            self.show_action_toast(group, image, cx);
                        }
                    } else if let Some(toast) = self.action_toast.as_mut() {
                        toast.group = group;
                    }
                    self.update_tray_badge(cx);
                }
                self.push_recent(format!(
                    "拦截 · {rule_id} · {} 个结果 · {request}",
                    action_results.len()
                ));
            }
            UiEvent::ImportedAudio(path) => self.finish_import("blocked-sound", path, cx),
            UiEvent::ImportedImage(path) => self.finish_import("blocked-picture", path, cx),
            UiEvent::ImportedHtml(path) => self.finish_import("blocked-game", path, cx),
            UiEvent::ExportStats(path) => match self.analytics.export_csv(&path) {
                Ok(()) => self.push_recent(format!("统计已导出：{}", path.display())),
                Err(error) => self.push_recent(format!("导出统计失败：{error:#}")),
            },
            UiEvent::NetworkProbe(message) => self.push_recent(message),
            UiEvent::VerifySystemProxy(expected) => {
                if self.system_proxy_active && !system::is_our_proxy(&expected) {
                    self.system_proxy_active = false;
                    let _ = system::restore_system_proxy();
                    self.push_recent(
                        "Windows 系统代理被其他程序改回；已停止接管并恢复原设置".into(),
                    );
                }
            }
            UiEvent::TrayShow => {
                self.window_hidden = false;
                if self.page == Page::Actions {
                    self.clear_action_unread(cx);
                }
                cx.activate(true);
            }
            UiEvent::TrayToggleProxy => self.toggle_protection_inner(cx),
            UiEvent::TrayQuit => {
                self.cleanup_system_proxy();
                cx.quit();
            }
            UiEvent::Error(message) => self.push_recent(format!("错误 · {message}")),
        }
        cx.notify();
    }

    fn push_recent(&mut self, message: String) {
        self.recent.push_front(message);
        self.recent.truncate(7);
    }

    fn toggle_proxy(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.toggle_proxy_inner(cx);
    }

    fn toggle_proxy_inner(&mut self, cx: &mut Context<Self>) {
        if self.proxy_running || self.proxy.is_running() {
            self.proxy.stop();
            if self.system_proxy_active {
                match system::restore_system_proxy() {
                    Ok(()) => self.system_proxy_active = false,
                    Err(error) => self.push_recent(format!("恢复系统代理失败：{error:#}")),
                }
            }
        } else {
            match self.start_listener_checked() {
                Ok(detail) => self.push_recent(detail),
                Err(error) => {
                    self.push_recent(format!("启动监听已取消，系统代理未修改：{error:#}"))
                }
            }
        }
        cx.notify();
    }

    fn toggle_system_proxy(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.system_proxy_active {
            match system::restore_system_proxy() {
                Ok(()) => {
                    self.system_proxy_active = false;
                    self.push_recent("已恢复接管前的 Windows 系统代理".into());
                }
                Err(error) => self.push_recent(format!("恢复系统代理失败：{error:#}")),
            }
        } else {
            self.begin_protection_start();
        }
        cx.notify();
    }

    fn capture_existing_proxy_for_auto_route(&mut self, listen: &str) {
        let existing = system::current_system_proxy().filter(|value| value != listen);
        let result = {
            let mut config = self.config.write().expect("config lock");
            if config.proxy.network_mode == NetworkMode::Auto
                && config.proxy.upstream_proxy != existing
            {
                config.proxy.upstream_proxy = existing;
                config.save(&self.config_path)
            } else {
                Ok(())
            }
        };
        if let Err(error) = result {
            self.push_recent(format!("保存自动上游失败：{error:#}"));
        }
    }

    fn toggle_protection(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.toggle_protection_inner(cx);
    }

    fn toggle_protection_inner(&mut self, cx: &mut Context<Self>) {
        if self.protection_pending {
            return;
        }
        if self.system_proxy_active {
            self.proxy.stop();
            match system::restore_system_proxy() {
                Ok(()) => {
                    self.system_proxy_active = false;
                    self.proxy_detail = "保护已停止，已恢复原系统代理".into();
                }
                Err(error) => self.push_recent(format!("恢复系统代理失败：{error:#}")),
            }
        } else {
            self.begin_protection_start();
        }
        cx.notify();
    }

    fn begin_protection_start(&mut self) {
        if self.protection_pending {
            return;
        }
        let (mode, upstream, listen) = {
            let config = self.config.read().expect("config lock");
            (
                config.proxy.network_mode,
                config.proxy.upstream_proxy.clone(),
                config.proxy.listen,
            )
        };
        self.capture_existing_proxy_for_auto_route(&listen.to_string());
        let upstream = self
            .config
            .read()
            .expect("config lock")
            .proxy
            .upstream_proxy
            .clone()
            .or(upstream);
        self.protection_pending = true;
        self.proxy_detail = "正在验证 Clash 出站…".into();
        let tx = self.ui_tx.clone();
        std::thread::spawn(move || {
            let result = proxy::probe_upstream(mode, upstream.as_deref(), listen)
                .map_err(|error| format!("{error:#}"));
            let _ = tx.send_blocking(UiEvent::ProtectionUpstreamChecked { result });
        });
    }

    fn start_listener_checked(&mut self) -> Result<String> {
        let (mode, upstream, listen) = {
            let config = self.config.read().expect("config lock");
            (
                config.proxy.network_mode,
                config.proxy.upstream_proxy.clone(),
                config.proxy.listen,
            )
        };
        let upstream_detail = proxy::probe_upstream(mode, upstream.as_deref(), listen)?;
        if !self.proxy.is_running() {
            self.proxy.start()?;
        }
        match proxy::probe_local_proxy(listen) {
            Ok(local_detail) => Ok(format!("{upstream_detail}；{local_detail}")),
            Err(error) => {
                self.proxy.stop();
                Err(error).context("本机代理转发验证失败，监听已停止")
            }
        }
    }

    fn enable_system_proxy_checked(&mut self, listen: &str) -> Result<()> {
        system::enable_system_proxy(listen)?;
        if !system::is_our_proxy(listen) {
            let _ = system::restore_system_proxy();
            anyhow::bail!("Windows 系统代理写入后未保持为 {listen}");
        }
        self.system_proxy_active = true;
        let tx = self.ui_tx.clone();
        let expected = listen.to_string();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(800));
            let _ = tx.send_blocking(UiEvent::VerifySystemProxy(expected));
        });
        Ok(())
    }

    fn toggle_startup(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        let next = !self.startup_enabled;
        match system::set_startup(next, true) {
            Ok(()) => {
                self.startup_enabled = next;
                self.push_recent(if next {
                    "已启用开机启动（静默到托盘）".into()
                } else {
                    "已关闭开机启动".into()
                });
            }
            Err(error) => self.push_recent(format!("开机启动设置失败：{error:#}")),
        }
        cx.notify();
    }

    fn reload_config(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        match AppConfig::load_or_create(&self.config_path) {
            Ok(config) => {
                *self.config.write().expect("config lock") = config;
                let disconnected = self.proxy.disconnect_clients();
                self.push_recent(format!(
                    "已重新载入规则、黑名单和 Action；已刷新 {disconnected} 条连接"
                ));
            }
            Err(error) => self.push_recent(format!("载入配置失败：{error:#}")),
        }
        cx.notify();
    }

    fn open_config(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        let result = Command::new("notepad.exe").arg(&self.config_path).spawn();
        if let Err(error) = result {
            self.push_recent(format!("无法打开配置：{error}"));
        }
        cx.notify();
    }

    fn hide_to_tray(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.window_hidden = true;
        cx.hide();
    }

    fn test_image(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        let definition = self
            .config
            .read()
            .expect("config lock")
            .action("blocked-picture")
            .cloned();
        if let Some(definition) = definition {
            let context = ActionContext {
                rule_id: "action-preview".into(),
                request: "preview://popup-image".into(),
            };
            match self.actions.execute(&context, &definition) {
                Ok(output) => {
                    let summary = ActionExecutionSummary {
                        action_id: definition.id,
                        kind: definition.kind,
                        status: ActionExecutionStatus::Succeeded,
                        surface: ActionSurface::InAppCard,
                        error: None,
                    };
                    let group = ActionEventGroup::preview(summary);
                    self.show_action_toast(group, output.image, cx);
                }
                Err(error) => self.push_recent(format!("图片预览失败：{error:#}")),
            }
        }
        cx.notify();
    }

    fn test_audio(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        let definition = self
            .config
            .read()
            .expect("config lock")
            .action("blocked-sound")
            .cloned();
        if let Some(definition) = definition {
            let context = ActionContext {
                rule_id: "action-preview".into(),
                request: "preview://play-audio".into(),
            };
            if let Err(error) = self.actions.execute(&context, &definition) {
                self.push_recent(format!("音频预览失败：{error:#}"));
            }
        }
        cx.notify();
    }

    fn import_audio(&mut self, _: &ClickEvent, _: &mut Window, _: &mut Context<Self>) {
        let tx = self.ui_tx.clone();
        std::thread::spawn(move || {
            if let Some(path) = rfd::FileDialog::new()
                .set_title("选择拦截提示音乐")
                .add_filter("音频", &["wav", "mp3", "flac", "ogg"])
                .pick_file()
            {
                let _ = tx.send_blocking(UiEvent::ImportedAudio(path));
            }
        });
    }

    fn import_image(&mut self, _: &ClickEvent, _: &mut Window, _: &mut Context<Self>) {
        let tx = self.ui_tx.clone();
        std::thread::spawn(move || {
            if let Some(path) = rfd::FileDialog::new()
                .set_title("选择拦截提示图片")
                .add_filter("图片", &["png", "jpg", "jpeg", "webp", "gif", "svg"])
                .pick_file()
            {
                let _ = tx.send_blocking(UiEvent::ImportedImage(path));
            }
        });
    }

    fn import_html(&mut self, _: &ClickEvent, _: &mut Window, _: &mut Context<Self>) {
        let tx = self.ui_tx.clone();
        std::thread::spawn(move || {
            if let Some(path) = rfd::FileDialog::new()
                .set_title("选择拦截后显示的 HTML 页面")
                .add_filter("HTML", &["html", "htm"])
                .pick_file()
            {
                let _ = tx.send_blocking(UiEvent::ImportedHtml(path));
            }
        });
    }

    fn test_html(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.html_help_visible = !self.html_help_visible;
        cx.notify();
    }

    fn use_default_image(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.set_default_action_resource("blocked-picture", "builtin:blocked", cx);
    }

    fn use_default_audio(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.set_default_action_resource("blocked-sound", "builtin:soft-chime", cx);
    }

    fn use_default_html(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.set_default_action_resource("blocked-game", "builtin:mini-game", cx);
    }

    fn set_default_action_resource(
        &mut self,
        action_id: &str,
        source: &str,
        cx: &mut Context<Self>,
    ) {
        let result = {
            let mut config = self.config.write().expect("config lock");
            if let Some(action) = config
                .actions
                .iter_mut()
                .find(|action| action.id == action_id)
            {
                action.enabled = true;
                action.params.insert("source".into(), source.into());
            }
            config.save(&self.config_path)
        };
        match result {
            Ok(()) => {
                let disconnected = self.proxy.disconnect_clients();
                self.push_recent(format!(
                    "{action_id} 已切换为内置默认；已刷新 {disconnected} 条连接"
                ));
            }
            Err(error) => self.push_recent(format!("恢复内置资源失败：{error:#}")),
        }
        cx.notify();
    }

    fn finish_import(&mut self, action_id: &str, path: PathBuf, cx: &mut Context<Self>) {
        let display = path.display().to_string();
        let result = {
            let mut config = self.config.write().expect("config lock");
            if let Some(action) = config
                .actions
                .iter_mut()
                .find(|action| action.id == action_id)
            {
                action.enabled = true;
                action.params.insert("source".into(), display.clone());
            }
            config.save(&self.config_path)
        };
        match result {
            Ok(()) => {
                let disconnected = self.proxy.disconnect_clients();
                self.push_recent(format!(
                    "已选择并启用：{display}；已刷新 {disconnected} 条连接"
                ));
            }
            Err(error) => self.push_recent(format!("保存导入文件失败：{error:#}")),
        }
        cx.notify();
    }

    fn show_action_toast(
        &mut self,
        group: ActionEventGroup,
        image: Option<ImagePresentation>,
        cx: &mut Context<Self>,
    ) {
        let duration_ms = image
            .as_ref()
            .map(|image| image.duration_ms)
            .unwrap_or(4_500)
            .max(750);
        self.action_toast = Some(ActionToast { group, image });
        self.toast_generation = self.toast_generation.wrapping_add(1);
        let generation = self.toast_generation;
        cx.spawn(async move |this, cx| {
            Timer::after(Duration::from_millis(duration_ms)).await;
            let _ = this.update(cx, |view, cx| {
                if view.toast_generation == generation {
                    view.action_toast = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn reload_stats_inner(&mut self) {
        match self.analytics.dashboard(self.chart_range) {
            Ok(stats) => self.stats = stats,
            Err(error) => self.push_recent(format!("读取统计失败：{error:#}")),
        }
    }

    fn reload_action_groups_inner(&mut self) {
        let retention_days = self
            .config
            .read()
            .expect("config lock")
            .analytics
            .detailed_retention_days;
        match self.analytics.recent_action_groups(20, 30, retention_days) {
            Ok(groups) => self.action_groups = groups,
            Err(error) => self.push_recent(format!("读取 Action 响应失败：{error:#}")),
        }
    }

    fn update_tray_badge(&self, cx: &mut Context<Self>) {
        let Some(tray) = cx.try_global::<TrayGlobal>() else {
            return;
        };
        let unread = self.unread_action_groups;
        let _ = tray.icon.set_icon(tray_icon_image(unread > 0).ok());
        let tooltip = if unread == 0 {
            "Net Sentinel · 无未读响应".to_string()
        } else if unread > 99 {
            "Net Sentinel · 99+ 条未读响应".to_string()
        } else {
            format!("Net Sentinel · {unread} 条未读响应")
        };
        let _ = tray.icon.set_tooltip(Some(tooltip));
    }

    fn clear_action_unread(&mut self, cx: &mut Context<Self>) {
        self.unread_action_groups = 0;
        self.action_toast = None;
        self.toast_generation = self.toast_generation.wrapping_add(1);
        self.update_tray_badge(cx);
    }

    fn show_overview(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.page = Page::Overview;
        self.reload_stats_inner();
        cx.notify();
    }

    fn show_rules(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.page = Page::Rules;
        cx.notify();
    }

    fn show_actions(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.page = Page::Actions;
        self.reload_action_groups_inner();
        self.clear_action_unread(cx);
        cx.notify();
    }

    fn show_settings(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.page = Page::Settings;
        cx.notify();
    }

    fn set_chart_range(&mut self, range: ChartRange, cx: &mut Context<Self>) {
        self.chart_range = range;
        self.reload_stats_inner();
        cx.notify();
    }

    fn select_rule(&mut self, index: usize, cx: &mut Context<Self>) {
        let rule = self
            .config
            .read()
            .expect("config lock")
            .blacklist
            .get(index)
            .cloned();
        if let Some(rule) = rule {
            self.selected_rule = Some(index);
            self.draft_target = rule.target;
            self.draft_mode = rule.mode;
            self.draft_enabled = rule.enabled;
            self.draft_actions = rule.actions.into_iter().take(1).collect();
            self.rule_id_input
                .update(cx, |input, cx| input.set_value(rule.id, cx));
            self.rule_pattern_input
                .update(cx, |input, cx| input.set_value(rule.pattern, cx));
            self.rule_test_result = "规则已载入，可编辑、测试并保存".into();
            cx.notify();
        }
    }

    fn new_rule(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.selected_rule = None;
        self.draft_target = MatchTarget::Host;
        self.draft_mode = MatchMode::Contains;
        self.draft_enabled = true;
        self.draft_actions = self
            .config
            .read()
            .expect("config lock")
            .actions
            .iter()
            .filter(|action| action.enabled)
            .map(|action| action.id.clone())
            .take(1)
            .collect();
        let next = self.config.read().expect("config lock").blacklist.len() + 1;
        self.rule_id_input
            .update(cx, |input, cx| input.set_value(format!("rule-{next}"), cx));
        self.rule_pattern_input
            .update(cx, |input, cx| input.set_value("", cx));
        self.rule_test_result = "正在创建新规则".into();
        cx.notify();
    }

    fn toggle_draft_action(&mut self, action_id: String, cx: &mut Context<Self>) {
        self.draft_actions.clear();
        self.draft_actions.push(action_id);
        cx.notify();
    }

    fn draft_rule(&self, cx: &App) -> Result<Rule> {
        let id = self.rule_id_input.read(cx).value().trim().to_string();
        let pattern = self.rule_pattern_input.read(cx).value().trim().to_string();
        anyhow::ensure!(!id.is_empty(), "规则名称不能为空");
        anyhow::ensure!(!pattern.is_empty(), "匹配内容不能为空");
        let rule = Rule {
            id,
            enabled: self.draft_enabled,
            target: self.draft_target,
            mode: self.draft_mode,
            pattern,
            methods: Vec::new(),
            header_name: None,
            actions: self.draft_actions.iter().take(1).cloned().collect(),
        };
        let facts = RequestFacts {
            method: "GET".into(),
            host: "validation.invalid".into(),
            url: "http://validation.invalid/".into(),
            path: "/".into(),
            headers: Vec::new(),
        };
        let _ = rule_matches(&rule, &facts)?;
        Ok(rule)
    }

    fn save_rule(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        let rule = match self.draft_rule(cx) {
            Ok(rule) => rule,
            Err(error) => {
                self.rule_test_result = format!("无法保存：{error:#}");
                cx.notify();
                return;
            }
        };
        let mut config = self.config.write().expect("config lock");
        if config
            .blacklist
            .iter()
            .enumerate()
            .any(|(index, item)| Some(index) != self.selected_rule && item.id == rule.id)
        {
            drop(config);
            self.rule_test_result = "无法保存：规则名称必须唯一".into();
            cx.notify();
            return;
        }
        let index = if let Some(index) = self.selected_rule {
            config.blacklist[index] = rule;
            index
        } else {
            config.blacklist.push(rule);
            config.blacklist.len() - 1
        };
        let result = config.save(&self.config_path);
        drop(config);
        match result {
            Ok(()) => {
                let disconnected = self.proxy.disconnect_clients();
                self.selected_rule = Some(index);
                self.rule_test_result = if disconnected == 0 {
                    "规则已保存并立即生效".into()
                } else {
                    format!("规则已保存并立即生效；已刷新 {disconnected} 条现有连接")
                };
            }
            Err(error) => self.rule_test_result = format!("保存失败：{error:#}"),
        }
        cx.notify();
    }

    fn delete_rule(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self.selected_rule else {
            self.rule_test_result = "新规则尚未保存，无需删除".into();
            cx.notify();
            return;
        };
        let result = {
            let mut config = self.config.write().expect("config lock");
            if index < config.blacklist.len() {
                config.blacklist.remove(index);
            }
            config.save(&self.config_path)
        };
        match result {
            Ok(()) => {
                let disconnected = self.proxy.disconnect_clients();
                self.selected_rule = None;
                self.draft_target = MatchTarget::Host;
                self.draft_mode = MatchMode::Contains;
                self.draft_enabled = true;
                self.draft_actions.clear();
                self.rule_id_input
                    .update(cx, |input, cx| input.set_value("", cx));
                self.rule_pattern_input
                    .update(cx, |input, cx| input.set_value("", cx));
                self.rule_test_result = if disconnected == 0 {
                    "规则已删除并立即生效".into()
                } else {
                    format!("规则已删除并立即生效；已刷新 {disconnected} 条现有连接")
                };
            }
            Err(error) => self.rule_test_result = format!("删除失败：{error:#}"),
        }
        cx.notify();
    }

    fn test_rule(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        let rule = match self.draft_rule(cx) {
            Ok(rule) => rule,
            Err(error) => {
                self.rule_test_result = format!("规则无效：{error:#}");
                cx.notify();
                return;
            }
        };
        if matches!(rule.target, MatchTarget::Header) {
            self.rule_test_result = "Header 规则需要通过实际请求验证".into();
            cx.notify();
            return;
        }
        let raw = self.rule_test_input.read(cx).value();
        let parsed = match Url::parse(raw.trim()) {
            Ok(parsed) => parsed,
            Err(error) => {
                self.rule_test_result = format!("测试 URL 无效：{error}");
                cx.notify();
                return;
            }
        };
        let facts = RequestFacts {
            method: "GET".into(),
            host: parsed.host_str().unwrap_or_default().to_lowercase(),
            url: parsed.to_string(),
            path: match parsed.query() {
                Some(query) => format!("{}?{query}", parsed.path()),
                None => parsed.path().to_string(),
            },
            headers: Vec::new(),
        };
        self.rule_test_result = match rule_matches(&rule, &facts) {
            Ok(true) => "测试结果：命中，将被拦截".into(),
            Ok(false) => "测试结果：未命中，将放行".into(),
            Err(error) => format!("测试失败：{error:#}"),
        };
        cx.notify();
    }

    fn cycle_network_mode(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        let result = {
            let mut config = self.config.write().expect("config lock");
            config.proxy.network_mode = match config.proxy.network_mode {
                NetworkMode::Auto => NetworkMode::Direct,
                NetworkMode::Direct => NetworkMode::Http,
                NetworkMode::Http => NetworkMode::Socks5,
                NetworkMode::Socks5 => NetworkMode::Auto,
            };
            config.save(&self.config_path)
        };
        if let Err(error) = result {
            self.push_recent(format!("保存网络模式失败：{error:#}"));
        }
        cx.notify();
    }

    fn save_upstream(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        let value = self.upstream_input.read(cx).value().trim().to_string();
        let (result, mode, upstream, listen) = {
            let mut config = self.config.write().expect("config lock");
            config.proxy.upstream_proxy = (!value.is_empty()).then_some(value);
            (
                config.save(&self.config_path),
                config.proxy.network_mode,
                config.proxy.upstream_proxy.clone(),
                config.proxy.listen,
            )
        };
        let saved = result.is_ok();
        self.push_recent(match result {
            Ok(()) => "上游代理已保存；新连接立即使用".into(),
            Err(error) => format!("保存上游代理失败：{error:#}"),
        });
        if saved {
            let tx = self.ui_tx.clone();
            std::thread::spawn(move || {
                let message = match proxy::probe_upstream(mode, upstream.as_deref(), listen) {
                    Ok(detail) => format!("上游健康检查通过：{detail}"),
                    Err(error) => format!("上游不可用，已保持故障关闭：{error:#}"),
                };
                let _ = tx.send_blocking(UiEvent::NetworkProbe(message));
            });
        }
        cx.notify();
    }

    fn toggle_detailed_logging(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        let result = {
            let mut config = self.config.write().expect("config lock");
            config.analytics.detailed_logging = !config.analytics.detailed_logging;
            config.save(&self.config_path)
        };
        if let Err(error) = result {
            self.push_recent(format!("保存日志设置失败：{error:#}"));
        }
        cx.notify();
    }

    fn cycle_detail_retention(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        let result = {
            let mut config = self.config.write().expect("config lock");
            config.analytics.detailed_retention_days =
                match config.analytics.detailed_retention_days {
                    1 => 7,
                    7 => 30,
                    _ => 1,
                };
            let result = config.save(&self.config_path);
            let _ = self.analytics.maintain(
                config.analytics.detailed_retention_days,
                config.analytics.aggregate_retention_days,
            );
            result
        };
        if let Err(error) = result {
            self.push_recent(format!("保存保留策略失败：{error:#}"));
        }
        cx.notify();
    }

    fn cycle_aggregate_retention(
        &mut self,
        _: &ClickEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let result = {
            let mut config = self.config.write().expect("config lock");
            config.analytics.aggregate_retention_days =
                match config.analytics.aggregate_retention_days {
                    30 => 90,
                    90 => 180,
                    _ => 30,
                };
            let result = config.save(&self.config_path);
            let _ = self.analytics.maintain(
                config.analytics.detailed_retention_days,
                config.analytics.aggregate_retention_days,
            );
            result
        };
        if let Err(error) = result {
            self.push_recent(format!("保存保留策略失败：{error:#}"));
        }
        cx.notify();
    }

    fn clear_stats(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        match self.analytics.clear() {
            Ok(()) => {
                self.reload_stats_inner();
                self.reload_action_groups_inner();
                self.push_recent("统计数据已清空".into());
            }
            Err(error) => self.push_recent(format!("清空统计失败：{error:#}")),
        }
        cx.notify();
    }

    fn export_stats(&mut self, _: &ClickEvent, _: &mut Window, _: &mut Context<Self>) {
        let tx = self.ui_tx.clone();
        std::thread::spawn(move || {
            if let Some(path) = rfd::FileDialog::new()
                .set_title("导出 Net Sentinel 统计")
                .set_file_name("net-sentinel-stats.csv")
                .add_filter("CSV", &["csv"])
                .save_file()
            {
                let _ = tx.send_blocking(UiEvent::ExportStats(path));
            }
        });
    }

    fn cleanup_system_proxy(&mut self) {
        self.proxy.stop();
        let listen = self.config.read().expect("config lock").proxy.listen;
        if self.system_proxy_active || system::is_our_proxy(&listen.to_string()) {
            let _ = system::restore_system_proxy();
            self.system_proxy_active = false;
        }
    }
}

impl Drop for AppView {
    fn drop(&mut self) {
        self.cleanup_system_proxy();
    }
}

impl AppView {
    #[allow(dead_code)]
    fn render_legacy(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let config = self.config.read().expect("config lock").clone();
        let active_rules = config.blacklist.iter().filter(|rule| rule.enabled).count();
        let active_actions = config
            .actions
            .iter()
            .filter(|action| action.enabled)
            .count();
        let status_color = if self.proxy_running { SUCCESS } else { DANGER };

        div()
            .id("root-scroll")
            .size_full()
            .bg(rgb(BG))
            .text_color(rgb(TEXT))
            .font_family("Segoe UI")
            .overflow_y_scroll()
            .child(
                div()
                    .max_w(px(1160.0))
                    .mx_auto()
                    .p_8()
                    .flex()
                    .flex_col()
                    .gap_6()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .child(
                                        div()
                                            .text_2xl()
                                            .font_weight(gpui::FontWeight::BOLD)
                                            .child("NET SENTINEL"),
                                    )
                                    .child(
                                        div()
                                            .mt_1()
                                            .text_sm()
                                            .text_color(rgb(MUTED))
                                            .child("本机 HTTP 请求拦截器 · GPUI"),
                                    ),
                            )
                            .child(
                                div()
                                    .id("hide-to-tray")
                                    .cursor_pointer()
                                    .px_4()
                                    .py_2()
                                    .rounded_lg()
                                    .bg(rgb(PANEL_ALT))
                                    .hover(|style| style.bg(rgb(BORDER)))
                                    .child("隐藏到托盘")
                                    .on_click(cx.listener(Self::hide_to_tray)),
                            ),
                    )
                    .child(
                        div()
                            .p_6()
                            .rounded_2xl()
                            .bg(rgb(PANEL))
                            .border_1()
                            .border_color(rgb(BORDER))
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_4()
                                    .child(div().size_3().rounded_full().bg(rgb(status_color)))
                                    .child(
                                        div()
                                            .child(
                                                div()
                                                    .text_lg()
                                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                                    .child(if self.proxy_running {
                                                        "拦截引擎运行中"
                                                    } else {
                                                        "拦截引擎已停止"
                                                    }),
                                            )
                                            .child(
                                                div()
                                                    .mt_1()
                                                    .text_sm()
                                                    .text_color(rgb(MUTED))
                                                    .child(self.proxy_detail.clone()),
                                            ),
                                    ),
                            )
                            .child(
                                div()
                                    .id("toggle-proxy")
                                    .cursor_pointer()
                                    .px_5()
                                    .py_3()
                                    .rounded_xl()
                                    .bg(rgb(if self.proxy_running { DANGER } else { ACCENT }))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .child(if self.proxy_running { "停止监听" } else { "启动监听" })
                                    .on_click(cx.listener(Self::toggle_proxy)),
                            ),
                    )
                    .child(
                        div()
                            .grid()
                            .grid_cols(3)
                            .gap_4()
                            .child(metric(
                                "累计拦截",
                                self.stats
                                    .series
                                    .iter()
                                    .map(|point| point.count)
                                    .sum::<u64>()
                                    .to_string(),
                                "当前图表范围",
                            ))
                            .child(metric("启用规则", active_rules.to_string(), "blacklist"))
                            .child(metric("启用动作", active_actions.to_string(), "可扩展 registry")),
                    )
                    .child(section_title("网络接管", "先运行监听，再让 Windows 流量经过本机代理"))
                    .child(
                        div()
                            .grid()
                            .grid_cols(2)
                            .gap_4()
                            .child(setting_card(
                                "Windows 系统代理",
                                if self.system_proxy_active { "已接管" } else { "未接管" },
                                "只修改当前用户设置，启用前会备份原值。退出或停止时自动恢复。",
                                "toggle-system-proxy",
                                if self.system_proxy_active { "恢复系统代理" } else { "接管系统代理" },
                                cx.listener(Self::toggle_system_proxy),
                            ))
                            .child(setting_card(
                                "开机启动",
                                if self.startup_enabled { "已启用" } else { "未启用" },
                                "登录 Windows 后静默启动到系统托盘。无需管理员权限。",
                                "toggle-startup",
                                if self.startup_enabled { "关闭开机启动" } else { "启用开机启动" },
                                cx.listener(Self::toggle_startup),
                            )),
                    )
                    .child(section_title("HTTP 匹配规则 · BLACKLIST", "按 host、完整 URL、path 或 header 匹配；支持 exact / contains / glob / regex"))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_3()
                            .children(config.blacklist.iter().map(rule_row)),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_3()
                            .child(small_button("open-config", "编辑 config.toml", cx.listener(Self::open_config)))
                            .child(small_button("reload-config", "重新载入配置", cx.listener(Self::reload_config))),
                    )
                    .child(section_title("ACTIONS", "匹配器只分发动作；图片、音乐由独立 handler 执行"))
                    .child(
                        div()
                            .grid()
                            .grid_cols(2)
                            .gap_4()
                            .child(action_card(
                                "弹出图片",
                                "popup_image",
                                "内置拦截图，也支持 PNG / JPG / WebP / GIF / SVG。",
                                "内置默认".into(),
                                (
                                    "preview-image",
                                    "预览",
                                    cx.listener(Self::test_image),
                                ),
                                (
                                    "import-image",
                                    "导入图片",
                                    cx.listener(Self::import_image),
                                ),
                                (
                                    "default-image",
                                    "内置默认",
                                    cx.listener(Self::use_default_image),
                                ),
                            ))
                            .child(action_card(
                                "播放音乐",
                                "play_audio",
                                "内置 3 首原创提示旋律，也支持 WAV / MP3 / FLAC / OGG。",
                                "内置默认".into(),
                                (
                                    "preview-audio",
                                    "试听内置音乐",
                                    cx.listener(Self::test_audio),
                                ),
                                (
                                    "import-audio",
                                    "导入音乐",
                                    cx.listener(Self::import_audio),
                                ),
                                (
                                    "default-audio",
                                    "内置默认",
                                    cx.listener(Self::use_default_audio),
                                ),
                            )),
                    )
                    .child(section_title("最近事件", "仅保存在本次运行的内存中"))
                    .child(
                        div()
                            .p_4()
                            .rounded_xl()
                            .bg(rgb(PANEL))
                            .border_1()
                            .border_color(rgb(BORDER))
                            .children(if self.recent.is_empty() {
                                vec![div().text_color(rgb(MUTED)).child("暂无事件")]
                            } else {
                                self.recent
                                    .iter()
                                    .map(|event| {
                                        div()
                                            .py_2()
                                            .border_b_1()
                                            .border_color(rgb(BORDER))
                                            .text_sm()
                                            .child(event.clone())
                                    })
                                    .collect()
                            }),
                    )
                    .child(
                        div()
                            .pb_6()
                            .text_sm()
                            .text_color(rgb(MUTED))
                            .child("HTTPS 在无根证书模式下仅匹配 CONNECT 域名，不解密路径与内容。"),
                    ),
            )
    }
}

impl AppView {
    fn render_workspace(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        match self.page {
            Page::Overview => self.render_overview(cx),
            Page::Rules => self.render_rules_page(cx),
            Page::Actions => self.render_actions_page(cx),
            Page::Settings => self.render_settings_page(cx),
        }
    }

    fn render_overview(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let stats = self.stats.clone();
        let water_active = self.system_proxy_active || self.protection_pending;
        let (status_label, action_label) = if self.system_proxy_active {
            ("保护运行中", "点击停止保护")
        } else if self.protection_pending {
            ("正在检查网络", "正在启动保护")
        } else {
            ("保护已关闭", "点击启动保护")
        };
        div()
            .w_full()
            .flex()
            .flex_col()
            .items_center()
            .gap_5()
            .child(
                div()
                    .id("water-protection-toggle")
                    .size(px(304.0))
                    .rounded_full()
                    .overflow_hidden()
                    .relative()
                    .shadow_lg()
                    .cursor_pointer()
                    .hover(|style| style.opacity(0.94))
                    .active(|style| style.opacity(0.86))
                    .child(water_surface(self.wave_phase, water_active))
                    .child(
                        div()
                            .absolute()
                            .top(px(12.0))
                            .bottom(px(12.0))
                            .left(px(12.0))
                            .right(px(12.0))
                            .rounded_full()
                            .border_1()
                            .border_color(rgba(if water_active { 0x268ebf66 } else { 0x7a8c9966 })),
                    )
                    .child(
                        div()
                            .absolute()
                            .top(px(34.0))
                            .left_0()
                            .right_0()
                            .flex()
                            .justify_center()
                            .child(
                                div()
                                    .px_3()
                                    .py_1()
                                    .rounded_full()
                                    .bg(rgba(0xffffffc7))
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .text_sm()
                                    .text_color(rgb(MUTED))
                                    .child(div().size_2().rounded_full().bg(rgb(if water_active {
                                        SUCCESS
                                    } else {
                                        MUTED
                                    })))
                                    .child(status_label),
                            ),
                    )
                    .child(
                        div()
                            .absolute()
                            .top(px(82.0))
                            .left_0()
                            .right_0()
                            .flex()
                            .justify_center()
                            .child(
                                div()
                                    .size(px(58.0))
                                    .rounded_full()
                                    .border_2()
                                    .border_color(rgb(if water_active { 0x1787c9 } else { ACCENT }))
                                    .bg(rgba(0xffffffb8))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .font_weight(gpui::FontWeight::BOLD)
                                    .text_color(rgb(if water_active { 0x126f9f } else { ACCENT }))
                                    .child("N/S"),
                            ),
                    )
                    .child(
                        div()
                            .absolute()
                            .top(px(151.0))
                            .left_0()
                            .right_0()
                            .flex()
                            .flex_col()
                            .items_center()
                            .text_center()
                            .child(
                                div()
                                    .text_lg()
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .child(action_label),
                            )
                            .child(
                                div()
                                    .mt_2()
                                    .max_w(px(220.0))
                                    .text_sm()
                                    .text_color(rgb(MUTED))
                                    .child(self.proxy_detail.clone()),
                            ),
                    )
                    .on_click(cx.listener(Self::toggle_protection)),
            )
            .child(
                div()
                    .w_full()
                    .max_w(px(860.0))
                    .p_4()
                    .bg(rgb(PANEL))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .rounded_md()
                    .child(
                        div()
                            .flex()
                            .justify_between()
                            .items_center()
                            .child(
                                div()
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .child("拦截趋势"),
                            )
                            .child(
                                div()
                                    .flex()
                                    .gap_2()
                                    .child(range_button(
                                        "range-day",
                                        "24 小时",
                                        self.chart_range == ChartRange::Day,
                                        cx.listener(|view, _, _, cx| {
                                            view.set_chart_range(ChartRange::Day, cx)
                                        }),
                                    ))
                                    .child(range_button(
                                        "range-week",
                                        "7 天",
                                        self.chart_range == ChartRange::Week,
                                        cx.listener(|view, _, _, cx| {
                                            view.set_chart_range(ChartRange::Week, cx)
                                        }),
                                    ))
                                    .child(range_button(
                                        "range-month",
                                        "30 天",
                                        self.chart_range == ChartRange::Month,
                                        cx.listener(|view, _, _, cx| {
                                            view.set_chart_range(ChartRange::Month, cx)
                                        }),
                                    )),
                            ),
                    )
                    .child(trend_chart(&stats.series)),
            )
            .into_any_element()
    }

    fn render_rules_page(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let config = self.config.read().expect("config lock").clone();
        let rows = config
            .blacklist
            .iter()
            .enumerate()
            .map(|(index, rule)| {
                let selected = self.selected_rule == Some(index);
                div()
                    .id(("rule-select", index))
                    .cursor_pointer()
                    .p_3()
                    .border_b_1()
                    .border_color(rgb(BORDER))
                    .bg(rgb(if selected { PANEL_ALT } else { PANEL }))
                    .hover(|style| style.bg(rgb(PANEL_ALT)))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(div().size_2().rounded_full().bg(rgb(if rule.enabled {
                                SUCCESS
                            } else {
                                MUTED
                            })))
                            .child(
                                div()
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .child(rule.id.clone()),
                            ),
                    )
                    .child(
                        div()
                            .mt_1()
                            .text_sm()
                            .text_color(rgb(MUTED))
                            .child(rule.pattern.clone()),
                    )
                    .on_click(cx.listener(move |view, _, _, cx| view.select_rule(index, cx)))
                    .into_any_element()
            })
            .collect::<Vec<_>>();
        let action_buttons = config
            .actions
            .iter()
            .enumerate()
            .map(|(index, action)| {
                let id = action.id.clone();
                let active = self.draft_actions.contains(&id);
                div()
                    .id(("draft-action", index))
                    .cursor_pointer()
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(if active { ACCENT } else { BORDER }))
                    .bg(rgb(if active { ACCENT } else { PANEL }))
                    .text_sm()
                    .text_color(rgb(if active { 0xffffff } else { TEXT }))
                    .child(if active {
                        format!("●  {}", action.id)
                    } else {
                        action.id.clone()
                    })
                    .on_click(
                        cx.listener(move |view, _, _, cx| view.toggle_draft_action(id.clone(), cx)),
                    )
                    .into_any_element()
            })
            .collect::<Vec<_>>();
        let target_options = [
            (
                MatchTarget::Host,
                "Host",
                "匹配请求域名；HTTPS CONNECT 也能识别",
                "select-target-host",
            ),
            (
                MatchTarget::Url,
                "完整 URL",
                "匹配协议、域名、路径及查询参数",
                "select-target-url",
            ),
            (
                MatchTarget::Path,
                "Path / Query",
                "匹配路径与查询参数；仅适用于可见的 HTTP 请求",
                "select-target-path",
            ),
            (
                MatchTarget::Header,
                "Header",
                "匹配指定请求头；仅适用于可见的 HTTP 请求",
                "select-target-header",
            ),
        ]
        .into_iter()
        .map(|(value, label, description, id)| {
            segmented_option(
                id,
                label,
                self.draft_target == value,
                Some(description),
                cx.listener(move |view, _, _, cx| {
                    view.draft_target = value;
                    cx.notify();
                }),
            )
            .into_any_element()
        })
        .collect::<Vec<_>>();
        let mode_options = [
            (
                MatchMode::Exact,
                "Exact",
                "目标值必须与规则内容完全一致",
                "select-mode-exact",
            ),
            (
                MatchMode::Contains,
                "Contains",
                "目标值中包含规则文本即可命中",
                "select-mode-contains",
            ),
            (
                MatchMode::Glob,
                "Glob",
                "使用 * 和 ? 通配符进行匹配",
                "select-mode-glob",
            ),
            (
                MatchMode::Regex,
                "Regex",
                "使用正则表达式进行匹配",
                "select-mode-regex",
            ),
        ]
        .into_iter()
        .map(|(value, label, description, id)| {
            segmented_option(
                id,
                label,
                self.draft_mode == value,
                Some(description),
                cx.listener(move |view, _, _, cx| {
                    view.draft_mode = value;
                    cx.notify();
                }),
            )
            .into_any_element()
        })
        .collect::<Vec<_>>();
        div()
            .flex()
            .flex_col()
            .gap_4()
            .child(page_heading("规则"))
            .child(
                div()
                    .flex()
                    .gap_3()
                    .min_h(px(560.0))
                    .child(
                        div()
                            .w(px(320.0))
                            .min_w(px(320.0))
                            .flex_none()
                            .bg(rgb(PANEL))
                            .border_1()
                            .border_color(rgb(BORDER))
                            .rounded_md()
                            .overflow_hidden()
                            .child(
                                div()
                                    .p_3()
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .child(
                                        div()
                                            .text_sm()
                                            .text_color(rgb(MUTED))
                                            .child(format!("{} 条规则", rows.len())),
                                    )
                                    .child(small_button(
                                        "new-rule",
                                        "+ 新建",
                                        cx.listener(Self::new_rule),
                                    )),
                            )
                            .children(rows),
                    )
                    .child(
                        div()
                            .flex_1()
                            .bg(rgb(PANEL))
                            .border_1()
                            .border_color(rgb(BORDER))
                            .rounded_md()
                            .p_5()
                            .flex()
                            .flex_col()
                            .gap_4()
                            .child(input_field("规则名称", self.rule_id_input.clone()))
                            .child(input_field("匹配内容", self.rule_pattern_input.clone()))
                            .child(segmented_row("匹配对象", target_options))
                            .child(segmented_row("匹配模式", mode_options))
                            .child(switch_row(
                                "状态",
                                "启用规则",
                                self.draft_enabled,
                                cx.listener(|view, _, _, cx| {
                                    view.draft_enabled = !view.draft_enabled;
                                    cx.notify();
                                }),
                            ))
                            .child(div().text_sm().text_color(rgb(MUTED)).child("触发 Actions"))
                            .child(div().flex().flex_wrap().gap_2().children(action_buttons))
                            .child(
                                div()
                                    .p_4()
                                    .rounded_md()
                                    .border_1()
                                    .border_color(rgb(BORDER))
                                    .bg(rgb(BG))
                                    .child(
                                        div()
                                            .mb_3()
                                            .font_weight(gpui::FontWeight::SEMIBOLD)
                                            .child("规则测试"),
                                    )
                                    .child(input_field("测试 URL", self.rule_test_input.clone()))
                                    .child(
                                        div()
                                            .mt_3()
                                            .p_3()
                                            .rounded_md()
                                            .bg(rgb(PANEL))
                                            .text_sm()
                                            .text_color(rgb(
                                                if self.rule_test_result.contains("命中") {
                                                    SUCCESS
                                                } else {
                                                    MUTED
                                                },
                                            ))
                                            .child(self.rule_test_result.clone()),
                                    ),
                            )
                            .child(
                                div()
                                    .flex()
                                    .gap_2()
                                    .child(primary_button(
                                        "test-rule",
                                        "测试规则",
                                        cx.listener(Self::test_rule),
                                    ))
                                    .child(primary_button(
                                        "save-rule",
                                        "保存并生效",
                                        cx.listener(Self::save_rule),
                                    ))
                                    .child(danger_button(
                                        "delete-rule",
                                        "删除",
                                        cx.listener(Self::delete_rule),
                                    )),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn render_actions_page(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let config = self.config.read().expect("config lock").clone();
        let image_resource = action_resource_label(&config, "blocked-picture");
        let audio_resource = action_resource_label(&config, "blocked-sound");
        let html_resource = action_resource_label(&config, "blocked-game");
        div()
            .flex()
            .flex_col()
            .gap_4()
            .child(page_heading("Actions"))
            .child(
                div()
                    .grid()
                    .grid_cols(3)
                    .gap_3()
                    .child(action_card(
                        "应用内图片",
                        "popup_image",
                        "窗口可见时在右上角轻提示；隐藏时仅记录并更新托盘角标。",
                        image_resource,
                        ("preview-image-v2", "预览", cx.listener(Self::test_image)),
                        (
                            "import-image-v2",
                            "选择图片",
                            cx.listener(Self::import_image),
                        ),
                        (
                            "default-image-v2",
                            "内置默认",
                            cx.listener(Self::use_default_image),
                        ),
                    ))
                    .child(action_card(
                        "播放音乐",
                        "play_audio",
                        "在本机播放，不改变浏览器网页；支持内置旋律及 WAV / MP3 / FLAC / OGG。",
                        audio_resource,
                        ("preview-audio-v2", "试听", cx.listener(Self::test_audio)),
                        (
                            "import-audio-v2",
                            "选择音乐",
                            cx.listener(Self::import_audio),
                        ),
                        (
                            "default-audio-v2",
                            "内置默认",
                            cx.listener(Self::use_default_audio),
                        ),
                    ))
                    .child(action_card(
                        "HTML 小游戏",
                        "serve_html",
                        "替换浏览器当前的明文 HTTP 页面；HTTPS 不进行证书劫持。",
                        html_resource,
                        ("preview-html-v2", "使用说明", cx.listener(Self::test_html)),
                        (
                            "import-html-v2",
                            "选择 HTML",
                            cx.listener(Self::import_html),
                        ),
                        (
                            "default-html-v2",
                            "内置默认",
                            cx.listener(Self::use_default_html),
                        ),
                    )),
            )
            .when(self.html_help_visible, |page| {
                page.child(
                    div()
                        .p_4()
                        .rounded_lg()
                        .border_1()
                        .border_color(rgb(0x9db6cb))
                        .bg(rgb(0xf0f6fa))
                        .child(
                            div()
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .child("HTML Action 使用说明"),
                        )
                        .child(
                            div()
                                .mt_2()
                                .text_sm()
                                .text_color(rgb(MUTED))
                                .child("将 HTML Action 绑定到一条规则后，明文 HTTP 命中会在当前浏览器标签中显示所选页面。HTTPS 不解密内容，因此会标记为不支持并直接阻断连接。"),
                        ),
                )
            })
            .into_any_element()
    }

    fn render_settings_page(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let config = self.config.read().expect("config lock").clone();
        let network_mode = network_mode_label(config.proxy.network_mode);
        let content = match self.settings_section {
            SettingsSection::Windows => div()
                .flex()
                .flex_col()
                .gap_4()
                .child(page_heading("Windows 集成"))
                .child(
                    div()
                        .grid()
                        .grid_cols(2)
                        .gap_3()
                        .child(setting_card("Windows 系统代理", if self.system_proxy_active { "已接管" } else { "未接管" }, "完成 Clash 与本机转发双重检查后才允许接管。", "toggle-system-proxy-v2", if self.system_proxy_active { "恢复" } else { "安全接管" }, cx.listener(Self::toggle_system_proxy)))
                        .child(setting_card("开机启动", if self.startup_enabled { "已启用" } else { "未启用" }, "只启动界面到托盘，不会自动监听或修改系统代理。", "toggle-startup-v2", if self.startup_enabled { "关闭" } else { "启用" }, cx.listener(Self::toggle_startup))),
                )
                .into_any_element(),
            SettingsSection::Network => div()
                .flex()
                .flex_col()
                .gap_4()
                .child(page_heading("出口路由"))
                .child(
                    div()
                        .p_4()
                        .bg(rgb(PANEL))
                        .border_1()
                        .border_color(rgb(BORDER))
                        .rounded_md()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .child(choice_button("network-mode", "模式（点击切换）", network_mode, cx.listener(Self::cycle_network_mode)))
                        .child(input_field("上游代理（HTTP / SOCKS5）", self.upstream_input.clone()))
                        .child(div().flex().gap_2().child(primary_button("save-upstream", "保存并验证上游", cx.listener(Self::save_upstream))))
                        .child(div().text_sm().text_color(rgb(MUTED)).child("启动保护前会先验证 GitHub HTTPS CONNECT，再验证 8877 的完整转发链路；任何失败都不会改写 Windows 系统代理。")),
                )
                .into_any_element(),
            SettingsSection::Data => div()
                .flex()
                .flex_col()
                .gap_4()
                .child(page_heading("数据与日志"))
                .child(
                    div()
                        .grid()
                        .grid_cols(3)
                        .gap_3()
                        .child(choice_button("detailed-log", "详细日志", if config.analytics.detailed_logging { "开启" } else { "关闭" }, cx.listener(Self::toggle_detailed_logging)))
                        .child(choice_button("detail-retention", "详细记录保留", format!("{} 天", config.analytics.detailed_retention_days), cx.listener(Self::cycle_detail_retention)))
                        .child(choice_button("aggregate-retention", "聚合统计保留", format!("{} 天", config.analytics.aggregate_retention_days), cx.listener(Self::cycle_aggregate_retention))),
                )
                .child(
                    div()
                        .flex()
                        .gap_2()
                        .child(small_button("export-stats", "导出统计", cx.listener(Self::export_stats)))
                        .child(danger_button("clear-stats", "清空统计", cx.listener(Self::clear_stats))),
                )
                .into_any_element(),
            SettingsSection::Advanced => div()
                .flex()
                .flex_col()
                .gap_4()
                .child(page_heading("高级"))
                .child(
                    div()
                        .flex()
                        .gap_2()
                        .child(small_button("open-config-v2", "打开配置文件", cx.listener(Self::open_config)))
                        .child(small_button("reload-config-v2", "重新载入", cx.listener(Self::reload_config))),
                )
                .child(div().p_4().rounded_md().bg(rgb(PANEL_ALT)).text_sm().text_color(rgb(MUTED)).child("旧版 enabled_on_launch 与 set_system_proxy_on_launch 配置已废弃；启动应用永远不会自动接管网络。"))
                .into_any_element(),
        };
        div()
            .flex()
            .min_h(px(620.0))
            .bg(rgb(PANEL))
            .border_1()
            .border_color(rgb(BORDER))
            .rounded_md()
            .overflow_hidden()
            .child(
                div()
                    .w(px(220.0))
                    .min_w(px(220.0))
                    .border_r_1()
                    .border_color(rgb(BORDER))
                    .p_3()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .px_3()
                            .py_3()
                            .text_lg()
                            .font_weight(gpui::FontWeight::BOLD)
                            .child("设置"),
                    )
                    .child(settings_nav_button(
                        "settings-windows",
                        "Windows 集成",
                        self.settings_section == SettingsSection::Windows,
                        cx.listener(|view, _, _, cx| {
                            view.settings_section = SettingsSection::Windows;
                            cx.notify();
                        }),
                    ))
                    .child(settings_nav_button(
                        "settings-network",
                        "出口路由",
                        self.settings_section == SettingsSection::Network,
                        cx.listener(|view, _, _, cx| {
                            view.settings_section = SettingsSection::Network;
                            cx.notify();
                        }),
                    ))
                    .child(settings_nav_button(
                        "settings-data",
                        "数据与日志",
                        self.settings_section == SettingsSection::Data,
                        cx.listener(|view, _, _, cx| {
                            view.settings_section = SettingsSection::Data;
                            cx.notify();
                        }),
                    ))
                    .child(settings_nav_button(
                        "settings-advanced",
                        "高级",
                        self.settings_section == SettingsSection::Advanced,
                        cx.listener(|view, _, _, cx| {
                            view.settings_section = SettingsSection::Advanced;
                            cx.notify();
                        }),
                    )),
            )
            .child(div().flex_1().min_w_0().p_5().child(content))
            .into_any_element()
    }
}

impl Render for AppView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .relative()
            .bg(rgb(BG))
            .text_color(rgb(TEXT))
            .font_family("Segoe UI")
            .flex()
            .flex_col()
            .child(
                div()
                    .h(px(40.0))
                    .bg(rgb(CHROME))
                    .border_b_1()
                    .border_color(rgb(BORDER))
                    .flex()
                    .items_center()
                    .child(if self.page == Page::Settings {
                        div()
                            .id("title-back")
                            .w(px(40.0))
                            .h_full()
                            .cursor_pointer()
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_lg()
                            .text_color(rgb(0x34383c))
                            .hover(|style| style.bg(rgb(PANEL)))
                            .child("←")
                            .on_click(cx.listener(Self::show_overview))
                    } else {
                        div()
                            .id("title-settings")
                            .w(px(40.0))
                            .h_full()
                            .cursor_pointer()
                            .flex()
                            .items_center()
                            .justify_center()
                            .hover(|style| style.bg(rgb(PANEL)))
                            .child("⚙")
                            .on_click(cx.listener(Self::show_settings))
                    })
                    .child(
                        div()
                            .h_full()
                            .px_3()
                            .flex()
                            .items_center()
                            .gap_3()
                            .text_sm()
                            .child(
                                div()
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .child("Net Sentinel"),
                            )
                            .when(self.page != Page::Rules, |title| {
                                title.child(
                                    div().text_color(rgb(MUTED)).child(page_label(self.page)),
                                )
                            }),
                    )
                    .child(
                        div()
                            .flex_1()
                            .h_full()
                            .window_control_area(WindowControlArea::Drag),
                    )
                    .child(titlebar_control(
                        "title-min",
                        TitlebarGlyph::Minimize,
                        WindowControlArea::Min,
                    ))
                    .child(titlebar_control(
                        "title-max",
                        TitlebarGlyph::Maximize,
                        WindowControlArea::Max,
                    ))
                    .child(titlebar_control(
                        "title-close",
                        TitlebarGlyph::Close,
                        WindowControlArea::Close,
                    )),
            )
            .child(
                div()
                    .id("main-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px_6()
                    .child(
                        div()
                            .w_full()
                            .max_w(px(1120.0))
                            .mx_auto()
                            .pt_5()
                            .pb_6()
                            .child(self.render_workspace(cx)),
                    ),
            )
            .child(
                div()
                    .h(px(36.0))
                    .px_2()
                    .bg(rgb(CHROME))
                    .border_t_1()
                    .border_color(rgb(BORDER))
                    .flex()
                    .items_center()
                    .justify_end()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .child(status_nav_button(
                                "nav-overview",
                                "◉",
                                "保护概览",
                                self.page == Page::Overview,
                                None,
                                cx.listener(Self::show_overview),
                            ))
                            .child(tab_separator())
                            .child(status_nav_button(
                                "nav-rules",
                                "⛨",
                                "拦截规则",
                                self.page == Page::Rules,
                                None,
                                cx.listener(Self::show_rules),
                            ))
                            .child(tab_separator())
                            .child(status_nav_button(
                                "nav-actions",
                                "ϟ",
                                "响应 Actions",
                                self.page == Page::Actions,
                                (self.unread_action_groups > 0).then(|| {
                                    if self.unread_action_groups > 99 {
                                        "99+".into()
                                    } else {
                                        self.unread_action_groups.to_string()
                                    }
                                }),
                                cx.listener(Self::show_actions),
                            )),
                    ),
            )
            .when_some(self.action_toast.clone(), |root, toast| {
                root.child(action_toast(toast, cx.listener(Self::show_actions)))
            })
    }
}

struct TooltipView {
    text: gpui::SharedString,
}

impl Render for TooltipView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .max_w(px(280.0))
            .px_3()
            .py_2()
            .rounded_md()
            .bg(rgb(0x30343a))
            .text_sm()
            .text_color(rgb(0xffffff))
            .child(self.text.clone())
    }
}

#[derive(Clone, Copy)]
enum TitlebarGlyph {
    Minimize,
    Maximize,
    Close,
}

fn page_heading(title: &'static str) -> impl IntoElement {
    div().child(
        div()
            .text_2xl()
            .font_weight(gpui::FontWeight::BOLD)
            .child(title),
    )
}

fn page_label(page: Page) -> &'static str {
    match page {
        Page::Overview => "Protection Overview",
        Page::Rules => "",
        Page::Actions => "Configure Response Actions",
        Page::Settings => "Application Settings",
    }
}

fn titlebar_control(
    id: &'static str,
    glyph: TitlebarGlyph,
    area: WindowControlArea,
) -> impl IntoElement {
    div()
        .id(id)
        .w(px(46.0))
        .h_full()
        .flex()
        .items_center()
        .justify_center()
        .hover(move |style| {
            style.bg(rgb(if matches!(glyph, TitlebarGlyph::Close) {
                0xe6a3a3
            } else {
                PANEL
            }))
        })
        .window_control_area(area)
        .child(
            canvas(
                move |_, _, _| glyph,
                move |bounds, glyph, window, _| {
                    let center_x = bounds.origin.x + bounds.size.width / 2.0;
                    let center_y = bounds.origin.y + bounds.size.height / 2.0;
                    let mut path = gpui::PathBuilder::stroke(px(1.25));
                    match glyph {
                        TitlebarGlyph::Minimize => {
                            path.move_to(point(center_x - px(5.0), center_y + px(3.0)));
                            path.line_to(point(center_x + px(5.0), center_y + px(3.0)));
                        }
                        TitlebarGlyph::Maximize => {
                            path.move_to(point(center_x - px(4.5), center_y - px(4.5)));
                            path.line_to(point(center_x + px(4.5), center_y - px(4.5)));
                            path.line_to(point(center_x + px(4.5), center_y + px(4.5)));
                            path.line_to(point(center_x - px(4.5), center_y + px(4.5)));
                            path.line_to(point(center_x - px(4.5), center_y - px(4.5)));
                        }
                        TitlebarGlyph::Close => {
                            path.move_to(point(center_x - px(4.5), center_y - px(4.5)));
                            path.line_to(point(center_x + px(4.5), center_y + px(4.5)));
                            path.move_to(point(center_x + px(4.5), center_y - px(4.5)));
                            path.line_to(point(center_x - px(4.5), center_y + px(4.5)));
                        }
                    }
                    if let Ok(path) = path.build() {
                        window.paint_path(path, rgb(0x282c30));
                    }
                },
            )
            .size(px(16.0)),
        )
}

fn status_nav_button(
    id: &'static str,
    icon: &'static str,
    label: &'static str,
    active: bool,
    badge: Option<String>,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .cursor_pointer()
        .h(px(28.0))
        .w(px(32.0))
        .px_2()
        .rounded_sm()
        .overflow_hidden()
        .bg(rgb(if active { PANEL } else { CHROME }))
        .text_sm()
        .text_color(rgb(if active { TEXT } else { 0x3c4146 }))
        .flex()
        .items_center()
        .hover(|style| style.w(px(122.0)).bg(rgb(PANEL)).text_color(rgb(TEXT)))
        .child(
            div()
                .relative()
                .w(px(16.0))
                .flex_none()
                .text_center()
                .text_color(rgb(if active { ACCENT } else { 0x2e3338 }))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .child(icon)
                .when_some(badge, |icon_box, badge| {
                    icon_box.child(
                        div()
                            .absolute()
                            .top(px(-8.0))
                            .right(px(-11.0))
                            .min_w(px(16.0))
                            .h(px(16.0))
                            .px_1()
                            .rounded_full()
                            .bg(rgb(DANGER))
                            .text_xs()
                            .text_color(rgb(0xffffff))
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(badge),
                    )
                }),
        )
        .child(div().ml_2().whitespace_nowrap().child(label))
        .on_click(on_click)
}

fn tab_separator() -> impl IntoElement {
    div().mx_1().w(px(1.0)).h(px(18.0)).bg(rgb(0x96999c))
}

fn settings_nav_button(
    id: &'static str,
    label: &'static str,
    active: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .cursor_pointer()
        .px_3()
        .py_2()
        .rounded_sm()
        .bg(rgb(if active { PANEL_ALT } else { PANEL }))
        .text_color(rgb(if active { TEXT } else { MUTED }))
        .hover(|style| style.bg(rgb(PANEL_ALT)).text_color(rgb(TEXT)))
        .child(label)
        .on_click(on_click)
}

fn range_button(
    id: &'static str,
    label: &'static str,
    active: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .cursor_pointer()
        .px_3()
        .py_1()
        .rounded_md()
        .bg(rgb(if active { ACCENT } else { PANEL_ALT }))
        .text_color(rgb(if active { 0xffffff } else { TEXT }))
        .text_sm()
        .child(label)
        .on_click(on_click)
}

fn water_surface(phase: f32, active: bool) -> impl IntoElement {
    div()
        .size_full()
        .rounded_full()
        .overflow_hidden()
        .bg(rgb(if active { 0xd9f2ff } else { 0xe9f3f8 }))
        .border_1()
        .border_color(rgb(if active { 0x8fcbe9 } else { BORDER }))
        .child(
            canvas(
                move |_, _, _| (phase, active),
                move |bounds, state, window, _| {
                    let (phase, active) = state;
                    let width = bounds.size.width;
                    let height = bounds.size.height;
                    let amplitude = if active { 22.0 } else { 7.0 };
                    let baseline = bounds.origin.y + height * 0.70;
                    let left = bounds.origin.x;
                    let center_x = bounds.origin.x + width / 2.0;
                    let center_y = bounds.origin.y + height / 2.0;
                    let radius = width.min(height) / 2.0;
                    let samples = 72usize;
                    let circle_edges = |x: gpui::Pixels| {
                        let normalized = ((x - center_x) / radius).clamp(-1.0_f32, 1.0_f32);
                        let extent = radius * (1.0_f32 - normalized * normalized).sqrt();
                        (center_y - extent, center_y + extent)
                    };
                    let mut fill_path = gpui::PathBuilder::fill();
                    for index in 0..=samples {
                        let ratio = index as f32 / samples as f32;
                        let x = left + width * ratio;
                        let raw_y = baseline
                            + px((ratio * std::f32::consts::TAU * 1.7 + phase).sin() * amplitude);
                        let (circle_top, circle_bottom) = circle_edges(x);
                        let y = if raw_y < circle_top {
                            circle_top
                        } else if raw_y > circle_bottom {
                            circle_bottom
                        } else {
                            raw_y
                        };
                        if index == 0 {
                            fill_path.move_to(point(x, y));
                        } else {
                            fill_path.line_to(point(x, y));
                        }
                    }
                    for index in (0..=samples).rev() {
                        let ratio = index as f32 / samples as f32;
                        let x = left + width * ratio;
                        let (_, circle_bottom) = circle_edges(x);
                        fill_path.line_to(point(x, circle_bottom));
                    }
                    fill_path.line_to(point(left, center_y));
                    if let Ok(path) = fill_path.build() {
                        window.paint_path(path, rgba(if active { 0x1787c9aa } else { 0x6ba5c477 }));
                    }
                    for (offset, alpha) in [(0.0, 0xcc), (34.0, 0x99), (66.0, 0x66)] {
                        let mut stroke = gpui::PathBuilder::stroke(px(2.0));
                        let mut pen_down = false;
                        for index in 0..=samples {
                            let ratio = index as f32 / samples as f32;
                            let x = left + width * ratio;
                            let y = baseline
                                + px(offset)
                                + px(
                                    (ratio * std::f32::consts::TAU * 2.0 + phase + offset / 24.0)
                                        .sin()
                                        * amplitude
                                        * 0.62,
                                );
                            let (circle_top, circle_bottom) = circle_edges(x);
                            if y < circle_top || y > circle_bottom {
                                pen_down = false;
                            } else if pen_down {
                                stroke.line_to(point(x, y));
                            } else {
                                stroke.move_to(point(x, y));
                                pen_down = true;
                            }
                        }
                        if let Ok(path) = stroke.build() {
                            let color = (0x0f6fa0u32 << 8) | alpha;
                            window.paint_path(path, rgba(color));
                        }
                    }
                },
            )
            .size_full(),
        )
}

fn trend_chart(series: &[SeriesPoint]) -> impl IntoElement {
    let values = series.iter().map(|point| point.count).collect::<Vec<_>>();
    let first = series
        .first()
        .map(|point| point.label.clone())
        .unwrap_or_default();
    let last = series
        .last()
        .map(|point| point.label.clone())
        .unwrap_or_default();
    let empty = values.iter().all(|value| *value == 0);
    div()
        .mt_3()
        .child(
            canvas(
                move |_, _, _| values,
                move |bounds, values, window, _| {
                    if values.is_empty() {
                        return;
                    }
                    let max = values.iter().copied().max().unwrap_or(1).max(1) as f32;
                    let left = bounds.origin.x + px(12.0);
                    let top = bounds.origin.y + px(12.0);
                    let width = bounds.size.width - px(24.0);
                    let height = bounds.size.height - px(24.0);
                    let step = if values.len() > 1 {
                        width / (values.len() - 1) as f32
                    } else {
                        width
                    };
                    let points = values
                        .iter()
                        .enumerate()
                        .map(|(index, value)| {
                            point(
                                left + step * index as f32,
                                top + height * (1.0 - *value as f32 / max),
                            )
                        })
                        .collect::<Vec<_>>();
                    let baseline = top + height;
                    let mut fill = gpui::PathBuilder::fill();
                    fill.move_to(point(left, baseline));
                    for point in &points {
                        fill.line_to(*point);
                    }
                    fill.line_to(point(left + width, baseline));
                    fill.line_to(point(left, baseline));
                    if let Ok(path) = fill.build() {
                        window.paint_path(path, rgba(0x496b8d24));
                    }
                    let mut stroke = gpui::PathBuilder::stroke(px(2.0));
                    if let Some(first) = points.first() {
                        stroke.move_to(*first);
                        for point in points.iter().skip(1) {
                            stroke.line_to(*point);
                        }
                        if let Ok(path) = stroke.build() {
                            window.paint_path(path, rgb(ACCENT));
                        }
                    }
                },
            )
            .h(px(150.0))
            .w_full(),
        )
        .child(
            div()
                .flex()
                .justify_between()
                .text_sm()
                .text_color(rgb(MUTED))
                .child(first)
                .child(if empty {
                    "暂无数据".to_string()
                } else {
                    "本机时间".to_string()
                })
                .child(last),
        )
}

fn input_field(label: &'static str, input: Entity<TextInput>) -> impl IntoElement {
    div()
        .w_full()
        .child(div().mb_2().text_sm().text_color(rgb(MUTED)).child(label))
        .child(
            div()
                .w_full()
                .h(px(40.0))
                .px_3()
                .flex()
                .items_center()
                .rounded_md()
                .bg(rgb(BG))
                .border_1()
                .border_color(rgb(BORDER))
                .child(input),
        )
}

fn choice_button(
    id: &'static str,
    label: &'static str,
    value: impl Into<gpui::SharedString>,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let value = value.into();
    div()
        .id(id)
        .cursor_pointer()
        .p_3()
        .rounded_md()
        .bg(rgb(PANEL))
        .border_1()
        .border_color(rgb(BORDER))
        .hover(|style| style.bg(rgb(PANEL_ALT)))
        .child(div().text_sm().text_color(rgb(MUTED)).child(label))
        .child(div().mt_1().child(value))
        .on_click(on_click)
}

fn segmented_row(label: &'static str, options: Vec<gpui::AnyElement>) -> impl IntoElement {
    div()
        .w_full()
        .flex()
        .items_center()
        .gap_3()
        .child(
            div()
                .w(px(72.0))
                .flex_none()
                .text_sm()
                .text_color(rgb(MUTED))
                .child(label),
        )
        .child(div().min_w_0().flex_1().flex().gap_2().children(options))
}

fn switch_row(
    label: &'static str,
    value_label: &'static str,
    active: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .w_full()
        .flex()
        .items_center()
        .gap_3()
        .child(
            div()
                .w(px(72.0))
                .flex_none()
                .text_sm()
                .text_color(rgb(MUTED))
                .child(label),
        )
        .child(
            div()
                .id("rule-enabled-switch")
                .flex_1()
                .h(px(44.0))
                .px_3()
                .rounded_md()
                .border_1()
                .border_color(rgb(if active { ACCENT } else { BORDER }))
                .bg(rgb(PANEL))
                .cursor_pointer()
                .flex()
                .items_center()
                .justify_between()
                .hover(|style| style.bg(rgb(PANEL_ALT)))
                .child(
                    div()
                        .text_sm()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .child(value_label),
                )
                .child(
                    div()
                        .w(px(44.0))
                        .h(px(24.0))
                        .rounded_full()
                        .bg(rgb(if active { ACCENT } else { 0xb7bcc2 }))
                        .flex()
                        .items_center()
                        .child(
                            div()
                                .ml(px(if active { 22.0 } else { 2.0 }))
                                .size(px(20.0))
                                .rounded_full()
                                .bg(rgb(0xffffff)),
                        ),
                )
                .on_click(on_click),
        )
}

fn segmented_option(
    id: &'static str,
    label: &'static str,
    selected: bool,
    tooltip: Option<&'static str>,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .min_w_0()
        .flex_1()
        .cursor_pointer()
        .h(px(40.0))
        .px_2()
        .rounded_md()
        .border_1()
        .border_color(rgb(if selected { ACCENT } else { BORDER }))
        .bg(rgb(if selected { PANEL_ALT } else { PANEL }))
        .flex()
        .items_center()
        .justify_center()
        .text_center()
        .text_sm()
        .when(selected, |this| {
            this.font_weight(gpui::FontWeight::SEMIBOLD)
        })
        .when_some(tooltip, |this, tooltip| {
            this.tooltip(move |_, cx| {
                cx.new(|_| TooltipView {
                    text: tooltip.into(),
                })
                .into()
            })
        })
        .hover(|style| style.bg(rgb(PANEL_ALT)))
        .child(label)
        .on_click(on_click)
}

fn primary_button(
    id: &'static str,
    label: impl Into<gpui::SharedString>,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .cursor_pointer()
        .px_4()
        .py_2()
        .rounded_md()
        .bg(rgb(ACCENT))
        .text_color(rgb(0xffffff))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .child(label.into())
        .on_click(on_click)
}

fn danger_button(
    id: &'static str,
    label: &'static str,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .cursor_pointer()
        .px_4()
        .py_2()
        .rounded_md()
        .bg(rgb(DANGER))
        .text_color(rgb(0xffffff))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .child(label)
        .on_click(on_click)
}

fn network_mode_label(mode: NetworkMode) -> &'static str {
    match mode {
        NetworkMode::Auto => "自动：系统代理 / TUN / VPN",
        NetworkMode::Direct => "直接：当前系统路由",
        NetworkMode::Http => "指定 HTTP 上游",
        NetworkMode::Socks5 => "指定 SOCKS5 上游",
    }
}

fn section_title(title: &'static str, subtitle: &'static str) -> impl IntoElement {
    div()
        .mt_2()
        .child(
            div()
                .text_lg()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .child(title),
        )
        .child(
            div()
                .mt_1()
                .text_sm()
                .text_color(rgb(MUTED))
                .child(subtitle),
        )
}

fn metric(
    label: &'static str,
    value: String,
    note: impl Into<gpui::SharedString>,
) -> impl IntoElement {
    let note = note.into();
    div()
        .p_5()
        .rounded_xl()
        .bg(rgb(PANEL))
        .border_1()
        .border_color(rgb(BORDER))
        .child(div().text_sm().text_color(rgb(MUTED)).child(label))
        .child(
            div()
                .mt_2()
                .text_2xl()
                .font_weight(gpui::FontWeight::BOLD)
                .child(value),
        )
        .child(div().mt_1().text_sm().text_color(rgb(MUTED)).child(note))
}

fn setting_card(
    title: &'static str,
    status: &'static str,
    description: &'static str,
    id: &'static str,
    label: &'static str,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .p_5()
        .rounded_xl()
        .bg(rgb(PANEL))
        .border_1()
        .border_color(rgb(BORDER))
        .child(
            div()
                .flex()
                .justify_between()
                .child(div().font_weight(gpui::FontWeight::SEMIBOLD).child(title))
                .child(div().text_sm().text_color(rgb(ACCENT)).child(status)),
        )
        .child(
            div()
                .mt_2()
                .text_sm()
                .text_color(rgb(MUTED))
                .child(description),
        )
        .child(div().mt_4().child(small_button(id, label, on_click)))
}

fn action_card<FirstClick, SecondClick, ThirdClick>(
    title: &'static str,
    kind: &'static str,
    description: &'static str,
    resource: String,
    first: (&'static str, &'static str, FirstClick),
    second: (&'static str, &'static str, SecondClick),
    third: (&'static str, &'static str, ThirdClick),
) -> impl IntoElement
where
    FirstClick: Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    SecondClick: Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ThirdClick: Fn(&ClickEvent, &mut Window, &mut App) + 'static,
{
    let (first_id, first_label, first_click) = first;
    let (second_id, second_label, second_click) = second;
    let (third_id, third_label, third_click) = third;
    div()
        .p_5()
        .rounded_xl()
        .bg(rgb(PANEL))
        .border_1()
        .border_color(rgb(BORDER))
        .child(
            div()
                .flex()
                .justify_between()
                .child(
                    div()
                        .text_lg()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .child(title),
                )
                .child(
                    div()
                        .px_2()
                        .py_1()
                        .rounded_md()
                        .bg(rgb(PANEL_ALT))
                        .text_sm()
                        .text_color(rgb(ACCENT))
                        .child(kind),
                ),
        )
        .child(
            div()
                .mt_3()
                .text_sm()
                .text_color(rgb(MUTED))
                .child(description),
        )
        .child(
            div()
                .mt_4()
                .px_3()
                .py_2()
                .rounded_md()
                .border_1()
                .border_color(rgb(BORDER))
                .bg(rgb(BG))
                .flex()
                .items_center()
                .justify_between()
                .gap_2()
                .child(div().text_sm().text_color(rgb(MUTED)).child("当前资源"))
                .child(
                    div()
                        .min_w_0()
                        .text_sm()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(rgb(ACCENT))
                        .child(resource),
                ),
        )
        .child(
            div()
                .mt_4()
                .flex()
                .flex_wrap()
                .gap_3()
                .child(small_button(first_id, first_label, first_click))
                .child(small_button(second_id, second_label, second_click))
                .child(small_button(third_id, third_label, third_click)),
        )
}

fn action_resource_label(config: &AppConfig, action_id: &str) -> String {
    let Some(action) = config.action(action_id) else {
        return "未配置".into();
    };
    let source = action
        .params
        .get("source")
        .map(String::as_str)
        .unwrap_or_default();
    if source.is_empty() || source.starts_with("builtin:") {
        "● 内置默认".into()
    } else {
        let path = PathBuf::from(source);
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(source);
        format!("● {name}")
    }
}

fn action_toast(
    toast: ActionToast,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let headline = if toast.group.hit_count > 1 {
        format!("Action 响应 · {} 次", toast.group.hit_count)
    } else {
        "Action 响应".into()
    };
    let status = toast
        .group
        .action_results
        .first()
        .map(|result| {
            format!(
                "{} · {}",
                action_status_label(result.status),
                action_surface_label(result.surface)
            )
        })
        .unwrap_or_else(|| "已记录".into());
    let image = toast.image.map(|image| match image.source {
        ActionImageSource::BuiltinBlocked => div()
            .w_full()
            .h(px(110.0))
            .rounded_lg()
            .bg(rgb(0x172039))
            .flex()
            .items_center()
            .justify_center()
            .gap_4()
            .child(
                div()
                    .size(px(58.0))
                    .rounded_full()
                    .border_2()
                    .border_color(rgb(0xd65c6d))
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_lg()
                    .font_weight(gpui::FontWeight::BOLD)
                    .text_color(rgb(0xf5f7ff))
                    .child("N/S"),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(rgb(0xf5f7ff))
                            .child("请求已拦截"),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(0xafb9d8))
                            .child("NET SENTINEL"),
                    ),
            )
            .into_any_element(),
        ActionImageSource::File(path) => img(path)
            .w_full()
            .h(px(110.0))
            .object_fit(gpui::ObjectFit::Contain)
            .into_any_element(),
    });
    div()
        .id("action-toast")
        .absolute()
        .top(px(54.0))
        .right(px(16.0))
        .w(px(360.0))
        .p_4()
        .rounded_xl()
        .bg(rgb(PANEL))
        .border_1()
        .border_color(rgb(BORDER))
        .shadow_lg()
        .cursor_pointer()
        .hover(|style| style.border_color(rgb(ACCENT)))
        .when_some(image, |card, image| card.child(image))
        .child(
            div()
                .mt_2()
                .flex()
                .items_center()
                .justify_between()
                .child(
                    div()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .child(headline),
                )
                .child(div().text_sm().text_color(rgb(ACCENT)).child(status)),
        )
        .child(
            div()
                .mt_2()
                .text_sm()
                .text_color(rgb(MUTED))
                .child(format!("规则 · {}", toast.group.rule_id)),
        )
        .child(
            div()
                .mt_1()
                .text_sm()
                .child(if toast.group.detail_redacted {
                    "详细日志已关闭".into()
                } else {
                    toast.group.host
                }),
        )
        .on_click(on_click)
}

fn action_notice_key(
    rule_id: &str,
    request: &str,
    protocol: InterceptionProtocol,
    results: &[ActionExecutionSummary],
) -> String {
    let host = Url::parse(request)
        .ok()
        .and_then(|url| url.host_str().map(str::to_string))
        .unwrap_or_else(|| "unknown".into());
    format!(
        "{host}\u{1f}{rule_id}\u{1f}{protocol:?}\u{1f}{}",
        serde_json::to_string(results).unwrap_or_default()
    )
}

fn action_status_label(status: ActionExecutionStatus) -> &'static str {
    match status {
        ActionExecutionStatus::Succeeded => "成功",
        ActionExecutionStatus::Failed => "失败",
        ActionExecutionStatus::Unsupported => "不支持",
        ActionExecutionStatus::Skipped => "已跳过",
    }
}

fn action_surface_label(surface: ActionSurface) -> &'static str {
    match surface {
        ActionSurface::BrowserPage => "浏览器页面",
        ActionSurface::InAppCard => "应用内卡片",
        ActionSurface::LocalAudio => "本机音频",
        ActionSurface::ConnectionBlock => "连接阻断",
    }
}

fn small_button(
    id: &'static str,
    label: &'static str,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .cursor_pointer()
        .px_4()
        .py_2()
        .rounded_lg()
        .bg(rgb(PANEL_ALT))
        .border_1()
        .border_color(rgb(BORDER))
        .hover(|style| style.bg(rgb(BORDER)))
        .text_sm()
        .child(label)
        .on_click(on_click)
}

fn rule_row(rule: &config::Rule) -> impl IntoElement {
    let target = match rule.target {
        MatchTarget::Host => "HOST",
        MatchTarget::Url => "URL",
        MatchTarget::Path => "PATH",
        MatchTarget::Header => "HEADER",
    };
    let mode = match rule.mode {
        MatchMode::Exact => "exact",
        MatchMode::Contains => "contains",
        MatchMode::Glob => "glob",
        MatchMode::Regex => "regex",
    };
    div()
        .p_4()
        .rounded_xl()
        .bg(rgb(PANEL))
        .border_1()
        .border_color(rgb(BORDER))
        .flex()
        .items_center()
        .gap_4()
        .child(
            div()
                .size_2()
                .rounded_full()
                .bg(rgb(if rule.enabled { SUCCESS } else { MUTED })),
        )
        .child(
            div().w(px(190.0)).child(rule.id.clone()).child(
                div()
                    .text_sm()
                    .text_color(rgb(MUTED))
                    .child(if rule.enabled {
                        "已启用"
                    } else {
                        "已停用"
                    }),
            ),
        )
        .child(
            div()
                .w(px(120.0))
                .text_sm()
                .text_color(rgb(ACCENT))
                .child(format!("{target} · {mode}")),
        )
        .child(div().flex_1().text_sm().child(rule.pattern.clone()))
        .child(
            div()
                .text_sm()
                .text_color(rgb(MUTED))
                .child(format!("{} actions", rule.actions.len())),
        )
}

fn build_tray(ui_tx: Sender<UiEvent>) -> Result<TrayIcon> {
    let menu = Menu::new();
    let show = MenuItem::new("打开 Net Sentinel", true, None);
    let toggle = MenuItem::new("启动 / 停止监听", true, None);
    let quit = MenuItem::new("退出并恢复系统代理", true, None);
    menu.append(&show)?;
    menu.append(&toggle)?;
    menu.append(&PredefinedMenuItem::separator())?;
    menu.append(&quit)?;

    let show_id = show.id().clone();
    let toggle_id = toggle.id().clone();
    let quit_id = quit.id().clone();
    let menu_tx = ui_tx.clone();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        let command = if event.id == show_id {
            Some(UiEvent::TrayShow)
        } else if event.id == toggle_id {
            Some(UiEvent::TrayToggleProxy)
        } else if event.id == quit_id {
            Some(UiEvent::TrayQuit)
        } else {
            None
        };
        if let Some(command) = command {
            let _ = menu_tx.try_send(command);
        }
    }));

    TrayIconEvent::set_event_handler(Some(move |event| {
        if matches!(
            event,
            TrayIconEvent::DoubleClick {
                button: MouseButton::Left,
                ..
            } | TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            }
        ) {
            let _ = ui_tx.try_send(UiEvent::TrayShow);
        }
    }));

    TrayIconBuilder::new()
        .with_tooltip("Net Sentinel · HTTP 黑名单拦截器")
        .with_menu(Box::new(menu))
        .with_icon(tray_icon_image(false)?)
        .build()
        .context("无法创建系统托盘图标")
}

fn tray_icon_image(unread: bool) -> Result<Icon> {
    let size = 32u32;
    let mut rgba = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let dx = x as f32 - 15.5;
            let dy = y as f32 - 15.5;
            let distance = (dx * dx + dy * dy).sqrt();
            let badge_dx = x as f32 - 25.0;
            let badge_dy = y as f32 - 7.0;
            let (r, g, b, a) = if unread && badge_dx * badge_dx + badge_dy * badge_dy <= 30.25 {
                (205, 57, 57, 255)
            } else if distance < 14.0 {
                if (x as i32 - y as i32).abs() < 3 {
                    (255, 104, 126, 255)
                } else {
                    (74, 85, 150, 255)
                }
            } else {
                (0, 0, 0, 0)
            };
            rgba.extend_from_slice(&[r, g, b, a]);
        }
    }
    Icon::from_rgba(rgba, size, size).context("无法生成托盘图标")
}

fn main() -> Result<()> {
    let path = config_path();
    let loaded = AppConfig::load_or_create(&path)?;
    let startup_network_notice =
        match system::recover_stale_takeover(&loaded.proxy.listen.to_string()) {
            Ok(true) => Some(UiEvent::NetworkProbe(
                "检测到上次异常退出，已在启动界面前恢复 Clash 系统代理".into(),
            )),
            Ok(false) => None,
            Err(error) => Some(UiEvent::Error(format!(
                "恢复上次代理快照失败，应用不会自动启动保护：{error:#}"
            ))),
        };
    let analytics = Arc::new(Analytics::new(app_data_dir().join("analytics.sqlite3"))?);
    analytics.maintain(
        loaded.analytics.detailed_retention_days,
        loaded.analytics.aggregate_retention_days,
    )?;
    let start_minimized =
        std::env::args().any(|arg| arg == "--minimized") || loaded.startup.start_minimized;
    let shared: SharedConfig = Arc::new(RwLock::new(loaded));
    let (ui_tx, ui_rx) = async_channel::unbounded();
    if let Some(notice) = startup_network_notice {
        let _ = ui_tx.try_send(notice);
    }
    let actions = Arc::new(ActionRegistry::standard(ui_tx.clone()));

    Application::new()
        .with_assets(assets::EmbeddedAssets)
        .run(move |cx: &mut App| {
            text_input::bind_keys(cx);
            match build_tray(ui_tx.clone()) {
                Ok(icon) => cx.set_global(TrayGlobal { icon }),
                Err(error) => {
                    let _ = ui_tx.try_send(UiEvent::Error(format!("系统托盘不可用：{error:#}")));
                }
            }

            let bounds = Bounds::centered(None, size(px(1120.0), px(780.0)), cx);
            let shared_for_window = shared.clone();
            let path_for_window = path.clone();
            let actions_for_window = actions.clone();
            let analytics_for_window = analytics.clone();
            let tx_for_window = ui_tx.clone();
            let rx_for_window = ui_rx.clone();
            let _handle = cx
                .open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(bounds)),
                        window_min_size: Some(size(px(1040.0), px(680.0))),
                        titlebar: Some(TitlebarOptions {
                            title: Some("Net Sentinel".into()),
                            appears_transparent: true,
                            traffic_light_position: None,
                        }),
                        ..Default::default()
                    },
                    move |window, cx| {
                        let proxy = ProxyService::new(
                            shared_for_window.clone(),
                            actions_for_window.clone(),
                            Some(analytics_for_window.clone()),
                            tx_for_window.clone(),
                        );
                        let view = cx.new(|cx| {
                            AppView::new(
                                shared_for_window,
                                path_for_window,
                                actions_for_window,
                                analytics_for_window,
                                proxy,
                                tx_for_window,
                                rx_for_window,
                                start_minimized,
                                cx,
                            )
                        });
                        let close = window.handler_for(&view, |view, _window, cx| {
                            view.cleanup_system_proxy();
                            cx.quit();
                        });
                        window.on_window_should_close(cx, move |window, cx| {
                            close(window, cx);
                            false
                        });
                        view
                    },
                )
                .expect("failed to open main window");

            if start_minimized {
                cx.hide();
            } else {
                cx.activate(true);
            }
        });
    Ok(())
}
