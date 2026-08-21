use std::{
    collections::HashMap,
    io::{Read, Write},
    net::{Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs},
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context as _, Result, anyhow};
use async_channel::Sender;
use url::Url;

use crate::{
    actions::{ActionContext, ActionRegistry},
    analytics::Analytics,
    config::{AppConfig, NetworkMode},
    events::{
        ActionExecutionStatus, ActionExecutionSummary, ActionSurface, InterceptionProtocol, UiEvent,
    },
    rules::{RequestFacts, matches},
};

const MAX_HEADER_BYTES: usize = 64 * 1024;

pub type SharedConfig = Arc<RwLock<AppConfig>>;

pub struct ProxyService {
    config: SharedConfig,
    actions: Arc<ActionRegistry>,
    analytics: Option<Arc<Analytics>>,
    ui_tx: Sender<UiEvent>,
    stop: Arc<AtomicBool>,
    running: Arc<AtomicBool>,
    active_clients: Arc<Mutex<HashMap<u64, TcpStream>>>,
    next_client_id: Arc<AtomicU64>,
}

impl ProxyService {
    pub fn new(
        config: SharedConfig,
        actions: Arc<ActionRegistry>,
        analytics: Option<Arc<Analytics>>,
        ui_tx: Sender<UiEvent>,
    ) -> Self {
        Self {
            config,
            actions,
            analytics,
            ui_tx,
            stop: Arc::new(AtomicBool::new(true)),
            running: Arc::new(AtomicBool::new(false)),
            active_clients: Arc::new(Mutex::new(HashMap::new())),
            next_client_id: Arc::new(AtomicU64::new(1)),
        }
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }

    pub fn start(&mut self) -> Result<()> {
        if self.is_running() {
            return Ok(());
        }
        let listen = self.config.read().expect("config lock").proxy.listen;
        let listener = TcpListener::bind(listen)
            .with_context(|| format!("无法监听 {listen}，端口可能已被占用"))?;
        listener.set_nonblocking(true)?;
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_thread = stop.clone();
        let running = self.running.clone();
        let config = self.config.clone();
        let actions = self.actions.clone();
        let analytics = self.analytics.clone();
        let ui_tx = self.ui_tx.clone();
        let active_clients = self.active_clients.clone();
        let next_client_id = self.next_client_id.clone();

        running.store(true, Ordering::Release);
        let spawn_result = thread::Builder::new()
            .name("net-sentinel-proxy".into())
            .spawn(move || {
                let result = run_listener(
                    listener,
                    listen,
                    stop_for_thread,
                    running.clone(),
                    config,
                    actions,
                    analytics,
                    ui_tx.clone(),
                    active_clients,
                    next_client_id,
                );
                running.store(false, Ordering::Release);
                if let Err(error) = result {
                    let _ = ui_tx.send_blocking(UiEvent::ProxyStatus {
                        running: false,
                        detail: format!("启动失败：{error:#}"),
                    });
                }
            });
        match spawn_result {
            Ok(_) => {
                self.stop = stop;
                Ok(())
            }
            Err(error) => {
                self.running.store(false, Ordering::Release);
                Err(error).context("无法启动代理线程")
            }
        }
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
        self.disconnect_clients();
    }

    /// Disconnects browser connections that were matched against an older
    /// ruleset. Browsers reconnect automatically and the next request uses the
    /// latest shared configuration.
    pub fn disconnect_clients(&self) -> usize {
        let clients = {
            let mut active = self.active_clients.lock().expect("active clients lock");
            active.drain().map(|(_, stream)| stream).collect::<Vec<_>>()
        };
        let count = clients.len();
        for stream in clients {
            let _ = stream.shutdown(Shutdown::Both);
        }
        count
    }
}

impl Drop for ProxyService {
    fn drop(&mut self) {
        self.stop();
    }
}

#[allow(clippy::too_many_arguments)]
fn run_listener(
    listener: TcpListener,
    listen: SocketAddr,
    stop: Arc<AtomicBool>,
    running: Arc<AtomicBool>,
    config: SharedConfig,
    actions: Arc<ActionRegistry>,
    analytics: Option<Arc<Analytics>>,
    ui_tx: Sender<UiEvent>,
    active_clients: Arc<Mutex<HashMap<u64, TcpStream>>>,
    next_client_id: Arc<AtomicU64>,
) -> Result<()> {
    let _ = ui_tx.send_blocking(UiEvent::ProxyStatus {
        running: true,
        detail: format!("正在监听 {listen}"),
    });

    while !stop.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((client, peer)) => {
                let client_id = next_client_id.fetch_add(1, Ordering::Relaxed);
                match client.try_clone() {
                    Ok(tracked) => {
                        active_clients
                            .lock()
                            .expect("active clients lock")
                            .insert(client_id, tracked);
                    }
                    Err(error) => {
                        let _ = ui_tx.send_blocking(UiEvent::Error(format!(
                            "无法跟踪代理连接 {peer}：{error}"
                        )));
                        continue;
                    }
                }
                let config = config.clone();
                let actions = actions.clone();
                let analytics = analytics.clone();
                let connection_tx = ui_tx.clone();
                let active_for_connection = active_clients.clone();
                let spawn = thread::Builder::new()
                    .name("net-sentinel-connection".into())
                    .spawn(move || {
                        if let Err(error) = handle_client(
                            client,
                            peer,
                            config,
                            actions,
                            analytics,
                            connection_tx.clone(),
                        ) {
                            let _ = connection_tx.send_blocking(UiEvent::Error(format!(
                                "代理连接 {peer} 出错：{error:#}"
                            )));
                        }
                        active_for_connection
                            .lock()
                            .expect("active clients lock")
                            .remove(&client_id);
                    });
                if let Err(error) = spawn {
                    active_clients
                        .lock()
                        .expect("active clients lock")
                        .remove(&client_id);
                    let _ = ui_tx.send_blocking(UiEvent::Error(format!(
                        "无法启动代理连接线程 {peer}：{error}"
                    )));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(30));
            }
            Err(error) => return Err(error.into()),
        }
    }
    running.store(false, Ordering::Release);
    let _ = ui_tx.send_blocking(UiEvent::ProxyStatus {
        running: false,
        detail: "代理已停止".into(),
    });
    Ok(())
}

