use std::{future::Future, sync::Arc, time::{Duration, Instant}};

use anyhow::{Context, Result};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use dida_core::{DisplayCommand, DisplayState};
use serde::Serialize;
use tokio::sync::{Mutex, RwLock, mpsc, watch};

use crate::config::{Settings, Store};

pub const SERVICE: &str = "io.github.shellwen.DidaBox1";
pub const PATH: &str = "/io/github/shellwen/DidaBox1";
const RPC_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, Serialize)]
pub struct Status {
    pub provider: String,
    pub display: String,
    pub lighting: String,
    pub playing: bool,
    pub title: String,
    pub accent: u32,
    pub config_error: Option<String>,
}

impl Status {
    pub fn new(config_error: Option<String>) -> Self {
        Self {
            provider: "starting".into(),
            display: "disabled".into(),
            lighting: "disabled".into(),
            playing: false,
            title: String::new(),
            accent: 0x909098,
            config_error,
        }
    }
}

pub struct Service {
    pub store: Store,
    pub settings: watch::Sender<Settings>,
    pub status: Arc<RwLock<Status>>,
    pub playback: watch::Receiver<DisplayState>,
    pub commands: Arc<RwLock<Option<mpsc::Sender<DisplayCommand>>>>,
    pub shutdown: watch::Sender<bool>,
    pub writer: Arc<Mutex<()>>,
}

fn failed(error: impl std::fmt::Display) -> zbus::fdo::Error {
    zbus::fdo::Error::Failed(error.to_string())
}

#[zbus::interface(name = "io.github.shellwen.DidaBox1")]
impl Service {
    async fn get_config(&self) -> zbus::fdo::Result<String> {
        serde_json::to_string(&*self.settings.borrow()).map_err(failed)
    }

    async fn set_setting(&self, key: &str, value_json: &str) -> zbus::fdo::Result<()> {
        let _writer = self.writer.lock().await;
        let candidate = self.settings.borrow().with_setting(key, value_json)
            .map_err(|error| zbus::fdo::Error::InvalidArgs(format!("{error:#}")))?;
        let store = self.store.clone();
        let saved = candidate.clone();
        tokio::task::spawn_blocking(move || store.save(&saved)).await.map_err(failed)?
            .map_err(|error| failed(format!("{error:#}")))?;
        self.settings.send_replace(candidate);
        self.status.write().await.config_error = None;
        Ok(())
    }

    async fn get_status(&self) -> zbus::fdo::Result<String> {
        let mut status = self.status.read().await.clone();
        let state = self.playback.borrow();
        status.playing = state.active;
        status.title = state.content.title.to_string();
        status.accent = state.content.accent;
        serde_json::to_string(&status).map_err(failed)
    }

    async fn get_playback(&self) -> zbus::fdo::Result<String> {
        #[derive(Serialize)]
        struct Line<'a> { start_ms: u64, text: &'a str }
        #[derive(Serialize)]
        struct Playback<'a> {
            id: &'a str,
            title: &'a str,
            subtitle: &'a str,
            body: &'a str,
            duration_ms: u64,
            position_ms: u64,
            active: bool,
            accent: u32,
            album_art: Option<String>,
            timeline: Vec<Line<'a>>,
        }
        let state = self.playback.borrow().clone();
        let content = &state.content;
        let playback = Playback {
            id: &content.id,
            title: &content.title,
            subtitle: &content.subtitle,
            body: &content.body,
            duration_ms: content.duration.as_millis().min(u64::MAX as u128) as u64,
            position_ms: state.position_at(Instant::now()).as_millis().min(u64::MAX as u128) as u64,
            active: state.active,
            accent: content.accent,
            album_art: content.album_art.as_ref().map(|bytes| STANDARD.encode(bytes)),
            timeline: content.timeline.iter().map(|line| Line {
                start_ms: line.start.as_millis().min(u64::MAX as u128) as u64,
                text: &line.text,
            }).collect(),
        };
        let json = serde_json::to_string(&playback).map_err(failed)?;
        if json.len() > 2 * 1024 * 1024 {
            return Err(failed("playback exceeds 2 MiB"));
        }
        Ok(json)
    }

    async fn command(&self, action: &str) -> zbus::fdo::Result<()> {
        let command = match action {
            "toggle" => DisplayCommand::ToggleActive,
            "next" => DisplayCommand::Next,
            "seek-back" => DisplayCommand::SeekBy(-5_000),
            "seek-forward" => DisplayCommand::SeekBy(5_000),
            _ => return Err(zbus::fdo::Error::InvalidArgs("unknown playback command".into())),
        };
        let sender = self.commands.read().await.clone().ok_or_else(|| failed("provider is unavailable"))?;
        sender.try_send(command).map_err(failed)
    }

    async fn quit(&self) {
        self.shutdown.send_replace(true);
    }
}

#[zbus::proxy(
    interface = "io.github.shellwen.DidaBox1",
    default_service = "io.github.shellwen.DidaBox1",
    default_path = "/io/github/shellwen/DidaBox1"
)]
trait Backend {
    async fn get_config(&self) -> zbus::Result<String>;
    async fn set_setting(&self, key: &str, value_json: &str) -> zbus::Result<()>;
    async fn get_status(&self) -> zbus::Result<String>;
    async fn get_playback(&self) -> zbus::Result<String>;
    async fn command(&self, action: &str) -> zbus::Result<()>;
    async fn quit(&self) -> zbus::Result<()>;
}

pub struct Client {
    connection: zbus::Connection,
}

async fn bounded<T>(operation: impl Future<Output = zbus::Result<T>>) -> Result<T> {
    tokio::time::timeout(RPC_TIMEOUT, operation).await.context("daemon request timed out")?
        .context("communicating with dida-box daemon")
}

impl Client {
    pub async fn connect() -> Result<Self> {
        let connection = bounded(zbus::Connection::session()).await?;
        let client = Self { connection };
        client.status().await?;
        Ok(client)
    }

    pub async fn config(&self) -> Result<Settings> {
        let json = bounded(async {
            BackendProxy::new(&self.connection).await?.get_config().await
        }).await?;
        let settings: Settings = serde_json::from_str(&json).context("decoding daemon configuration")?;
        settings.validate()?;
        Ok(settings)
    }

    pub async fn set(&self, key: &str, value_json: &str) -> Result<()> {
        bounded(async { BackendProxy::new(&self.connection).await?.set_setting(key, value_json).await }).await
    }

    pub async fn status(&self) -> Result<String> {
        bounded(async { BackendProxy::new(&self.connection).await?.get_status().await }).await
    }

    pub async fn playback(&self) -> Result<String> {
        bounded(async { BackendProxy::new(&self.connection).await?.get_playback().await }).await
    }

    pub async fn command(&self, action: &str) -> Result<()> {
        bounded(async { BackendProxy::new(&self.connection).await?.command(action).await }).await
    }

    pub async fn quit(&self) -> Result<()> {
        bounded(async { BackendProxy::new(&self.connection).await?.quit().await }).await
    }
}
