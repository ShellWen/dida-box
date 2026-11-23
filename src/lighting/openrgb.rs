use std::{io::ErrorKind, net::SocketAddr, time::Duration};

use anyhow::{Context, Result, bail, ensure};
use dida_core::DisplayState;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::watch,
    time::{MissedTickBehavior, interval, sleep, timeout},
};

use super::audio::{BANDS, Spectrum};

const PROTOCOL: u32 = 6;
const IO_TIMEOUT: Duration = Duration::from_secs(2);
const RETRY_DELAY: Duration = Duration::from_secs(2);
const STALE_AUDIO: Duration = Duration::from_millis(500);
const MAX_PACKET: usize = 4 * 1024 * 1024;
const MAX_CONTROLLERS: u32 = 1024;
const MAX_UNSOLICITED: usize = 64;
const COUNT: u32 = 0;
const DESCRIPTION: u32 = 1;
const VERSION: u32 = 40;
const DEVICE_LIST_UPDATED: u32 = 100;
const UPDATE_LEDS: u32 = 1050;
const PER_LED_COLOR: u32 = 1 << 5;

pub(super) async fn run(
    address: SocketAddr,
    devices: Vec<String>,
    when_paused: bool,
    state: watch::Receiver<DisplayState>,
    mut spectrum: watch::Receiver<Spectrum>,
    mut shutdown: watch::Receiver<bool>,
    status: watch::Sender<String>,
) -> Result<()> {
    loop {
        report(&status, "connecting to OpenRGB SDK");
        tokio::select! {
            biased;
            _ = stopped(&mut shutdown) => return Ok(()),
            result = session(address, &devices, when_paused, &state, &mut spectrum, &status) => {
                if let Err(error) = result {
                    eprintln!("OpenRGB {address}: {error:#}; reconnecting in 2 seconds");
                    report(&status, &format!("OpenRGB SDK unavailable: {error:#}; retrying in 2 seconds"));
                }
            }
        }
        tokio::select! {
            biased;
            _ = stopped(&mut shutdown) => return Ok(()),
            _ = sleep(RETRY_DELAY) => {}
        }
    }
}

fn report(status: &watch::Sender<String>, message: &str) {
    status.send_if_modified(|previous| {
        if previous.as_str() == message {
            false
        } else {
            previous.clear();
            previous.push_str(message);
            true
        }
    });
}

async fn stopped(shutdown: &mut watch::Receiver<bool>) {
    loop {
        if *shutdown.borrow_and_update() {
            return;
        }
        if shutdown.changed().await.is_err() {
            return;
        }
    }
}

struct Client {
    stream: TcpStream,
    receive: Vec<u8>,
    send: Vec<u8>,
    dirty: bool,
}

impl Client {
    fn header(&mut self, device: u32, command: u32, size: usize) {
        self.send.clear();
        self.send.extend_from_slice(b"ORGB");
        self.send.extend_from_slice(&device.to_le_bytes());
        self.send.extend_from_slice(&command.to_le_bytes());
        self.send.extend_from_slice(&(size as u32).to_le_bytes());
    }

    async fn send(&mut self) -> Result<()> {
        timeout(IO_TIMEOUT, self.stream.write_all(&self.send))
            .await
            .context("SDK write timed out")??;
        Ok(())
    }

    async fn packet(&mut self, first: Option<u8>) -> Result<(u32, u32)> {
        let mut header = [0u8; 16];
        if let Some(byte) = first {
            header[0] = byte;
            self.stream.read_exact(&mut header[1..]).await?;
        } else {
            self.stream.read_exact(&mut header).await?;
        }
        ensure!(&header[..4] == b"ORGB", "invalid SDK packet magic");
        let mut fields = Cursor(&header[4..]);
        let device = fields.u32()?;
        let command = fields.u32()?;
        let size = fields.u32()? as usize;
        ensure!(size <= MAX_PACKET, "SDK packet exceeds 4 MiB");
        self.receive.resize(size, 0);
        self.stream.read_exact(&mut self.receive).await?;
        if command == DEVICE_LIST_UPDATED {
            self.dirty = true;
        }
        Ok((device, command))
    }