fn handle_client(
    mut client: TcpStream,
    _peer: SocketAddr,
    config: SharedConfig,
    actions: Arc<ActionRegistry>,
    analytics: Option<Arc<Analytics>>,
    ui_tx: Sender<UiEvent>,
) -> Result<()> {
    // On Windows, a socket accepted from our non-blocking listener can retain
    // non-blocking mode. CONNECT tunnels then fail as soon as the TLS client
    // hello is not immediately available (WSAEWOULDBLOCK / 10035).
    client.set_nonblocking(false)?;
    client.set_read_timeout(Some(Duration::from_secs(15)))?;
    client.set_write_timeout(Some(Duration::from_secs(15)))?;
    let request_bytes = read_headers(&mut client)?;
    if request_bytes.is_empty() {
        return Ok(());
    }
    let header_end = find_header_end(&request_bytes).ok_or_else(|| anyhow!("HTTP 请求头不完整"))?;
    let header_text =
        std::str::from_utf8(&request_bytes[..header_end]).context("HTTP 请求头不是 UTF-8/ASCII")?;
    let parsed = ParsedRequest::parse(header_text)?;

    if parsed.is_internal_probe() {
        return forward_http(
            client,
            parsed,
            request_bytes,
            header_end,
            &OutboundRoute::Direct,
        );
    }

    let facts = parsed.facts();
    let matched = {
        let guard = config.read().expect("config lock");
        guard
            .blacklist
            .iter()
            .find(|rule| matches(rule, &facts).unwrap_or(false))
            .cloned()
    };

    if let Some(rule) = matched {
        let is_https = parsed.method.eq_ignore_ascii_case("CONNECT");
        let protocol = if is_https {
            InterceptionProtocol::Https
        } else {
            InterceptionProtocol::Http
        };
        let definitions = {
            let guard = config.read().expect("config lock");
            rule.actions
                .iter()
                .filter_map(|id| guard.action(id).cloned())
                .collect::<Vec<_>>()
        };
        let context = ActionContext {
            rule_id: rule.id.clone(),
            request: facts.url.clone(),
        };
        let mut html_response = None;
        let mut image = None;
        let mut action_results = Vec::with_capacity(definitions.len() + 1);
        for definition in &definitions {
            let surface = actions
                .surface(&definition.kind)
                .unwrap_or(ActionSurface::InAppCard);
            if !definition.enabled {
                action_results.push(action_summary(
                    definition,
                    ActionExecutionStatus::Skipped,
                    surface,
                    Some("Action 已停用".into()),
                ));
                continue;
            }
            if is_https && definition.kind == "serve_html" {
                action_results.push(action_summary(
                    definition,
                    ActionExecutionStatus::Unsupported,
                    surface,
                    Some("HTTPS 未解密，不能读取或注入 HTML".into()),
                ));
                continue;
            }
            if definition.kind == "serve_html" && html_response.is_some() {
                action_results.push(action_summary(
                    definition,
                    ActionExecutionStatus::Skipped,
                    surface,
                    Some("已有 HTML Action 成功生成页面".into()),
                ));
                continue;
            }
            match actions.execute(&context, definition) {
                Ok(output) => {
                    if let Some(response) = output.html_response {
                        html_response = Some(response);
                    }
                    if image.is_none() {
                        image = output.image;
                    }
                    action_results.push(action_summary(
                        definition,
                        ActionExecutionStatus::Succeeded,
                        surface,
                        None,
                    ));
                }
                Err(error) => {
                    let error_summary = format!("{error:#}");
                    action_results.push(action_summary(
                        definition,
                        ActionExecutionStatus::Failed,
                        surface,
                        Some(error_summary.clone()),
                    ));
                    let _ = ui_tx.send_blocking(UiEvent::Error(format!(
                        "Action {} 执行失败：{error:#}",
                        definition.id
                    )));
                }
            }
        }
        action_results.push(ActionExecutionSummary {
            action_id: "connection-block".into(),
            kind: "connection_block".into(),
            status: ActionExecutionStatus::Succeeded,
            surface: ActionSurface::ConnectionBlock,
            error: None,
        });
        if let Some(analytics) = analytics {
            let detailed_logging = config
                .read()
                .expect("config lock")
                .analytics
                .detailed_logging;
            if let Err(error) = analytics.record(
                &rule.id,
                &facts.url,
                protocol,
                &action_results,
                detailed_logging,
            ) {
                let _ = ui_tx.send_blocking(UiEvent::Error(format!("记录拦截统计失败：{error:#}")));
            }
        }
        let _ = ui_tx.send_blocking(UiEvent::Blocked {
            rule_id: rule.id.clone(),
            request: facts.url.clone(),
            protocol,
            action_results: action_results.clone(),
            image,
        });
        if is_https {
            write_connect_blocked(&mut client)?;
        } else if let Some(response) = html_response {
            write_html(&mut client, &response.body)?;
        } else {
            write_blocked(&mut client, &facts.url, &rule.id, &action_results)?;
        }
        return Ok(());
    }

    if parsed.method.eq_ignore_ascii_case("CONNECT") {
        let route = outbound_route(&config, &parsed.authority)?;
        tunnel_connect(client, &parsed.authority, &route)
    } else {
        let route = outbound_route(&config, &parsed.authority)?;
        forward_http(client, parsed, request_bytes, header_end, &route)
    }
}

#[derive(Debug, Clone)]
enum OutboundRoute {
    Direct,
    Http(String),
    Socks5(String),
}

pub fn probe_upstream(
    mode: NetworkMode,
    upstream: Option<&str>,
    listen: SocketAddr,
) -> Result<String> {
    let (route, label) = match mode {
        NetworkMode::Direct => (OutboundRoute::Direct, "直接系统路由".to_string()),
        NetworkMode::Auto => match upstream.filter(|value| !value.trim().is_empty()) {
            Some(value) => (parse_upstream(value, false)?, format!("自动上游 {value}")),
            None => (
                OutboundRoute::Direct,
                "自动路由（TUN / VPN / 系统网络）".to_string(),
            ),
        },
        NetworkMode::Http => {
            let value = upstream
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| anyhow!("HTTP 上游代理尚未配置"))?;
            (parse_upstream(value, false)?, format!("HTTP 上游 {value}"))
        }
        NetworkMode::Socks5 => {
            let value = upstream
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| anyhow!("SOCKS5 上游代理尚未配置"))?;
            (parse_upstream(value, true)?, format!("SOCKS5 上游 {value}"))
        }
    };
    let loops = match &route {
        OutboundRoute::Direct => false,
        OutboundRoute::Http(address) | OutboundRoute::Socks5(address) => {
            address == &listen.to_string()
        }
    };
    if loops {
        return Err(anyhow!("检测到代理回环：{listen}"));
    }
    probe_route(&route)?;
    Ok(format!("{label} 已通过端到端 HTTP 检查"))
}

