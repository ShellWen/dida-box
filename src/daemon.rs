use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result, bail, ensure};
use dida_core::{DisplayCommand, DisplayState};
use futures_util::StreamExt;
use tokio::sync::{Mutex, RwLock, mpsc, watch};

use crate::{config::{Settings, Store}, control::{self, Service, Status}, demo, lighting, spotify};

type SharedStatus = Arc<RwLock<Status>>;
type Commands = Arc<RwLock<Option<mpsc::Sender<DisplayCommand>>>>;

pub async fn run() -> Result<()> {
    let (store, initial, config_error) = Store::load()?;
    let status = Arc::new(RwLock::new(Status::new(config_error)));
    let (settings, settings_rx) = watch::channel(initial.clone());
    let (shutdown, shutdown_rx) = watch::channel(false);
    let (playback, playback_rx) = watch::channel(if initial.source == "demo" { demo::initial() } else { spotify::initial() });
    let commands = Arc::new(RwLock::new(None));
    let connection = tokio::time::timeout(Duration::from_secs(5), zbus::Connection::session())
        .await.context("session D-Bus connection timed out")?.context("connecting to session D-Bus")?;
    let writer = Arc::new(Mutex::new(()));
    let initializing = writer.lock().await;
    connection.object_server().at(control::PATH, Service {
        store: store.clone(),
        settings: settings.clone(),
        status: status.clone(),
        playback: playback_rx.clone(),
        commands: commands.clone(),
        shutdown: shutdown.clone(),
        writer: writer.clone(),
    }).await?;
    let ownership = connection.request_name_with_flags(control::SERVICE, zbus::fdo::RequestNameFlags::DoNotQueue.into()).await
        .context("another dida-box daemon may already be running")?;
    ensure!(ownership == zbus::fdo::RequestNameReply::PrimaryOwner, "another dida-box daemon is already running");
    let config_valid = status.read().await.config_error.is_none();
    if config_valid && let Err(error) = store.initialize(&initial) {
        status.write().await.config_error = Some(format!("{error:#}"));
    }
    drop(initializing);
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut workers = Vec::new();
    for kind in [Kind::Provider, Kind::Display, Kind::Lighting] {
        workers.push(tokio::spawn(supervise(
            kind, settings_rx.clone(), shutdown_rx.clone(), status.clone(), playback.clone(), commands.clone(),
        )));
    }
    let mut stop = shutdown_rx;
    let mut bus = zbus::MessageStream::from(&connection);
    let outcome = tokio::select! {
        result = tokio::signal::ctrl_c() => result.context("listening for Ctrl-C"),
        _ = terminate.recv() => Ok(()),
        _ = stopped(&mut stop) => Ok(()),
        _ = async { while let Some(Ok(_)) = bus.next().await {} } => Err(anyhow::anyhow!("session D-Bus disconnected")),
    };
    shutdown.send_replace(true);
    for worker in workers {
        if let Err(error) = worker.await {
            eprintln!("worker supervisor: {error}");
        }
    }
    drop(bus);
    connection.release_name(control::SERVICE).await?;
    outcome
}

#[derive(Clone, Copy)]
enum Kind { Provider, Display, Lighting }

impl Kind {
    fn changed(self, old: &Settings, new: &Settings) -> bool {
        match self {
            Self::Provider => old.source != new.source || old.spotify_listen != new.spotify_listen,
            Self::Display => old.device != new.device || old.device_index != new.device_index
                || old.force_driver != new.force_driver || old.size != new.size || old.rotation != new.rotation
                || old.ui != new.ui || old.fps != new.fps || old.quality != new.quality || old.baud != new.baud,
            Self::Lighting => old.openrgb != new.openrgb || old.openrgb_device != new.openrgb_device
                || old.openrgb_when_paused != new.openrgb_when_paused,
        }
    }

    async fn status(self, status: &SharedStatus, text: String) {
        let mut status = status.write().await;
        match self {
            Self::Provider => status.provider = text,
            Self::Display => status.display = text,
            Self::Lighting => status.lighting = text,
        }
    }
}