    async fn request(&mut self, device: u32, command: u32, body: &[u8]) -> Result<()> {
        self.header(device, command, body.len());
        self.send.extend_from_slice(body);
        self.send().await?;
        timeout(IO_TIMEOUT, async {
            for _ in 0..MAX_UNSOLICITED {
                if self.packet(None).await? == (device, command) {
                    return Ok(());
                }
            }
            bail!("too many unsolicited SDK packets")
        })
        .await
        .context("SDK response timed out")?
    }

    async fn drain(&mut self) -> Result<()> {
        timeout(IO_TIMEOUT, async {
            for _ in 0..MAX_UNSOLICITED {
                let mut first = [0];
                match self.stream.try_read(&mut first) {
                    Ok(0) => bail!("SDK connection closed"),
                    Ok(_) => {
                        self.packet(Some(first[0])).await?;
                    }
                    Err(error) if error.kind() == ErrorKind::WouldBlock => return Ok(()),
                    Err(error) => return Err(error.into()),
                }
            }
            bail!("too many unsolicited SDK packets")
        })
        .await
        .context("SDK notification timed out")?
    }

    async fn controllers(&mut self, protocol: u32, ids: &mut Vec<u32>) -> Result<()> {
        self.request(0, COUNT, &[]).await?;
        let mut data = Cursor(&self.receive);
        let count = data.u32()?;
        ensure!(count <= MAX_CONTROLLERS, "too many SDK controllers");
        ids.clear();
        for index in 0..count {
            let id = if protocol >= 6 { data.u32()? } else { index };
            ensure!(!ids.contains(&id), "duplicate SDK controller ID");
            ids.push(id);
        }
        ensure!(data.0.is_empty(), "invalid SDK controller count");
        Ok(())
    }
}

async fn session(
    address: SocketAddr,
    devices: &[String],
    when_paused: bool,
    state: &watch::Receiver<DisplayState>,
    spectrum: &mut watch::Receiver<Spectrum>,
    status: &watch::Sender<String>,
) -> Result<()> {
    let stream = timeout(IO_TIMEOUT, TcpStream::connect(address))
        .await
        .context("SDK connect timed out")??;
    stream.set_nodelay(true)?;
    let mut client = Client {
        stream,
        receive: Vec::new(),
        send: Vec::new(),
        dirty: true,
    };
    client.request(0, VERSION, &PROTOCOL.to_le_bytes()).await?;
    ensure!(client.receive.len() == 4, "invalid SDK protocol response");
    let protocol = Cursor(&client.receive).u32()?.min(PROTOCOL);
    ensure!(protocol >= 3, "SDK protocol 3 or newer required");
    report(status, "OpenRGB SDK connected; discovering eligible devices");

    let mut ticker = interval(Duration::from_secs_f64(1.0 / 30.0));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut ids = Vec::new();
    let mut zones = Vec::new();
    // An old watch value must not animate after a reconnect until audio publishes again.
    spectrum.borrow_and_update();
    let mut audio = Spectrum::default();
    loop {
        ticker.tick().await;
        client.drain().await?;
        client.dirty = false;
        client.controllers(protocol, &mut ids).await?;
        if client.dirty {
            continue;
        }
        let mut selected = false;
        let mut output_status = None;
        for &device in &ids {
            client
                .request(device, DESCRIPTION, &protocol.to_le_bytes())
                .await?;
            if client.dirty {
                break;
            }
            let controller = parse_description(&client.receive, protocol, devices, &mut zones)?;
            selected |= controller.selected;
            if !controller.direct || !controller.selected || controller.leds == 0 {
                continue;
            }
            if spectrum.has_changed().unwrap_or(false) {
                audio = *spectrum.borrow_and_update();
            }
            let (active, accent) = {
                let state = state.borrow();
                (state.active, state.content.accent)
            };
            let audio_ready = spectrum.has_changed().is_ok()
                && audio.captured_at.is_some_and(|at| at.elapsed() < STALE_AUDIO);
            let enabled = active || when_paused;
            let levels = if enabled && audio_ready {
                &audio.levels
            } else {
                &[0.0; BANDS]
            };
            build_colors(
                &mut client,
                device,
                controller.leds,
                &zones,
                accent,
                levels,
            );
            // The SDK has no atomic mode-check/update: another client can race this gate.
            client.drain().await?;
            if client.dirty {
                break;
            }
            client.send().await?;
            output_status = Some(match (enabled, audio_ready) {
                (false, true) => "OpenRGB SDK connected; paused / waiting for playback",
                (false, false) => "OpenRGB SDK connected; paused / waiting for playback; audio capture unavailable or stale",
                (true, false) => "OpenRGB SDK connected; audio capture unavailable or stale; output zeroed",
                (true, true) => "OpenRGB SDK connected; active audio-reactive output",
            });
        }
        if !client.dirty {
            report(status, output_status.unwrap_or(if selected {
                "OpenRGB SDK connected; no selected device eligible in Direct mode"
            } else {
                "OpenRGB SDK connected; no selected devices found"
            }));
        }
    }
}