pub fn probe_local_proxy(listen: SocketAddr) -> Result<String> {
    let mut last_error = None;
    for attempt in 1..=3 {
        match probe_local_once(listen) {
            Ok(()) => return Ok(format!("本机代理 {listen} 已完成转发检查")),
            Err(error) => last_error = Some(error),
        }
        if attempt < 3 {
            thread::sleep(Duration::from_millis(160));
        }
    }
    Err(last_error.expect("本机转发检查至少执行一次")).context("本机代理连续 3 次转发检查失败")
}

const PROBE_AUTHORITY: &str = "github.com:443";
const LOCAL_PROBE_HEADER: &str = "X-Net-Sentinel-Probe";
const LOCAL_PROBE_PATH: &str = "/__net-sentinel-probe/";

fn probe_local_once(listen: SocketAddr) -> Result<()> {
    let canary = TcpListener::bind("127.0.0.1:0").context("无法创建本机转发验证端点")?;
    canary.set_nonblocking(true)?;
    let canary_address = canary.local_addr()?;
    let token = format!(
        "{:x}-{:x}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    let expected_token = token.clone();
    let canary_thread = thread::spawn(move || -> Result<()> {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let (mut stream, _) = loop {
            match canary.accept() {
                Ok(connection) => break connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if std::time::Instant::now() >= deadline {
                        return Err(anyhow!("本机验证端点未收到转发请求"));
                    }
                    thread::sleep(Duration::from_millis(20));
                }
                Err(error) => return Err(error.into()),
            }
        };
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
        stream.set_write_timeout(Some(Duration::from_secs(2)))?;
        let request = String::from_utf8_lossy(&read_headers(&mut stream)?).to_string();
        anyhow::ensure!(
            request.starts_with(&format!("GET {LOCAL_PROBE_PATH}{expected_token} ")),
            "本机验证端点收到的路径不匹配"
        );
        anyhow::ensure!(
            request.lines().any(|line| {
                line.split_once(':').is_some_and(|(name, value)| {
                    name.eq_ignore_ascii_case(LOCAL_PROBE_HEADER) && value.trim() == expected_token
                })
            }),
            "本机验证端点收到的校验标记不匹配"
        );
        let response = format!(
            "HTTP/1.1 204 No Content\r\n{LOCAL_PROBE_HEADER}: {expected_token}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        stream.write_all(response.as_bytes())?;
        Ok(())
    });

    let client_result = (|| -> Result<()> {
        let mut stream = connect(&listen.to_string(), 8080)
            .with_context(|| format!("无法连接本机代理 {listen}"))?;
        stream.set_read_timeout(Some(Duration::from_secs(3)))?;
        stream.set_write_timeout(Some(Duration::from_secs(3)))?;
        let target = format!("http://{canary_address}{LOCAL_PROBE_PATH}{token}");
        let request = format!(
            "GET {target} HTTP/1.1\r\nHost: {canary_address}\r\n{LOCAL_PROBE_HEADER}: {token}\r\nConnection: close\r\n\r\n"
        );
        stream.write_all(request.as_bytes())?;
        let response = String::from_utf8_lossy(
            &read_headers(&mut stream).context("本机代理未返回有效验证响应")?,
        )
        .to_string();
        let status = response.lines().next().unwrap_or_default();
        anyhow::ensure!(
            status.split_whitespace().nth(1) == Some("204"),
            "本机代理返回异常状态：{status}"
        );
        anyhow::ensure!(
            response.lines().any(|line| {
                line.split_once(':').is_some_and(|(name, value)| {
                    name.eq_ignore_ascii_case(LOCAL_PROBE_HEADER) && value.trim() == token
                })
            }),
            "本机代理验证响应缺少正确的校验标记"
        );
        Ok(())
    })();
    let canary_result = canary_thread
        .join()
        .map_err(|_| anyhow!("本机验证端点线程异常退出"))?;
    client_result?;
    canary_result
}

fn probe_route(route: &OutboundRoute) -> Result<()> {
    match route {
        OutboundRoute::Direct => {
            let _ = connect(PROBE_AUTHORITY, 443).context("系统路由无法连接 GitHub HTTPS")?;
        }
        OutboundRoute::Http(proxy) => {
            let _ = connect_http_tunnel(proxy, PROBE_AUTHORITY)
                .context("上游代理无法建立 GitHub HTTPS 隧道")?;
        }
        OutboundRoute::Socks5(proxy) => {
            let _ = connect_socks5(proxy, PROBE_AUTHORITY, 443)
                .context("SOCKS5 上游无法连接 GitHub HTTPS")?;
        }
    }
    Ok(())
}

fn outbound_route(config: &SharedConfig, target: &str) -> Result<OutboundRoute> {
    let guard = config.read().expect("config lock");
    let upstream = guard
        .proxy
        .upstream_proxy
        .as_deref()
        .filter(|value| !value.is_empty());
    let route = match guard.proxy.network_mode {
        NetworkMode::Direct => OutboundRoute::Direct,
        NetworkMode::Auto => match upstream {
            Some(value) => parse_upstream(value, false)?,
            None => OutboundRoute::Direct,
        },
        NetworkMode::Http => parse_upstream(
            upstream.ok_or_else(|| anyhow!("HTTP 上游代理尚未配置"))?,
            false,
        )?,
        NetworkMode::Socks5 => parse_upstream(
            upstream.ok_or_else(|| anyhow!("SOCKS5 上游代理尚未配置"))?,
            true,
        )?,
    };
    let listen = guard.proxy.listen.to_string();
    let loops = match &route {
        OutboundRoute::Direct => false,
        OutboundRoute::Http(value) | OutboundRoute::Socks5(value) => value == &listen,
    };
    if loops || target == listen {
        return Err(anyhow!("检测到代理回环：{listen}"));
    }
    Ok(route)
}

fn parse_upstream(value: &str, force_socks: bool) -> Result<OutboundRoute> {
    let (kind, normalized) = value
        .split(';')
        .find_map(|entry| {
            let entry = entry.trim();
            if entry.is_empty() {
                None
            } else if let Some((kind, address)) = entry.split_once('=') {
                Some((Some(kind.trim()), address.trim()))
            } else {
                Some((None, entry))
            }
        })
        .ok_or_else(|| anyhow!("上游代理地址为空"))?;
    if force_socks
        || kind.is_some_and(|kind| kind.eq_ignore_ascii_case("socks"))
        || normalized.starts_with("socks5://")
        || normalized.starts_with("socks://")
    {
        Ok(OutboundRoute::Socks5(
            normalized
                .trim_start_matches("socks5://")
                .trim_start_matches("socks://")
                .to_string(),
        ))
    } else {
        Ok(OutboundRoute::Http(
            normalized.trim_start_matches("http://").to_string(),
        ))
    }
}

fn read_headers(stream: &mut TcpStream) -> Result<Vec<u8>> {
    let mut bytes = Vec::with_capacity(4096);
    let mut buffer = [0u8; 4096];
    while bytes.len() < MAX_HEADER_BYTES {
        let read = stream.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..read]);
        if find_header_end(&bytes).is_some() {
            return Ok(bytes);
        }
    }
    if bytes.len() >= MAX_HEADER_BYTES {
        Err(anyhow!("HTTP 请求头超过 {} KiB", MAX_HEADER_BYTES / 1024))
    } else {
        Ok(bytes)
    }
}