async fn stopped(shutdown: &mut watch::Receiver<bool>) {
    while !*shutdown.borrow_and_update() {
        if shutdown.changed().await.is_err() { break; }
    }
}

async fn reconfigured(kind: Kind, current: &Settings, settings: &mut watch::Receiver<Settings>) {
    loop {
        if kind.changed(current, &settings.borrow_and_update()) { return; }
        if settings.changed().await.is_err() { return; }
    }
}

async fn supervise(
    kind: Kind,
    mut settings: watch::Receiver<Settings>,
    mut shutdown: watch::Receiver<bool>,
    status: SharedStatus,
    playback: watch::Sender<DisplayState>,
    commands: Commands,
) {
    loop {
        if *shutdown.borrow() { break; }
        let current = settings.borrow_and_update().clone();
        let enabled = match kind {
            Kind::Provider => true,
            Kind::Display => current.device.is_some(),
            Kind::Lighting => current.openrgb.is_some(),
        };
        if !enabled {
            kind.status(&status, "disabled".into()).await;
            tokio::select! {
                _ = stopped(&mut shutdown) => break,
                _ = reconfigured(kind, &current, &mut settings) => continue,
            }
        }
        let (stop, stop_rx) = watch::channel(false);
        let worker_settings = current.clone();
        let worker_status = status.clone();
        let worker_playback = playback.clone();
        let worker_commands = commands.clone();
        let mut worker = tokio::spawn(async move {
            match kind {
                Kind::Provider => provider(worker_settings, worker_playback, worker_commands, stop_rx, worker_status).await,
                Kind::Display => display(worker_settings, worker_playback.subscribe(), stop_rx, worker_status).await,
                Kind::Lighting => lights(worker_settings, worker_playback.subscribe(), stop_rx, worker_status).await,
            }
        });
        let result = tokio::select! {
            result = &mut worker => Some(result),
            _ = stopped(&mut shutdown) => None,
            _ = reconfigured(kind, &current, &mut settings) => None,
        };
        let failed = result.is_some();
        let result = match result {
            Some(result) => result,
            None => {
                kind.status(&status, "stopping".into()).await;
                stop.send_replace(true);
                worker.await
            }
        };
        if matches!(kind, Kind::Provider) {
            *commands.write().await = None;
            playback.send_replace(if current.source == "demo" { demo::initial() } else { spotify::initial() });
        }
        if *shutdown.borrow() { break; }
        if failed {
            let message = match result {
                Ok(Ok(())) => "worker stopped unexpectedly".to_owned(),
                Ok(Err(error)) => format!("{error:#}"),
                Err(error) => format!("worker task failed: {error}"),
            };
            kind.status(&status, format!("error: {message}; retrying")).await;
            tokio::select! {
                _ = stopped(&mut shutdown) => break,
                _ = reconfigured(kind, &current, &mut settings) => {},
                _ = tokio::time::sleep(Duration::from_secs(2)) => {},
            }
        }
    }
    kind.status(&status, "stopped".into()).await;
}

async fn provider(
    settings: Settings,
    playback: watch::Sender<DisplayState>,
    commands: Commands,
    mut shutdown: watch::Receiver<bool>,
    status: SharedStatus,
) -> Result<()> {
    let (sender, receiver) = mpsc::channel(16);
    if settings.source == "demo" {
        playback.send_replace(demo::initial());
        *commands.write().await = Some(sender);
        Kind::Provider.status(&status, "demo".into()).await;
        demo::run(playback, receiver, shutdown).await;
        return Ok(());
    }
    Kind::Provider.status(&status, format!("binding {}", settings.spotify_listen)).await;
    let listener = tokio::select! {
        _ = stopped(&mut shutdown) => return Ok(()),
        result = spotify::bind(settings.spotify_listen) => result?,
    };
    *commands.write().await = Some(sender);
    let mut state = playback.subscribe();
    let provider = spotify::run(listener, playback, receiver, shutdown);
    tokio::pin!(provider);
    loop {
        let waiting = state.borrow_and_update().content.id.as_ref() == "spotify-waiting";
        Kind::Provider.status(&status, if waiting {
            format!("listening {}; waiting for Spotify", settings.spotify_listen)
        } else { "Spotify playback received".into() }).await;
        tokio::select! {
            result = &mut provider => return result,
            changed = state.changed() => if changed.is_err() { bail!("playback channel closed"); },
        }
    }
}