struct Controller {
    selected: bool,
    direct: bool,
    leds: u16,
}

struct Zone {
    linear: bool,
    leds: usize,
}

struct Cursor<'a>(&'a [u8]);

impl<'a> Cursor<'a> {
    fn take(&mut self, size: usize) -> Result<&'a [u8]> {
        ensure!(size <= self.0.len(), "truncated SDK descriptor");
        let (bytes, rest) = self.0.split_at(size);
        self.0 = rest;
        Ok(bytes)
    }

    fn u16(&mut self) -> Result<u16> {
        let bytes = self.take(2)?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    fn u32(&mut self) -> Result<u32> {
        let bytes = self.take(4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn string(&mut self) -> Result<&'a [u8]> {
        let len = self.u16()? as usize;
        let bytes = self.take(len)?;
        ensure!(bytes.last() == Some(&0), "SDK string lacks terminator");
        Ok(&bytes[..len - 1])
    }
}

fn parse_description(
    bytes: &[u8],
    protocol: u32,
    devices: &[String],
    zones: &mut Vec<Zone>,
) -> Result<Controller> {
    let mut data = Cursor(bytes);
    ensure!(
        data.u32()? as usize == bytes.len(),
        "invalid SDK descriptor size"
    );
    data.u32()?;
    let name = data.string()?;
    let selected = devices.is_empty() || devices.iter().any(|device| device.as_bytes() == name);
    for _ in 0..5 {
        data.string()?;
    }
    let modes = data.u16()?;
    let active = data.u32()?;
    let mut direct = parse_modes(&mut data, protocol, modes, active)?;
    zones.clear();
    let zone_count = data.u16()?;
    let mut zone_leds = 0usize;
    for _ in 0..zone_count {
        data.string()?;
        let kind = data.u32()?;
        data.take(8)?;
        let leds = data.u32()? as usize;
        zone_leds = zone_leds
            .checked_add(leds)
            .context("SDK zone size overflow")?;
        ensure!(
            zone_leds <= u16::MAX as usize,
            "SDK zones exceed LED count limit"
        );
        parse_matrix(&mut data)?;
        if protocol >= 4 {
            let segments = data.u16()?;
            for _ in 0..segments {
                data.string()?;
                data.take(12)?;
                if protocol >= 6 {
                    parse_matrix(&mut data)?;
                    data.u32()?; // Segment flags are v6 in the serializer, despite SDK table v5.
                }
            }
        }
        if protocol >= 5 {
            data.u32()?;
        }
        if protocol >= 6 {
            let active = data.u32()?;
            let modes = data.u16()?;
            let zone_direct = parse_modes(&mut data, protocol, modes, active)?;
            // A signed -1 means the zone inherits the already-checked controller mode.
            if leds != 0 && active != u32::MAX && !zone_direct {
                direct = false;
            }
            data.string()?;
        }
        zones.push(Zone {
            linear: kind == 1 || kind == 3,
            leds,
        });
    }
    let leds = data.u16()?;
    for _ in 0..leds {
        data.string()?;
        if protocol < 6 {
            data.u32()?;
        }
    }
    let colors = data.u16()?;
    data.take(usize::from(colors) * 4)?;
    if protocol >= 5 {
        let names = data.u16()?;
        for _ in 0..names {
            data.string()?;
        }
        data.u32()?;
    }
    if protocol >= 6 {
        data.string()?;
        let len = data.u32()? as usize;
        data.take(len)?;
    }
    ensure!(data.0.is_empty(), "unexpected SDK descriptor fields");
    ensure!(
        colors == leds && zone_leds <= usize::from(leds),
        "inconsistent SDK LED counts"
    );
    Ok(Controller {
        selected,
        direct,
        leds,
    })
}

fn parse_modes(data: &mut Cursor<'_>, protocol: u32, count: u16, active: u32) -> Result<bool> {
    let mut direct = false;
    for index in 0..u32::from(count) {
        let name = data.string()?;
        if protocol < 6 {
            data.u32()?;
        } // Mode value is not the active-mode index.
        let flags = data.u32()?;
        data.take(10 * 4)?; // Includes brightness min, max, and current since protocol 3.
        let colors = data.u16()? as usize;
        data.take(colors * 4)?;
        if index == active {
            direct = name.eq_ignore_ascii_case(b"Direct") && flags & PER_LED_COLOR != 0;
        }
    }
    Ok(direct)
}

fn parse_matrix(data: &mut Cursor<'_>) -> Result<()> {
    let len = data.u16()? as usize;
    let mut matrix = Cursor(data.take(len)?);
    if len != 0 {
        let height = matrix.u32()? as usize;
        let width = matrix.u32()? as usize;
        let cells = height
            .checked_mul(width)
            .and_then(|n| n.checked_mul(4))
            .context("SDK matrix size overflow")?;
        ensure!(cells == matrix.0.len(), "invalid SDK matrix length");
    }
    Ok(())
}

fn build_colors(
    client: &mut Client,
    device: u32,
    leds: u16,
    zones: &[Zone],
    accent: u32,
    levels: &[f32; BANDS],
) {
    let size = 6 + usize::from(leds) * 4;
    client.header(device, UPDATE_LEDS, size);
    client.send.extend_from_slice(&(size as u32).to_le_bytes());
    client.send.extend_from_slice(&leds.to_le_bytes());
    let amplitude = |value: f32| {
        if value.is_finite() {
            value.clamp(0.0, 1.0)
        } else {
            0.0
        }
    };
    let energy = (levels.iter().map(|&v| amplitude(v).powi(2)).sum::<f32>() / BANDS as f32).sqrt();
    let rgb = [(accent >> 16) as u8, (accent >> 8) as u8, accent as u8];
    let mut push = |level: f32| {
        client.send.extend_from_slice(&[
            (f32::from(rgb[0]) * level).round() as u8,
            (f32::from(rgb[1]) * level).round() as u8,
            (f32::from(rgb[2]) * level).round() as u8,
            0,
        ]);
    };
    let mut written = 0;
    for zone in zones {
        for led in 0..zone.leds {
            let level = if zone.linear && zone.leds > 1 {
                let position = led as f32 * (BANDS - 1) as f32 / (zone.leds - 1) as f32;
                let lower = position.floor() as usize;
                let upper = (lower + 1).min(BANDS - 1);
                let fraction = position - lower as f32;
                amplitude(levels[lower] * (1.0 - fraction) + levels[upper] * fraction)
            } else {
                energy
            };
            push(level);
        }
        written += zone.leds;
    }
    for _ in written..usize::from(leds) {
        push(energy);
    }
}