fn find_header_end(bytes: &[u8]) -> Option<usize> {
    bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| index + 4)
}

struct ParsedRequest {
    method: String,
    version: String,
    authority: String,
    url: String,
    path: String,
    headers: Vec<(String, String)>,
}

impl ParsedRequest {
    fn parse(header: &str) -> Result<Self> {
        let mut lines = header.split("\r\n");
        let request_line = lines.next().ok_or_else(|| anyhow!("缺少请求行"))?;
        let mut parts = request_line.split_whitespace();
        let method = parts
            .next()
            .ok_or_else(|| anyhow!("缺少 HTTP 方法"))?
            .to_string();
        let target = parts.next().ok_or_else(|| anyhow!("缺少请求目标"))?;
        let version = parts.next().unwrap_or("HTTP/1.1").to_string();
        let headers = lines
            .filter(|line| !line.is_empty())
            .filter_map(|line| line.split_once(':'))
            .map(|(name, value)| (name.trim().to_string(), value.trim().to_string()))
            .collect::<Vec<_>>();

        if method.eq_ignore_ascii_case("CONNECT") {
            let authority = target.to_string();
            return Ok(Self {
                method,
                version,
                authority: authority.clone(),
                url: format!("https://{authority}/"),
                path: "/".into(),
                headers,
            });
        }

        let url = if target.starts_with("http://") || target.starts_with("https://") {
            Url::parse(target).context("代理请求中的 URL 无效")?
        } else {
            let host = header_value(&headers, "Host").ok_or_else(|| anyhow!("缺少 Host 头"))?;
            Url::parse(&format!("http://{host}{target}"))?
        };
        let authority = match url.port() {
            Some(port) => format!("{}:{port}", url.host_str().unwrap_or_default()),
            None => url.host_str().unwrap_or_default().to_string(),
        };
        let path = match url.query() {
            Some(query) => format!("{}?{query}", url.path()),
            None => url.path().to_string(),
        };
        Ok(Self {
            method,
            version,
            authority,
            url: url.to_string(),
            path,
            headers,
        })
    }

    fn facts(&self) -> RequestFacts {
        RequestFacts {
            method: self.method.clone(),
            host: self
                .authority
                .split(':')
                .next()
                .unwrap_or_default()
                .to_lowercase(),
            url: self.url.clone(),
            path: self.path.clone(),
            headers: self.headers.clone(),
        }
    }

    fn is_internal_probe(&self) -> bool {
        if !self.authority.starts_with("127.0.0.1:") {
            return false;
        }
        let Some(token) = header_value(&self.headers, LOCAL_PROBE_HEADER) else {
            return false;
        };
        self.path
            .strip_prefix(LOCAL_PROBE_PATH)
            .is_some_and(|path_token| path_token == token)
    }
}

fn header_value<'a>(headers: &'a [(String, String)], expected: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(expected))
        .map(|(_, value)| value.as_str())
}

fn tunnel_connect(mut client: TcpStream, authority: &str, route: &OutboundRoute) -> Result<()> {
    let mut upstream = match route {
        OutboundRoute::Direct => connect(authority, 443)?,
        OutboundRoute::Http(proxy) => connect_http_tunnel(proxy, authority)?,
        OutboundRoute::Socks5(proxy) => connect_socks5(proxy, authority, 443)?,
    };
    client.write_all(
        b"HTTP/1.1 200 Connection Established\r\nProxy-Agent: NetSentinel/0.1\r\n\r\n",
    )?;
    client.set_read_timeout(None)?;
    client.set_write_timeout(None)?;
    upstream.set_read_timeout(None)?;
    upstream.set_write_timeout(None)?;
    let mut client_reader = client.try_clone()?;
    let mut upstream_writer = upstream.try_clone()?;
    let upload = thread::spawn(move || {
        let result = std::io::copy(&mut client_reader, &mut upstream_writer);
        let _ = upstream_writer.shutdown(Shutdown::Write);
        result
    });
    let download_result = std::io::copy(&mut upstream, &mut client);
    let _ = client.shutdown(Shutdown::Write);
    let uploaded = upload
        .join()
        .map_err(|_| anyhow!("HTTPS 隧道上传线程异常退出"))?
        .context("HTTPS 隧道上传失败")?;
    let downloaded = download_result.context("HTTPS 隧道下载失败")?;
    anyhow::ensure!(
        uploaded == 0 || downloaded > 0,
        "HTTPS 隧道已上传 {uploaded} 字节，但上游未返回任何数据"
    );
    Ok(())
}

fn connect_http_tunnel(proxy: &str, authority: &str) -> Result<TcpStream> {
    let mut stream = connect(proxy, 8080)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    let request = format!(
        "CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\nUser-Agent: NetSentinel/0.1\r\nProxy-Connection: Keep-Alive\r\nConnection: Keep-Alive\r\n\r\n"
    );
    stream.write_all(request.as_bytes())?;
    let response = read_headers(&mut stream).context("上游 HTTP 代理未返回 CONNECT 响应")?;
    let status_line = String::from_utf8_lossy(&response)
        .lines()
        .next()
        .unwrap_or_default()
        .to_string();
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|value| value.parse::<u16>().ok())
        .ok_or_else(|| anyhow!("上游 HTTP 代理返回无效状态：{status_line}"))?;
    anyhow::ensure!(status == 200, "上游 HTTP 代理拒绝 CONNECT：{status_line}");
    stream.set_read_timeout(None)?;
    stream.set_write_timeout(None)?;
    Ok(stream)
}

