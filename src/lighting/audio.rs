use std::{f32::consts::TAU, process::Stdio, sync::Arc, time::Duration};

use anyhow::{Context, Result, bail};
use rustfft::{Fft, FftPlanner, num_complex::Complex32};
use tokio::{
    io::AsyncReadExt,
    process::{Child, ChildStdout, Command},
    sync::watch,
    time::{Instant, MissedTickBehavior, interval, sleep, timeout},
};

pub(super) const BANDS: usize = 12;
const FFT_SIZE: usize = 2048;
const HOP: usize = 1536; // 32 ms at 48 kHz: 31.25 spectrum updates per second.
const RATE: f32 = 48_000.0;
const RETRY: Duration = Duration::from_secs(1);
const READ_TIMEOUT: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, Default)]
pub(super) struct Spectrum {
    pub levels: [f32; BANDS],
    pub captured_at: Option<Instant>,
}

#[derive(Clone, PartialEq)]
struct DefaultSink {
    metadata_id: u32,
    name: String,
}

async fn stopped(shutdown: &mut watch::Receiver<bool>) {
    loop {
        if *shutdown.borrow_and_update() || shutdown.changed().await.is_err() {
            return;
        }
    }
}

async fn reap(child: &mut Child) -> Result<()> {
    // start_kill may race a normal exit; wait still reaps either outcome.
    let _ = child.start_kill();
    child.wait().await.context("reaping PipeWire helper")?;
    Ok(())
}

