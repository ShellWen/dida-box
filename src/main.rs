mod demo;
mod spotify;

use std::{net::SocketAddr, path::PathBuf, time::Duration};

use anyhow::{Context, Result, ensure};
use clap::Parser;
use dida_core::ScreenSize;
use tokio::sync::{mpsc, watch};

#[derive(Parser, Debug)]
#[command(
    version,
    about = "GPUI offscreen display with independent async screen drivers"
)]
struct Args {
    /// OS path, unix:/path.sock, pipe:\\.\pipe\name, or usb:VID:PID
    #[arg(long)]
    device: Option<String>,
    /// Zero-based index when multiple USB serial devices match --device
    #[arg(long, default_value_t = 0)]
    device_index: usize,
    /// Force a driver instead of using the USB VID/PID driver map
    #[arg(long, value_parser = clap::builder::PossibleValuesParser::new(dida_drivers::DRIVER_IDS))]
    force_driver: Option<String>,
    /// Display the upright logical framebuffer in a local window
    #[arg(long)]
    preview: bool,
    /// Native output size; device sessions use the negotiated size
    #[arg(long, value_parser = parse_size)]
    size: Option<ScreenSize>,
    /// Override the DLS driver's automatic native output rotation (clockwise)
    #[arg(long, value_parser = parse_rotation)]
    rotation: Option<u16>,
    /// UI module
    #[arg(long, default_value = "timeline", value_parser = clap::builder::PossibleValuesParser::new(dida_ui::panels::PANEL_IDS))]
    ui: String,
    /// Data provider; demo is an explicit offline source
    #[arg(long, default_value = "spotify", value_parser = ["spotify", "demo"])]
    source: String,
    /// Local Spotify extension WebSocket listener
    #[arg(long, default_value = "127.0.0.1:17876")]
    spotify_listen: SocketAddr,
    #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u32).range(1..=30))]
    fps: u32,
    #[arg(long, default_value_t = 60, value_parser = clap::value_parser!(u8).range(1..=100))]
    quality: u8,
    /// CDC line coding; start with the documented 9600 baud
    #[arg(long, default_value_t = 9600, value_parser = clap::value_parser!(u32).range(1..))]
    baud: u32,
    /// Save the final offscreen framebuffer as PNG on shutdown
    #[arg(long)]
    snapshot: Option<PathBuf>,
    /// Stop automatically after this many seconds
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    duration: Option<u64>,
}

fn parse_size(value: &str) -> std::result::Result<ScreenSize, String> {
    let (width, height) = value.split_once('x').ok_or("expected WIDTHxHEIGHT")?;
    let width = width.parse::<u32>().map_err(|_| "invalid width")?;
    let height = height.parse::<u32>().map_err(|_| "invalid height")?;
    ScreenSize::new(width, height).map_err(|error| error.to_string())
}

fn parse_rotation(value: &str) -> std::result::Result<u16, String> {
    match value {
        "0" => Ok(0),
        "90" => Ok(90),
        "180" => Ok(180),
        "270" => Ok(270),
        _ => Err("expected 0, 90, 180 or 270".into()),
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    ensure!(
        args.device.is_some() || args.preview || args.snapshot.is_some(),
        "specify --device PATH, --preview, or --snapshot FILE"
    );
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    ensure!(
        args.device.is_some() || args.rotation.is_none(),
        "--rotation requires --device; preview-only frames are always upright"
    );
    let listener = if args.source == "spotify" {
        Some(runtime.block_on(spotify::bind(args.spotify_listen))?)
    } else {
        None
    };
    let display = match &args.device {
        Some(endpoint) => {
            let mut display = runtime
                .block_on(dida_drivers::connect(
                    endpoint,
                    args.force_driver.as_deref(),
                    args.device_index,
                    args.baud,
                    args.quality,
                ))
                .with_context(|| format!("connecting to {endpoint}"))?;
            display.set_rotation(args.rotation)?;
            Some(display)
        }
        None => None,
    };
    let dimensions = match (&display, args.size) {
        (Some(display), supplied) => {
            let native = display.size();
            if let Some(supplied) = supplied {
                ensure!(
                    supplied == native,
                    "--size differs from negotiated device resolution {}x{}",
                    native.width,
                    native.height
                );
            }
            eprintln!(
                "Connected: {} ({}x{})",
                display.identity(),
                native.width,
                native.height
            );
            display.logical_size()
        }
        (None, supplied) => supplied.unwrap_or(ScreenSize::new(360, 960)?),
    };
    let panel = dida_ui::panels::create(&args.ui)?;
    let (shutdown, shutdown_rx) = watch::channel(false);
    let initial = if listener.is_some() {
        spotify::initial()
    } else {
        demo::initial()
    };
    let (data_tx, data_rx) = watch::channel(initial);
    let (command_tx, command_rx) = mpsc::channel(16);
    let (frames_tx, frames_rx) = watch::channel(None);
    let provider_shutdown = shutdown.clone();
    let provider_stop = shutdown_rx.clone();
    let provider = runtime.spawn(async move {
        let result = match listener {
            Some(listener) => spotify::run(listener, data_tx, command_rx, provider_stop).await,
            None => {
                demo::run(data_tx, command_rx, provider_stop).await;
                Ok(())
            }
        };
        if let Err(error) = &result {
            eprintln!("data provider: {error:#}");
        }
        provider_shutdown.send_replace(true);
        result
    });
    let signal_shutdown = shutdown.clone();
    let signal = runtime.spawn(async move {
        tokio::select! {
            result = tokio::signal::ctrl_c() => { if let Err(error) = result { eprintln!("signal handler: {error}"); } }
            _ = async { match args.duration { Some(seconds) => tokio::time::sleep(Duration::from_secs(seconds)).await, None => std::future::pending().await } } => {}
        }
        signal_shutdown.send_replace(true);
    });
    let device = display.map(|display| {
        let shutdown = shutdown.clone();
        let frames = frames_rx.clone();
        let stop = shutdown_rx.clone();
        runtime.spawn(async move {
            let result = display.run(frames, stop).await;
            if let Err(error) = &result {
                eprintln!("display driver: {error:#}");
            }
            shutdown.send_replace(true);
            result
        })
    });
    let renderer = {
        let _entered = runtime.enter();
        dida_ui::spawn(
            dida_ui::RenderConfig {
                size: dimensions,
                fps: args.fps,
                panel,
                snapshot: args.snapshot,
            },
            data_rx,
            frames_tx,
            shutdown_rx.clone(),
        )
    };
    let render_shutdown = shutdown.clone();
    let renderer = runtime.spawn(async move {
        let result = tokio::task::spawn_blocking(move || {
            renderer
                .join()
                .map_err(|_| anyhow::anyhow!("GPUI renderer panicked"))?
        })
        .await
        .context("joining renderer");
        render_shutdown.send_replace(true);
        result?
    });
    let preview = if args.preview {
        dida_ui::run_preview(dimensions, frames_rx, command_tx, shutdown.clone())
    } else {
        runtime.block_on(async {
            let mut stop = shutdown_rx;
            while !*stop.borrow() {
                if stop.changed().await.is_err() {
                    break;
                }
            }
        });
        Ok(())
    };
    shutdown.send_replace(true);
    runtime.block_on(async {
        let render_result = renderer.await.context("renderer task")?;
        let driver_result = match device {
            Some(device) => device.await.context("driver task")?,
            None => Ok(()),
        };
        let provider_result = provider.await.context("data provider task")?;
        signal.abort();
        preview?;
        render_result?;
        provider_result?;
        driver_result
    })
}
