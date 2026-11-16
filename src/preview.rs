use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, bail, ensure};
use base64::Engine as _;
use dida_core::{DisplayCommand, DisplayContent, DisplayState, ScreenSize, TimedText};
use serde::Deserialize;
use tokio::sync::{mpsc, watch};

use crate::control::Client;

#[derive(Deserialize)]
struct Playback {
    id: String,
    title: String,
    subtitle: String,
    body: String,
    duration_ms: u64,
    position_ms: u64,
    active: bool,
    accent: u32,
    album_art: Option<String>,
    timeline: Vec<Line>,
}

#[derive(Deserialize)]
struct Line {
    start_ms: u64,
    text: String,
}

impl Playback {
    fn into_state(
        self,
        previous: Option<&DisplayState>,
        encoded_art: &mut Option<String>,
    ) -> Result<DisplayState> {
        let old = previous.map(|state| &state.content);
        let same_art = self.album_art == *encoded_art;
        let album_art = if same_art {
            old.and_then(|content| content.album_art.clone())
        } else {
            self.album_art
                .as_ref()
                .map(|encoded| {
                    ensure!(encoded.len() <= 256 * 1024, "daemon album artwork is too large");
                    // Spotify stores the original JPEG, not decoded RGBA pixels.
                    let bytes = base64::engine::general_purpose::STANDARD
                        .decode(encoded)
                        .context("decoding daemon album artwork")?;
                    ensure!(bytes.len() <= 128 * 1024, "daemon album artwork is too large");
                    Ok::<Arc<[u8]>, anyhow::Error>(bytes.into())
                })
                .transpose()?
        };
        let duration = Duration::from_millis(self.duration_ms);
        let content = if let Some(old) = old.filter(|old| {
            same_art
                && old.id.as_ref() == self.id
                && old.title.as_ref() == self.title
                && old.subtitle.as_ref() == self.subtitle
                && old.body.as_ref() == self.body
                && old.duration == duration
                && old.accent == self.accent
                && old.timeline.len() == self.timeline.len()
                && old.timeline.iter().zip(&self.timeline).all(|(old, new)| {
                    old.start == Duration::from_millis(new.start_ms)
                        && old.text.as_ref() == new.text
                })
        }) {
            Arc::clone(old)
        } else {
            Arc::new(DisplayContent {
                id: self.id.into(),
                title: self.title.into(),
                subtitle: self.subtitle.into(),
                body: self.body.into(),
                duration,
                accent: self.accent,
                album_art,
                timeline: self
                    .timeline
                    .into_iter()
                    .map(|line| TimedText {
                        start: Duration::from_millis(line.start_ms),
                        text: line.text.into(),
                    })
                    .collect(),
            })
        };
        *encoded_art = self.album_art;
        Ok(DisplayState {
            content,
            position: Duration::from_millis(self.position_ms).min(duration),
            active: self.active,
            observed_at: Instant::now(),
        })
    }
}

async fn stopped(shutdown: &mut watch::Receiver<bool>) {
    while !*shutdown.borrow_and_update() {
        if shutdown.changed().await.is_err() {
            break;
        }
    }
}

async fn follow(
    client: Client,
    states: watch::Sender<DisplayState>,
    mut commands: mpsc::Receiver<DisplayCommand>,
    mut encoded_art: Option<String>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let updates = async {
        let mut poll = tokio::time::interval_at(
            tokio::time::Instant::now() + Duration::from_millis(250),
            Duration::from_millis(250),
        );
        poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = poll.tick() => {
                    let json = client.playback().await.context("reading daemon playback")?;
                    let playback: Playback = serde_json::from_str(&json)
                        .context("decoding daemon playback")?;
                    let previous = states.borrow().clone();
                    let state = playback.into_state(Some(&previous), &mut encoded_art)?;
                    states.send_replace(state);
                }
                command = commands.recv() => {
                    let Some(command) = command else { return Ok(()); };
                    let action = match command {
                        DisplayCommand::ToggleActive => "toggle",
                        DisplayCommand::Next => "next",
                        DisplayCommand::SeekBy(-5000) => "seek-back",
                        DisplayCommand::SeekBy(5000) => "seek-forward",
                        _ => bail!("preview requested an unsupported playback command"),
                    };
                    if let Err(error) = client.command(action).await {
                        eprintln!("sending {action} to daemon: {error:#}");
                    }
                }
            }
        }
    };
    tokio::select! {
        result = updates => result,
        _ = stopped(&mut shutdown) => Ok(()),
    }
}