async fn default_sink(shutdown: &mut watch::Receiver<bool>) -> Result<Option<DefaultSink>> {
    let mut child = Command::new("pw-metadata")
        .args(["-n", "default", "0", "default.audio.sink"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("starting pw-metadata")?;
    let result = async {
        let mut stdout = child.stdout.take().context("opening pw-metadata stdout")?;
        let mut bytes = [0_u8; 16_384];
        let query = async {
            let mut used = 0;
            loop {
                if used == bytes.len() {
                    bail!("default sink metadata exceeds 16 KiB");
                }
                let count = stdout.read(&mut bytes[used..]).await?;
                if count == 0 {
                    break;
                }
                used += count;
            }
            if !child
                .wait()
                .await
                .context("waiting for pw-metadata")?
                .success()
            {
                bail!("pw-metadata could not query the default sink");
            }
            Ok::<usize, anyhow::Error>(used)
        };
        tokio::select! {
            biased;
            _ = stopped(shutdown) => Ok(None),
            result = timeout(Duration::from_millis(750), query) => {
                let used = result.context("timed out querying the default sink")??;
                let text = std::str::from_utf8(&bytes[..used])?;
                let metadata_id = text.lines().find_map(|line| {
                    line.strip_prefix("Found \"default\" metadata ")?.parse().ok()
                });
                let value = text.lines().find_map(|line| {
                    line.strip_prefix("update: id:0 key:'default.audio.sink' value:'")?
                        .rsplit_once("' type:")
                        .map(|(value, _)| value)
                });
                let (Some(metadata_id), Some(value)) = (metadata_id, value) else {
                    return Ok(None);
                };
                let value: serde_json::Value = serde_json::from_str(value)?;
                let Some(name) = value.get("name").and_then(serde_json::Value::as_str) else {
                    bail!("default sink metadata has no name");
                };
                if name.is_empty() {
                    return Ok(None);
                }
                Ok(Some(DefaultSink { metadata_id, name: name.to_owned() }))
            }
        }
    }
    .await;
    reap(&mut child).await?;
    result
}

async fn follow_default(
    tx: watch::Sender<Option<DefaultSink>>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut poll = interval(RETRY);
    poll.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut failed = false;
    loop {
        tokio::select! {
            biased;
            _ = stopped(&mut shutdown) => break,
            _ = tx.closed() => break,
            _ = poll.tick() => {}
        }
        let sink = match default_sink(&mut shutdown).await {
            Ok(sink) => {
                failed = false;
                sink
            }
            Err(error) => {
                if !failed {
                    eprintln!("lighting audio: {error:#}");
                }
                failed = true;
                None
            }
        };
        tx.send_if_modified(|previous| {
            if *previous == sink {
                false
            } else {
                *previous = sink;
                true
            }
        });
    }
}

pub(super) async fn run(
    tx: watch::Sender<Spectrum>,
    shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let (default_tx, default_rx) = watch::channel(None);
    let (_, result) = tokio::join!(
        follow_default(default_tx, shutdown.clone()),
        capture(tx, default_rx, shutdown),
    );
    result
}

async fn capture(
    tx: watch::Sender<Spectrum>,
    mut defaults: watch::Receiver<Option<DefaultSink>>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let mut analyzer = Analyzer::new();
    let mut bytes = [0_u8; HOP * 8];
    let mut frames = [[0.0_f32; 2]; HOP];
    let mut failed = false;
    tx.send_replace(Spectrum::default());
    loop {
        if *shutdown.borrow() || tx.is_closed() {
            break;
        }
        let sink = defaults.borrow_and_update().clone();
        let Some(sink) = sink else {
            tokio::select! {
                biased;
                _ = stopped(&mut shutdown) => break,
                _ = tx.closed() => break,
                changed = defaults.changed() => { if changed.is_err() { break; } }
            }
            continue;
        };
        // We follow defaults ourselves; policy target metadata must not move this monitor.
        let spawned = Command::new("pw-record")
            .args([
                "--raw", "--format", "f32", "--rate", "48000", "--channels", "2",
                "--latency", "20ms", "--target", &sink.name, "--properties",
                "{ stream.capture.sink = true node.dont-reconnect = false node.dont-fallback = true node.dont-move = true node.name = dida-box-lighting }",
                "-",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn();
        match spawned {
            Ok(mut child) => {
                let result = async {
                    let mut stdout = child.stdout.take().context("opening pw-record stdout")?;
                    stream(
                        &mut stdout,
                        &mut analyzer,
                        &mut bytes,
                        &mut frames,
                        &tx,
                        &mut defaults,
                        &mut shutdown,
                    )
                    .await
                }
                .await;
                tx.send_replace(Spectrum::default());
                analyzer.reset();
                reap(&mut child).await?;
                if let Err(error) = result {
                    if !failed {
                        eprintln!("lighting audio: {error:#}");
                    }
                    failed = true;
                } else {
                    failed = false;
                }
            }
            Err(error) => {
                if !failed {
                    eprintln!("lighting audio: starting pw-record: {error}");
                }
                failed = true;
            }
        }
        // Metadata changes restart immediately; unavailable servers retry at most once a second.
        if defaults.borrow().as_ref() != Some(&sink) {
            continue;
        }
        tokio::select! {
            biased;
            _ = stopped(&mut shutdown) => break,
            _ = tx.closed() => break,
            changed = defaults.changed() => { if changed.is_err() { break; } }
            _ = sleep(RETRY) => {}
        }
    }
    tx.send_replace(Spectrum::default());
    Ok(())
}

async fn stream(
    stdout: &mut ChildStdout,
    analyzer: &mut Analyzer,
    bytes: &mut [u8; HOP * 8],
    frames: &mut [[f32; 2]; HOP],
    tx: &watch::Sender<Spectrum>,
    defaults: &mut watch::Receiver<Option<DefaultSink>>,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<()> {
    loop {
        tokio::select! {
            biased;
            _ = stopped(shutdown) => return Ok(()),
            _ = tx.closed() => return Ok(()),
            _ = defaults.changed() => return Ok(()),
            result = timeout(READ_TIMEOUT, stdout.read_exact(bytes)) => {
                result.context("PipeWire capture stalled")?.context("reading PipeWire capture")?;
            }
        }
        for (frame, bytes) in frames.iter_mut().zip(bytes.chunks_exact(8)) {
            frame[0] = f32::from_le_bytes(bytes[..4].try_into().unwrap());
            frame[1] = f32::from_le_bytes(bytes[4..].try_into().unwrap());
        }
        tx.send_replace(Spectrum {
            levels: analyzer.analyze(frames),
            captured_at: Some(Instant::now()),
        });
    }
}

struct Analyzer {
    fft: Arc<dyn Fft<f32>>,
    scratch: Vec<Complex32>,
    buffer: Vec<Complex32>,
    samples: [[f32; 2]; FFT_SIZE],
    window: [f32; FFT_SIZE],
    edges: [usize; BANDS + 1],
    position: usize,
    normalization: f32,
    levels: [f32; BANDS],
    attack: f32,
    release: f32,
}

impl Analyzer {
    fn new() -> Self {
        let fft = FftPlanner::new().plan_fft_forward(FFT_SIZE);
        let window = std::array::from_fn(|i| 0.5 - 0.5 * (TAU * i as f32 / FFT_SIZE as f32).cos());
        let energy: f32 = window.iter().map(|value| value * value).sum();
        let edges = std::array::from_fn(|i| {
            let hz = 40.0 * 400.0_f32.powf(i as f32 / BANDS as f32);
            (hz * FFT_SIZE as f32 / RATE).ceil() as usize
        });
        Self {
            scratch: vec![Complex32::default(); fft.get_inplace_scratch_len()],
            buffer: vec![Complex32::default(); FFT_SIZE],
            fft,
            samples: [[0.0; 2]; FFT_SIZE],
            window,
            edges,
            position: 0,
            // One-sided FFT power, averaged across two independent channels.
            normalization: 1.0 / (FFT_SIZE as f32 * energy),
            levels: [0.0; BANDS],
            attack: 1.0 - (-(HOP as f32) / RATE / 0.04).exp(),
            release: 1.0 - (-(HOP as f32) / RATE / 0.22).exp(),
        }
    }

    fn reset(&mut self) {
        self.samples.fill([0.0; 2]);
        self.levels.fill(0.0);
        self.position = 0;
    }

    fn analyze(&mut self, frames: &[[f32; 2]]) -> [f32; BANDS] {
        for frame in frames {
            self.samples[self.position] = frame.map(|sample| {
                if sample.is_finite() {
                    sample.clamp(-1.0, 1.0)
                } else {
                    0.0
                }
            });
            self.position = (self.position + 1) % FFT_SIZE;
        }
        let mut power = [0.0; BANDS];
        for channel in 0..2 {
            let mean =
                self.samples.iter().map(|frame| frame[channel]).sum::<f32>() / FFT_SIZE as f32;
            for (i, sample) in self.buffer.iter_mut().enumerate() {
                *sample = Complex32::new(
                    (self.samples[(self.position + i) % FFT_SIZE][channel] - mean) * self.window[i],
                    0.0,
                );
            }
            self.fft
                .process_with_scratch(&mut self.buffer, &mut self.scratch);
            for (band, power) in power.iter_mut().enumerate() {
                *power += self.buffer[self.edges[band]..self.edges[band + 1]]
                    .iter()
                    .map(Complex32::norm_sqr)
                    .sum::<f32>()
                    * self.normalization;
            }
        }
        for (level, power) in self.levels.iter_mut().zip(power) {
            // Fixed -60 dBFS floor: silence never increases an automatic gain.
            let target = if power > 1.0e-6 {
                ((10.0 * power.log10() + 60.0) / 60.0).clamp(0.0, 1.0)
            } else {
                0.0
            };
            *level += (target - *level)
                * if target > *level {
                    self.attack
                } else {
                    self.release
                };
            if target == 0.0 && *level < 0.005 {
                *level = 0.0;
            }
        }
        self.levels
    }
}