async fn display(
    settings: Settings,
    state: watch::Receiver<DisplayState>,
    mut shutdown: watch::Receiver<bool>,
    status: SharedStatus,
) -> Result<()> {
    let endpoint = settings.device.as_deref().context("display is disabled")?;
    Kind::Display.status(&status, format!("connecting {endpoint}")).await;
    let mut display = tokio::select! {
        _ = stopped(&mut shutdown) => return Ok(()),
        result = dida_drivers::connect(endpoint, settings.force_driver.as_deref(), settings.device_index, settings.baud, settings.quality) => result?,
    };
    display.set_rotation(settings.rotation)?;
    if let Some([width, height]) = settings.size {
        ensure!(display.size() == dida_core::ScreenSize::new(width, height)?, "configured size differs from negotiated device resolution");
    }
    let identity = display.identity().to_string();
    let (stop, stop_rx) = watch::channel(false);
    let (frames, frame_rx) = watch::channel(None);
    let mut readiness = frame_rx.clone();
    let renderer = dida_ui::spawn(
        dida_ui::RenderConfig {
            size: display.logical_size(),
            fps: settings.fps,
            panel: dida_ui::panels::create(&settings.ui)?,
            snapshot: None,
        },
        state, frames, stop_rx.clone(),
    );
    let mut renderer = tokio::task::spawn_blocking(move || renderer.join()
        .map_err(|_| anyhow::anyhow!("GPUI renderer panicked"))?);
    let mut driver = tokio::spawn(display.run(frame_rx, stop_rx));
    Kind::Display.status(&status, format!("connected {identity}; starting renderer")).await;
    enum End { Stop, Renderer(Result<Result<()>, tokio::task::JoinError>), Driver(Result<Result<()>, tokio::task::JoinError>) }
    let mut ready = false;
    let end = loop {
        tokio::select! {
            _ = stopped(&mut shutdown) => break End::Stop,
            result = &mut renderer => break End::Renderer(result),
            result = &mut driver => break End::Driver(result),
            changed = readiness.changed(), if !ready => {
                if changed.is_ok() && readiness.borrow().is_some() {
                    ready = true;
                    Kind::Display.status(&status, format!("rendering to {identity}")).await;
                } else if changed.is_err() {
                    break End::Stop;
                }
            }
        }
    };
    stop.send_replace(true);
    match end {
        End::Stop => {
            let rendered = renderer.await.context("joining renderer");
            let driven = driver.await.context("joining display");
            rendered??;
            driven??;
            Ok(())
        }
        End::Renderer(result) => {
            let _ = driver.await;
            result.context("renderer task")??;
            bail!("renderer stopped unexpectedly")
        }
        End::Driver(result) => {
            let _ = renderer.await;
            result.context("display task")??;
            bail!("display stopped unexpectedly")
        }
    }
}

async fn lights(
    settings: Settings,
    state: watch::Receiver<DisplayState>,
    shutdown: watch::Receiver<bool>,
    status: SharedStatus,
) -> Result<()> {
    let address = settings.openrgb.context("lighting is disabled")?;
    let (sender, mut receiver) = watch::channel("starting".to_owned());
    let lighting = lighting::run(address, settings.openrgb_device, settings.openrgb_when_paused, state, shutdown, sender);
    tokio::pin!(lighting);
    loop {
        let text = receiver.borrow_and_update().clone();
        Kind::Lighting.status(&status, text).await;
        tokio::select! {
            result = &mut lighting => return result,
            changed = receiver.changed() => if changed.is_err() { return lighting.await; },
        }
    }
}