fn forward_http(
    mut client: TcpStream,
    parsed: ParsedRequest,
    original: Vec<u8>,
    header_end: usize,
    route: &OutboundRoute,
) -> Result<()> {
    let mut upstream = match route {
        OutboundRoute::Direct => connect(&parsed.authority, 80)?,
        OutboundRoute::Http(proxy) => connect(proxy, 8080)?,
        OutboundRoute::Socks5(proxy) => connect_socks5(proxy, &parsed.authority, 80)?,
    };
    upstream.set_read_timeout(Some(Duration::from_secs(30)))?;
    upstream.set_write_timeout(Some(Duration::from_secs(15)))?;

    let request_target = if matches!(route, OutboundRoute::Http(_)) {
        parsed.url.as_str()
    } else {
        parsed.path.as_str()
    };
    let mut rewritten = format!(
        "{} {} {}\r\n",
        parsed.method, request_target, parsed.version
    );
    for (name, value) in &parsed.headers {
        if name.eq_ignore_ascii_case("Proxy-Connection") || name.eq_ignore_ascii_case("Connection")
        {
            continue;
        }
        rewritten.push_str(&format!("{name}: {value}\r\n"));
    }
    rewritten.push_str("Connection: close\r\n\r\n");
    upstream.write_all(rewritten.as_bytes())?;
    upstream.write_all(&original[header_end..])?;

    let content_length = header_value(&parsed.headers, "Content-Length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    let already_read = original.len().saturating_sub(header_end);
    if content_length > already_read {
        let mut remaining = vec![0u8; content_length - already_read];
        client.read_exact(&mut remaining)?;
        upstream.write_all(&remaining)?;
    }
    upstream.shutdown(Shutdown::Write)?;
    std::io::copy(&mut upstream, &mut client)?;
    Ok(())
}

fn connect_socks5(proxy: &str, authority: &str, default_port: u16) -> Result<TcpStream> {
    let mut stream = connect(proxy, 1080)?;
    stream.write_all(&[0x05, 0x01, 0x00])?;
    let mut greeting = [0u8; 2];
    stream.read_exact(&mut greeting)?;
    if greeting != [0x05, 0x00] {
        return Err(anyhow!("SOCKS5 上游不支持无认证连接"));
    }
    let (host, port) = split_authority(authority, default_port)?;
    if host.len() > u8::MAX as usize {
        return Err(anyhow!("SOCKS5 目标域名过长"));
    }
    let mut request = vec![0x05, 0x01, 0x00, 0x03, host.len() as u8];
    request.extend_from_slice(host.as_bytes());
    request.extend_from_slice(&port.to_be_bytes());
    stream.write_all(&request)?;
    let mut response = [0u8; 4];
    stream.read_exact(&mut response)?;
    if response[1] != 0x00 {
        return Err(anyhow!("SOCKS5 上游连接失败，状态码 {}", response[1]));
    }
    let trailing = match response[3] {
        0x01 => 4 + 2,
        0x04 => 16 + 2,
        0x03 => {
            let mut length = [0u8; 1];
            stream.read_exact(&mut length)?;
            length[0] as usize + 2
        }
        _ => return Err(anyhow!("SOCKS5 上游返回未知地址类型")),
    };
    let mut discard = vec![0u8; trailing];
    stream.read_exact(&mut discard)?;
    Ok(stream)
}

fn split_authority(authority: &str, default_port: u16) -> Result<(String, u16)> {
    if let Some((host, port)) = authority.rsplit_once(':')
        && let Ok(port) = port.parse::<u16>()
    {
        return Ok((host.to_string(), port));
    }
    Ok((authority.to_string(), default_port))
}

fn connect(authority: &str, default_port: u16) -> Result<TcpStream> {
    let address = if authority
        .rsplit_once(':')
        .is_some_and(|(_, port)| port.parse::<u16>().is_ok())
    {
        authority.to_string()
    } else {
        format!("{authority}:{default_port}")
    };
    let socket = address
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| anyhow!("无法解析上游地址 {address}"))?;
    TcpStream::connect_timeout(&socket, Duration::from_secs(10))
        .with_context(|| format!("无法连接上游 {address}"))
}

fn action_summary(
    definition: &crate::config::ActionDefinition,
    status: ActionExecutionStatus,
    surface: ActionSurface,
    error: Option<String>,
) -> ActionExecutionSummary {
    ActionExecutionSummary {
        action_id: definition.id.clone(),
        kind: definition.kind.clone(),
        status,
        surface,
        error: error.map(|value| value.chars().take(240).collect()),
    }
}

fn write_connect_blocked(stream: &mut TcpStream) -> Result<()> {
    stream
        .write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")?;
    Ok(())
}