pub fn run(snapshot: Option<PathBuf>) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("starting preview runtime")?;
    let (client, settings, initial, encoded_art) = runtime.block_on(async {
        let client = Client::connect().await.context("connecting preview to daemon")?;
        let settings = client.config().await.context("reading preview configuration")?;
        settings.validate()?;
        let json = client.playback().await.context("reading initial daemon playback")?;
        let playback: Playback = serde_json::from_str(&json)
            .context("decoding initial daemon playback")?;
        let mut encoded_art = None;
        let initial = playback.into_state(None, &mut encoded_art)?;
        Ok::<_, anyhow::Error>((client, settings, initial, encoded_art))
    })?;
    let [width, height] = settings.size.unwrap_or([360, 960]);
    let dimensions = ScreenSize::new(width, height)?;
    let panel = dida_ui::panels::create(&settings.ui)?;
    let (shutdown, shutdown_rx) = watch::channel(false);
    let (states, states_rx) = watch::channel(initial);
    let (commands, command_rx) = mpsc::channel(16);
    let (frames, mut frames_rx) = watch::channel(None);
    let renderer = {
        let _entered = runtime.enter();
        dida_ui::spawn(
            dida_ui::RenderConfig {
                size: dimensions,
                fps: settings.fps,
                panel,
                snapshot: None,
            },
            states_rx,
            frames,
            shutdown_rx.clone(),
        )
    };
    let render_stop = shutdown.clone();
    let renderer = runtime.spawn_blocking(move || {
        let result = renderer
            .join()
            .map_err(|_| anyhow::anyhow!("preview renderer panicked"))
            .and_then(|result| result);
        render_stop.send_replace(true);
        result
    });
    let client_stop = shutdown.clone();
    let client_shutdown = shutdown_rx.clone();
    let client = runtime.spawn(async move {
        let result = follow(client, states, command_rx, encoded_art, client_shutdown).await;
        client_stop.send_replace(true);
        result
    });
    let signal_stop = shutdown.clone();
    let mut signal_shutdown = shutdown_rx.clone();
    let signal = runtime.spawn(async move {
        let mut terminate = match tokio::signal::unix::signal(
            tokio::signal::unix::SignalKind::terminate(),
        ) {
            Ok(signal) => signal,
            Err(error) => {
                signal_stop.send_replace(true);
                return Err(error).context("listening for preview SIGTERM");
            }
        };
        tokio::select! {
            result = tokio::signal::ctrl_c() => {
                signal_stop.send_replace(true);
                result.context("listening for preview shutdown")
            }
            _ = terminate.recv() => {
                signal_stop.send_replace(true);
                Ok(())
            }
            _ = stopped(&mut signal_shutdown) => Ok(()),
        }
    });
    let result = match snapshot {
        Some(path) => runtime.block_on(async {
            let mut stop = shutdown_rx;
            let frame = loop {
                if let Some(frame) = frames_rx.borrow_and_update().clone() {
                    break frame;
                }
                tokio::select! {
                    result = frames_rx.changed() => {
                        result.context("preview renderer stopped before producing a snapshot")?;
                    }
                    _ = stopped(&mut stop) => {
                        bail!("preview stopped before producing a snapshot");
                    }
                }
            };
            shutdown.send_replace(true);
            image::save_buffer_with_format(
                &path,
                &frame.rgba,
                frame.size.width,
                frame.size.height,
                image::ColorType::Rgba8,
                image::ImageFormat::Png,
            )
            .with_context(|| format!("saving snapshot {}", path.display()))?;
            eprintln!("Saved frame {} to {}", frame.sequence, path.display());
            Ok(())
        }),
        None => dida_ui::run_preview(dimensions, frames_rx, commands.clone(), shutdown.clone()),
    };
    shutdown.send_replace(true);
    runtime.block_on(async {
        let client = client.await.context("joining preview client").and_then(|result| result);
        let renderer = renderer.await.context("joining preview renderer").and_then(|result| result);
        let signal = signal.await.context("joining preview signal handler").and_then(|result| result);
        let errors: Vec<_> = [result, client, renderer, signal]
            .into_iter()
            .filter_map(Result::err)
            .map(|error| format!("{error:#}"))
            .collect();
        ensure!(errors.is_empty(), "{}", errors.join("\n"));
        Ok(())
    })
}
