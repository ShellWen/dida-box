mod audio;
mod openrgb;

use std::net::SocketAddr;

use anyhow::Result;
use dida_core::DisplayState;
use tokio::sync::watch;

pub async fn run(
    address: SocketAddr,
    devices: Vec<String>,
    when_paused: bool,
    state: watch::Receiver<DisplayState>,
    shutdown: watch::Receiver<bool>,
    status: watch::Sender<String>,
) -> Result<()> {
    let (audio_tx, audio_rx) = watch::channel(audio::Spectrum::default());
    tokio::try_join!(
        audio::run(audio_tx, shutdown.clone()),
        openrgb::run(address, devices, when_paused, state, audio_rx, shutdown, status),
    )?;
    Ok(())
}