fn write_blocked(
    stream: &mut TcpStream,
    request: &str,
    rule_id: &str,
    action_results: &[ActionExecutionSummary],
) -> Result<()> {
    let domain = Url::parse(request)
        .ok()
        .and_then(|url| url.host_str().map(str::to_string))
        .unwrap_or_else(|| "未知域名".into());
    let badges = action_results
        .iter()
        .map(|result| format!("<span>{}</span>", html_escape(&result.kind)))
        .collect::<Vec<_>>()
        .join("");
    let body = format!(
        r#"<!doctype html><html lang="zh-CN"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width"><title>请求已拦截</title><style>body{{font-family:system-ui;background:#f6f7f9;color:#20242a;display:grid;place-items:center;min-height:100vh;margin:0}}.card{{width:min(560px,calc(100vw - 48px));background:white;padding:42px;border-radius:16px;border:1px solid #d8dce2;box-shadow:0 14px 38px #1d26351a}}h1{{font-size:26px;margin:0 0 18px}}dl{{display:grid;grid-template-columns:88px 1fr;gap:10px;margin:24px 0}}dt{{color:#68717e}}dd{{margin:0;overflow-wrap:anywhere}}.badges{{display:flex;gap:8px;flex-wrap:wrap}}.badges span{{padding:4px 9px;border-radius:999px;background:#e9f3fa;color:#2e6d91;font-size:13px}}button{{border:1px solid #b8c0ca;background:#fff;border-radius:8px;padding:9px 15px;font:inherit;cursor:pointer}}</style></head><body><main class="card"><h1>请求已拦截</h1><p>Net Sentinel 已停止本次明文 HTTP 请求。</p><dl><dt>域名</dt><dd>{}</dd><dt>命中规则</dt><dd>{}</dd><dt>响应结果</dt><dd class="badges">{}</dd></dl><button onclick="history.back()">返回上一页</button></main></body></html>"#,
        html_escape(&domain),
        html_escape(rule_id),
        badges
    );
    let response = format!(
        "HTTP/1.1 403 Forbidden\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    stream.write_all(response.as_bytes())?;
    Ok(())
}

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn write_html(stream: &mut TcpStream, body: &str) -> Result<()> {
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    stream.write_all(response.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        actions::{ActionHandler, ActionOutput, ActionRegistry, HtmlResponse},
        config::{ActionDefinition, MatchMode, MatchTarget, Rule},
    };
    #[cfg(windows)]
    use std::process::Command;
    use std::sync::RwLock;

    fn run_one_proxy_connection(
        config: AppConfig,
    ) -> (SocketAddr, async_channel::Receiver<UiEvent>) {
        run_one_proxy_connection_with(config, ActionRegistry::standard)
    }

    fn run_one_proxy_connection_with(
        config: AppConfig,
        build_actions: impl FnOnce(async_channel::Sender<UiEvent>) -> ActionRegistry,
    ) -> (SocketAddr, async_channel::Receiver<UiEvent>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (ui_tx, ui_rx) = async_channel::unbounded();
        let actions = Arc::new(build_actions(ui_tx.clone()));
        let shared = Arc::new(RwLock::new(config));
        thread::spawn(move || {
            let (client, peer) = listener.accept().unwrap();
            handle_client(client, peer, shared, actions, None, ui_tx).unwrap();
        });
        (address, ui_rx)
    }

    struct MatrixAction {
        kind: &'static str,
        surface: ActionSurface,
    }

    impl ActionHandler for MatrixAction {
        fn kind(&self) -> &'static str {
            self.kind
        }

        fn surface(&self) -> ActionSurface {
            self.surface
        }

        fn execute(
            &self,
            _context: &ActionContext,
            _definition: &ActionDefinition,
        ) -> Result<ActionOutput> {
            Ok(ActionOutput {
                html_response: (self.kind == "serve_html").then(|| HtmlResponse {
                    body: "<h1>matrix html</h1>".into(),
                }),
                image: None,
            })
        }
    }

    #[test]
    fn parses_absolute_http_request() {
        let parsed = ParsedRequest::parse(
            "GET http://example.com:8080/a?q=1 HTTP/1.1\r\nHost: example.com:8080\r\n\r\n",
        )
        .unwrap();
        assert_eq!(parsed.authority, "example.com:8080");
        assert_eq!(parsed.path, "/a?q=1");
    }

    #[test]
    fn parses_connect_for_domain_matching() {
        let parsed = ParsedRequest::parse(
            "CONNECT blocked.example:443 HTTP/1.1\r\nHost: blocked.example:443\r\n\r\n",
        )
        .unwrap();
        assert_eq!(parsed.facts().host, "blocked.example");
        assert_eq!(parsed.facts().url, "https://blocked.example:443/");
    }

    #[test]
    fn parses_windows_socks_proxy_syntax() {
        assert!(matches!(
            parse_upstream("socks=127.0.0.1:7891", false).unwrap(),
            OutboundRoute::Socks5(address) if address == "127.0.0.1:7891"
        ));
    }

    #[test]
    fn upstream_probe_rejects_proxy_loop_before_connecting() {
        let listen = "127.0.0.1:8877".parse().unwrap();
        let error =
            probe_upstream(NetworkMode::Http, Some("http://127.0.0.1:8877"), listen).unwrap_err();
        assert!(error.to_string().contains("代理回环"));
    }

    #[test]
    fn endpoint_probe_rejects_a_port_without_http_forwarding() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        thread::spawn(move || {
            let _ = listener.accept();
        });
        let error = probe_route(&OutboundRoute::Http(address.to_string())).unwrap_err();
        assert!(
            error.to_string().contains("CONNECT")
                || error.to_string().contains("GitHub HTTPS")
                || error.to_string().contains("无效状态")
        );
    }

    #[test]
    fn endpoint_probe_accepts_a_connect_tunnel_response() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let _ = read_headers(&mut stream).unwrap();
            stream
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                .unwrap();
        });
        probe_route(&OutboundRoute::Http(address.to_string())).unwrap();
    }

    #[test]
    fn proxy_service_stays_running_after_local_forward_probe() {
        let port_reservation = TcpListener::bind("127.0.0.1:0").unwrap();
        let listen = port_reservation.local_addr().unwrap();
        drop(port_reservation);

        let mut config = AppConfig::default();
        config.proxy.listen = listen;
        config.proxy.network_mode = NetworkMode::Http;
        config.proxy.upstream_proxy = Some("127.0.0.1:1".into());
        config.blacklist = vec![Rule {
            id: "block-loopback".into(),
            enabled: true,
            target: MatchTarget::Host,
            mode: MatchMode::Exact,
            pattern: "127.0.0.1".into(),
            methods: vec![],
            header_name: None,
            actions: vec![],
        }];
        let (ui_tx, _ui_rx) = async_channel::unbounded();
        let actions = Arc::new(ActionRegistry::standard(ui_tx.clone()));
        let shared = Arc::new(RwLock::new(config));
        let mut service = ProxyService::new(shared, actions, None, ui_tx);

        service.start().unwrap();
        probe_local_proxy(listen).unwrap();
        thread::sleep(Duration::from_millis(120));
        assert!(service.is_running(), "本机转发检查不应停止代理服务");

        service.stop();
        for _ in 0..20 {
            if !service.is_running() {
                break;
            }
            thread::sleep(Duration::from_millis(30));
        }
        assert!(!service.is_running());
    }

    #[test]
    fn connect_tunnel_forwards_delayed_tls_bytes_from_nonblocking_listener() {
        let upstream = TcpListener::bind("127.0.0.1:0").unwrap();
        let upstream_address = upstream.local_addr().unwrap();
        let upstream_thread = thread::spawn(move || {
            let (mut stream, _) = upstream.accept().unwrap();
            let request = read_headers(&mut stream).unwrap();
            assert!(request.starts_with(b"CONNECT github.test:443 HTTP/1.1\r\n"));
            stream
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                .unwrap();
            let mut payload = [0u8; 4];
            stream.read_exact(&mut payload).unwrap();
            assert_eq!(&payload, b"ping");
            stream.write_all(b"pong").unwrap();
        });

        let port_reservation = TcpListener::bind("127.0.0.1:0").unwrap();
        let listen = port_reservation.local_addr().unwrap();
        drop(port_reservation);
        let mut config = AppConfig::default();
        config.proxy.listen = listen;
        config.proxy.network_mode = NetworkMode::Http;
        config.proxy.upstream_proxy = Some(upstream_address.to_string());
        config.blacklist.clear();
        let (ui_tx, _ui_rx) = async_channel::unbounded();
        let actions = Arc::new(ActionRegistry::standard(ui_tx.clone()));
        let shared = Arc::new(RwLock::new(config));
        let mut service = ProxyService::new(shared, actions, None, ui_tx);
        service.start().unwrap();

        let mut client = TcpStream::connect(listen).unwrap();
        client
            .write_all(b"CONNECT github.test:443 HTTP/1.1\r\nHost: github.test:443\r\n\r\n")
            .unwrap();
        let response = read_headers(&mut client).unwrap();
        assert!(response.starts_with(b"HTTP/1.1 200 Connection Established\r\n"));

        // Deliberately wait until the proxy's upload loop is already reading.
        // A Windows socket that incorrectly remains non-blocking fails here with 10035.
        thread::sleep(Duration::from_millis(80));
        client.write_all(b"ping").unwrap();
        let mut echoed = [0u8; 4];
        client.read_exact(&mut echoed).unwrap();
        assert_eq!(&echoed, b"pong");

        drop(client);
        service.stop();
        upstream_thread.join().unwrap();
    }

    #[test]
    fn rule_change_disconnects_existing_tunnel_and_applies_on_reconnect() {
        let upstream = TcpListener::bind("127.0.0.1:0").unwrap();
        let upstream_address = upstream.local_addr().unwrap();
        let upstream_thread = thread::spawn(move || {
            let (mut stream, _) = upstream.accept().unwrap();
            let request = read_headers(&mut stream).unwrap();
            assert!(request.starts_with(b"CONNECT dynamic.test:443 HTTP/1.1\r\n"));
            stream
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                .unwrap();
            let mut byte = [0u8; 1];
            let closed = stream.read(&mut byte);
            assert!(matches!(closed, Ok(0) | Err(_)));
        });

        let port_reservation = TcpListener::bind("127.0.0.1:0").unwrap();
        let listen = port_reservation.local_addr().unwrap();
        drop(port_reservation);
        let mut config = AppConfig::default();
        config.proxy.listen = listen;
        config.proxy.network_mode = NetworkMode::Http;
        config.proxy.upstream_proxy = Some(upstream_address.to_string());
        config.blacklist.clear();
        let shared = Arc::new(RwLock::new(config));
        let (ui_tx, _ui_rx) = async_channel::unbounded();
        let actions = Arc::new(ActionRegistry::standard(ui_tx.clone()));
        let mut service = ProxyService::new(shared.clone(), actions, None, ui_tx);
        service.start().unwrap();

        let mut existing = TcpStream::connect(listen).unwrap();
        existing
            .write_all(b"CONNECT dynamic.test:443 HTTP/1.1\r\nHost: dynamic.test:443\r\n\r\n")
            .unwrap();
        let response = read_headers(&mut existing).unwrap();
        assert!(response.starts_with(b"HTTP/1.1 200 Connection Established\r\n"));

        shared.write().unwrap().blacklist.push(Rule {
            id: "dynamic-block".into(),
            enabled: true,
            target: MatchTarget::Host,
            mode: MatchMode::Exact,
            pattern: "dynamic.test".into(),
            methods: vec![],
            header_name: None,
            actions: vec![],
        });
        assert_eq!(service.disconnect_clients(), 1);
        existing
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut byte = [0u8; 1];
        assert!(matches!(existing.read(&mut byte), Ok(0) | Err(_)));

        let mut reconnected = TcpStream::connect(listen).unwrap();
        reconnected
            .write_all(b"CONNECT dynamic.test:443 HTTP/1.1\r\nHost: dynamic.test:443\r\n\r\n")
            .unwrap();
        let mut blocked = String::new();
        reconnected.read_to_string(&mut blocked).unwrap();
        assert!(blocked.starts_with("HTTP/1.1 403 Forbidden"));

        service.stop();
        upstream_thread.join().unwrap();
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "requires the live Windows upstream proxy and Internet access"]
    fn live_github_https_through_proxy_core_without_system_takeover() {
        let upstream = crate::system::current_system_proxy()
            .expect("Windows 系统代理未启用，无法执行现场链路测试");
        assert_ne!(upstream, "127.0.0.1:8877", "现场测试不得经过系统接管端口");

        let port_reservation = TcpListener::bind("127.0.0.1:0").unwrap();
        let listen = port_reservation.local_addr().unwrap();
        drop(port_reservation);

        let mut config = AppConfig::default();
        config.proxy.listen = listen;
        config.proxy.network_mode = NetworkMode::Http;
        config.proxy.upstream_proxy = Some(upstream.clone());
        config.blacklist.clear();
        let (ui_tx, ui_rx) = async_channel::unbounded();
        let actions = Arc::new(ActionRegistry::standard(ui_tx.clone()));
        let shared = Arc::new(RwLock::new(config));
        let mut service = ProxyService::new(shared, actions, None, ui_tx);
        service.start().unwrap();

        let local_proxy = format!("http://{listen}");
        let output = Command::new("curl.exe")
            .args([
                "--noproxy",
                "",
                "--proxy",
                &local_proxy,
                "--connect-timeout",
                "8",
                "--max-time",
                "20",
                "--verbose",
                "--output",
                "NUL",
                "--write-out",
                "%{http_code}",
                "https://github.com/",
            ])
            .output()
            .expect("无法运行 curl.exe");

        service.stop();
        thread::sleep(Duration::from_millis(120));
        let events = std::iter::from_fn(|| ui_rx.try_recv().ok())
            .map(|event| format!("{event:?}"))
            .collect::<Vec<_>>()
            .join(" | ");
        let status = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let error = String::from_utf8_lossy(&output.stderr).trim().to_string();
        assert!(
            output.status.success() && status != "000",
            "GitHub HTTPS 未通过 Net Sentinel→{upstream}；HTTP={status}；curl={error}；events={events}"
        );
        println!("GitHub HTTPS 已通过 Net Sentinel→{upstream}，HTTP={status}");
    }

    #[test]
    fn blacklist_returns_403_and_emits_event() {
        let config = AppConfig {
            blacklist: vec![Rule {
                id: "block-test".into(),
                enabled: true,
                target: MatchTarget::Host,
                mode: MatchMode::Exact,
                pattern: "blocked.test".into(),
                methods: vec![],
                header_name: None,
                actions: vec![],
            }],
            ..AppConfig::default()
        };
        let (proxy, events) = run_one_proxy_connection(config);
        let mut client = TcpStream::connect(proxy).unwrap();
        client
            .write_all(b"GET http://blocked.test/a HTTP/1.1\r\nHost: blocked.test\r\n\r\n")
            .unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 403 Forbidden"));
        assert!(response.contains("blocked.test"));
        assert!(response.contains("block-test"));
        assert!(response.contains("connection_block"));
        assert!(response.contains("history.back()"));
        assert!(matches!(
            events.recv_blocking().unwrap(),
            UiEvent::Blocked { rule_id, .. } if rule_id == "block-test"
        ));
    }

    #[test]
    fn html_action_replaces_plain_http_block_page() {
        let config = AppConfig {
            blacklist: vec![Rule {
                id: "html-test".into(),
                enabled: true,
                target: MatchTarget::Host,
                mode: MatchMode::Exact,
                pattern: "game.test".into(),
                methods: vec![],
                header_name: None,
                actions: vec!["blocked-game".into()],
            }],
            ..AppConfig::default()
        };
        let (proxy, _events) = run_one_proxy_connection(config);
        let mut client = TcpStream::connect(proxy).unwrap();
        client
            .write_all(b"GET http://game.test/ HTTP/1.1\r\nHost: game.test\r\n\r\n")
            .unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        assert!(response.contains("Net Sentinel"));
        assert!(response.contains("<script>"));
    }

    #[test]
    fn html_action_does_not_attempt_https_certificate_injection() {
        let mut config = AppConfig {
            blacklist: vec![Rule {
                id: "https-html-test".into(),
                enabled: true,
                target: MatchTarget::Host,
                mode: MatchMode::Exact,
                pattern: "secure.test".into(),
                methods: vec![],
                header_name: None,
                actions: vec!["blocked-game".into()],
            }],
            ..AppConfig::default()
        };
        config
            .actions
            .iter_mut()
            .find(|action| action.id == "blocked-game")
            .unwrap()
            .params
            .insert(
                "source".into(),
                "Z:\\definitely-missing\\blocked.html".into(),
            );
        let (proxy, events) = run_one_proxy_connection(config);
        let mut client = TcpStream::connect(proxy).unwrap();
        client
            .write_all(b"CONNECT secure.test:443 HTTP/1.1\r\nHost: secure.test:443\r\n\r\n")
            .unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 403 Forbidden"));
        assert!(!response.contains("<script>"));
        let event = events.recv_blocking().unwrap();
        let UiEvent::Blocked {
            protocol,
            action_results,
            ..
        } = event
        else {
            panic!("expected blocked event");
        };
        assert_eq!(protocol, InterceptionProtocol::Https);
        let html = action_results
            .iter()
            .find(|result| result.kind == "serve_html")
            .unwrap();
        assert_eq!(html.status, ActionExecutionStatus::Unsupported);
        assert!(html.error.as_deref().unwrap().contains("不能读取或注入"));
    }

    #[test]
    fn only_first_successful_html_action_is_used() {
        let mut config = AppConfig::default();
        let mut params = std::collections::BTreeMap::new();
        params.insert("source".into(), "Z:\\never-read\\second.html".into());
        config.actions.push(ActionDefinition {
            id: "second-html".into(),
            kind: "serve_html".into(),
            enabled: true,
            params,
        });
        config.blacklist = vec![Rule {
            id: "two-html".into(),
            enabled: true,
            target: MatchTarget::Host,
            mode: MatchMode::Exact,
            pattern: "two-html.test".into(),
            methods: vec![],
            header_name: None,
            actions: vec!["blocked-game".into(), "second-html".into()],
        }];
        let (proxy, events) = run_one_proxy_connection(config);
        let mut client = TcpStream::connect(proxy).unwrap();
        client
            .write_all(b"GET http://two-html.test/ HTTP/1.1\r\nHost: two-html.test\r\n\r\n")
            .unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        let UiEvent::Blocked { action_results, .. } = events.recv_blocking().unwrap() else {
            panic!("expected blocked event");
        };
        assert_eq!(
            action_results
                .iter()
                .find(|result| result.action_id == "blocked-game")
                .unwrap()
                .status,
            ActionExecutionStatus::Succeeded
        );
        assert_eq!(
            action_results
                .iter()
                .find(|result| result.action_id == "second-html")
                .unwrap()
                .status,
            ActionExecutionStatus::Skipped
        );
    }

    #[test]
    fn action_execution_matrix_covers_http_and_https_surfaces() {
        for (kind, surface) in [
            ("popup_image", ActionSurface::InAppCard),
            ("play_audio", ActionSurface::LocalAudio),
            ("serve_html", ActionSurface::BrowserPage),
        ] {
            for (protocol, request) in [
                (
                    InterceptionProtocol::Http,
                    "GET http://matrix.test/ HTTP/1.1\r\nHost: matrix.test\r\n\r\n",
                ),
                (
                    InterceptionProtocol::Https,
                    "CONNECT matrix.test:443 HTTP/1.1\r\nHost: matrix.test:443\r\n\r\n",
                ),
            ] {
                let action_id = format!("matrix-{kind}");
                let config = AppConfig {
                    actions: vec![ActionDefinition {
                        id: action_id.clone(),
                        kind: kind.into(),
                        enabled: true,
                        params: Default::default(),
                    }],
                    blacklist: vec![Rule {
                        id: "matrix-rule".into(),
                        enabled: true,
                        target: MatchTarget::Host,
                        mode: MatchMode::Exact,
                        pattern: "matrix.test".into(),
                        methods: vec![],
                        header_name: None,
                        actions: vec![action_id.clone()],
                    }],
                    ..AppConfig::default()
                };
                let (proxy, events) = run_one_proxy_connection_with(config, move |_| {
                    let mut registry = ActionRegistry::new();
                    registry.register(MatrixAction { kind, surface });
                    registry
                });
                let mut client = TcpStream::connect(proxy).unwrap();
                client.write_all(request.as_bytes()).unwrap();
                let mut response = String::new();
                client.read_to_string(&mut response).unwrap();
                let UiEvent::Blocked {
                    protocol: actual_protocol,
                    action_results,
                    ..
                } = events.recv_blocking().unwrap()
                else {
                    panic!("expected blocked event");
                };
                assert_eq!(actual_protocol, protocol);
                let result = action_results
                    .iter()
                    .find(|result| result.action_id == action_id)
                    .unwrap();
                assert_eq!(result.surface, surface);
                assert_eq!(
                    result.status,
                    if protocol == InterceptionProtocol::Https && kind == "serve_html" {
                        ActionExecutionStatus::Unsupported
                    } else {
                        ActionExecutionStatus::Succeeded
                    },
                    "{protocol:?} × {kind}"
                );
            }
        }
    }

    #[test]
    fn allowed_http_request_is_forwarded() {
        let upstream = TcpListener::bind("127.0.0.1:0").unwrap();
        let upstream_address = upstream.local_addr().unwrap();
        thread::spawn(move || {
            let (mut stream, _) = upstream.accept().unwrap();
            let request = read_headers(&mut stream).unwrap();
            assert!(request.starts_with(b"GET /hello?q=1 HTTP/1.1\r\n"));
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK")
                .unwrap();
        });

        let mut config = AppConfig::default();
        config.blacklist.clear();
        let (proxy, _events) = run_one_proxy_connection(config);
        let mut client = TcpStream::connect(proxy).unwrap();
        let request = format!(
            "GET http://{upstream_address}/hello?q=1 HTTP/1.1\r\nHost: {upstream_address}\r\n\r\n"
        );
        client.write_all(request.as_bytes()).unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        assert!(response.ends_with("OK"));
    }
}
