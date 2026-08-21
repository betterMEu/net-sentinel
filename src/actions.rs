use std::{
    collections::HashMap,
    fs::{self, File},
    io::{BufReader, Cursor},
    path::PathBuf,
    sync::Arc,
    thread,
};

use anyhow::{Context as _, Result, anyhow};
use async_channel::Sender;
use rodio::{Decoder, OutputStream, OutputStreamBuilder, Sink};

use crate::{
    assets,
    config::ActionDefinition,
    events::{ActionSurface, ImagePresentation, ImageSource, UiEvent},
};

#[derive(Debug, Clone)]
pub struct ActionContext {
    pub rule_id: String,
    pub request: String,
}

#[derive(Debug, Clone)]
pub struct HtmlResponse {
    pub body: String,
}

#[derive(Debug, Clone, Default)]
pub struct ActionOutput {
    pub html_response: Option<HtmlResponse>,
    pub image: Option<ImagePresentation>,
}

pub trait ActionHandler: Send + Sync {
    fn kind(&self) -> &'static str;
    fn surface(&self) -> ActionSurface;
    fn execute(
        &self,
        context: &ActionContext,
        definition: &ActionDefinition,
    ) -> Result<ActionOutput>;
}

pub struct ActionRegistry {
    handlers: HashMap<String, Arc<dyn ActionHandler>>,
}

impl ActionRegistry {
    pub fn new() -> Self {
        Self {
            handlers: HashMap::new(),
        }
    }

    pub fn standard(ui_tx: Sender<UiEvent>) -> Self {
        let mut registry = Self::new();
        registry.register(PopupImageAction);
        let _ = ui_tx;
        registry.register(PlayAudioAction);
        registry.register(ServeHtmlAction);
        registry
    }

    pub fn register(&mut self, handler: impl ActionHandler + 'static) {
        self.handlers
            .insert(handler.kind().to_string(), Arc::new(handler));
    }

    pub fn execute(
        &self,
        context: &ActionContext,
        definition: &ActionDefinition,
    ) -> Result<ActionOutput> {
        if !definition.enabled {
            return Ok(ActionOutput::default());
        }
        let handler = self
            .handlers
            .get(&definition.kind)
            .ok_or_else(|| anyhow!("未注册 Action 类型: {}", definition.kind))?;
        handler.execute(context, definition)
    }

    pub fn surface(&self, kind: &str) -> Option<ActionSurface> {
        self.handlers.get(kind).map(|handler| handler.surface())
    }
}

struct PopupImageAction;

impl ActionHandler for PopupImageAction {
    fn kind(&self) -> &'static str {
        "popup_image"
    }

    fn surface(&self) -> ActionSurface {
        ActionSurface::InAppCard
    }

    fn execute(
        &self,
        _context: &ActionContext,
        definition: &ActionDefinition,
    ) -> Result<ActionOutput> {
        let configured = definition
            .params
            .get("source")
            .map(String::as_str)
            .unwrap_or("builtin:blocked");
        let source = if configured == "builtin:blocked" || configured.is_empty() {
            ImageSource::BuiltinBlocked
        } else {
            fs::metadata(configured)
                .with_context(|| format!("无法读取图片 Action 文件 {configured}"))?;
            ImageSource::File(PathBuf::from(configured))
        };
        let duration_ms = definition
            .params
            .get("duration_ms")
            .and_then(|value| value.parse().ok())
            .unwrap_or(4500);
        Ok(ActionOutput {
            html_response: None,
            image: Some(ImagePresentation {
                source,
                duration_ms,
            }),
        })
    }
}

struct PlayAudioAction;

impl ActionHandler for PlayAudioAction {
    fn kind(&self) -> &'static str {
        "play_audio"
    }

    fn surface(&self) -> ActionSurface {
        ActionSurface::LocalAudio
    }

    fn execute(
        &self,
        context: &ActionContext,
        definition: &ActionDefinition,
    ) -> Result<ActionOutput> {
        let source = definition
            .params
            .get("source")
            .cloned()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("Action {} 没有音频 source", definition.id))?;
        let volume = definition
            .params
            .get("volume")
            .and_then(|value| value.parse::<f32>().ok())
            .unwrap_or(0.7)
            .clamp(0.0, 1.5);
        let (stream, sink) = prepare_playback(&source, volume).with_context(|| {
            format!(
                "规则 {} 的音频 Action 无法开始（{}）",
                context.rule_id, context.request
            )
        })?;
        thread::Builder::new()
            .name("net-sentinel-audio".into())
            .spawn(move || {
                sink.sleep_until_end();
                drop(stream);
            })
            .context("无法启动音频线程")?;
        Ok(ActionOutput::default())
    }
}

struct ServeHtmlAction;

impl ActionHandler for ServeHtmlAction {
    fn kind(&self) -> &'static str {
        "serve_html"
    }

    fn surface(&self) -> ActionSurface {
        ActionSurface::BrowserPage
    }

    fn execute(
        &self,
        _context: &ActionContext,
        definition: &ActionDefinition,
    ) -> Result<ActionOutput> {
        const MAX_HTML_BYTES: u64 = 2 * 1024 * 1024;
        let source = definition
            .params
            .get("source")
            .map(String::as_str)
            .unwrap_or("builtin:mini-game");
        let body = if source == "builtin:mini-game" || source.is_empty() {
            include_str!("../assets/blocked-game.html").to_string()
        } else {
            let metadata = fs::metadata(source)
                .with_context(|| format!("无法读取 HTML Action 文件 {source}"))?;
            if metadata.len() > MAX_HTML_BYTES {
                return Err(anyhow!("HTML Action 文件超过 2 MiB 限制"));
            }
            fs::read_to_string(source)
                .with_context(|| format!("HTML Action 文件不是有效 UTF-8：{source}"))?
        };
        Ok(ActionOutput {
            html_response: Some(HtmlResponse { body }),
            image: None,
        })
    }
}

fn prepare_playback(source: &str, volume: f32) -> Result<(OutputStream, Sink)> {
    let stream = OutputStreamBuilder::open_default_stream().context("没有可用的音频输出设备")?;
    let sink = Sink::connect_new(stream.mixer());
    sink.set_volume(volume);

    if let Some(bytes) = assets::builtin_audio(source) {
        let decoder = Decoder::try_from(Cursor::new(bytes)).context("内置音频无法解码")?;
        sink.append(decoder);
    } else {
        let file = File::open(source).with_context(|| format!("无法打开音频文件 {source}"))?;
        let decoder = Decoder::try_from(BufReader::new(file)).context("音频文件无法解码")?;
        sink.append(decoder);
    }
    Ok((stream, sink))
}
