//! DLS display transport: OS serial paths, USB VID/PID serial selection, Unix
//! socket relays, or Windows named pipe relays. The handshake supplies screen
//! dimensions; subsequent output is an unframed stream of complete JPEGs.
//! Touch input is drained but not exposed to the UI.

use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use dida_core::{Frame, ScreenSize};
use image::{GenericImageView, ImageBuffer, Rgba, codecs::jpeg::JpegEncoder};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::watch,
    time::timeout,
};
use tokio_serial::{DataBits, FlowControl, Parity, SerialPort, SerialPortBuilderExt, StopBits};

pub const DRIVER_IDS: &[&str] = &["soeyi-dls"];
const SOEYI_USB: (u16, u16) = (0x33c3, 0xf101);
const MAGIC: [u8; 4] = [0xf0, 0xa5, 0x5a, 0x0f];
const HANDSHAKE_LIMIT: usize = 4096;
const IO_TIMEOUT: Duration = Duration::from_secs(5);

trait Connection: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Connection for T {}
type Stream = Box<dyn Connection>;

pub struct Display {
    stream: Stream,
    size: ScreenSize,
    identity: String,
    quality: u8,
    rotation: u16,
}

/// Open an endpoint, selecting a USB serial port when `endpoint` uses `usb:VID:PID`.
/// `forced_driver` bypasses the USB VID/PID driver map but must name a supported driver.
pub async fn connect(
    endpoint: &str,
    forced_driver: Option<&str>,
    device_index: usize,
    baud: u32,
    quality: u8,
) -> Result<Display> {
    ensure!(
        (1..=100).contains(&quality),
        "JPEG quality must be between 1 and 100"
    );
    let (endpoint, driver_id) = resolve_endpoint(endpoint, forced_driver, device_index)?;
    let stream = open_endpoint(&endpoint, baud).await?;
    negotiate(stream, quality)
        .await
        .with_context(|| format!("DLS handshake on {endpoint:?} using {driver_id:?} failed"))
}

fn resolve_endpoint(
    endpoint: &str,
    forced_driver: Option<&str>,
    device_index: usize,
) -> Result<(String, &'static str)> {
    if let Some((vid, pid)) = parse_usb_selector(endpoint)? {
        let ports = tokio_serial::available_ports().context("enumerating USB serial devices")?;
        let port = select_usb_port(ports, vid, pid, device_index)?;
        let driver_id = driver_for_usb(vid, pid, forced_driver)?;
        eprintln!(
            "Selected USB device {} for usb:{vid:04x}:{pid:04x} (index {device_index})",
            port.port_name
        );
        return Ok((port.port_name, driver_id));
    }

    ensure!(
        device_index == 0,
        "--device-index is only valid with --device usb:VID:PID"
    );
    let driver_id = validate_driver(forced_driver.unwrap_or("soeyi-dls"))?;
    Ok((endpoint.to_owned(), driver_id))
}

fn select_usb_port(
    ports: Vec<tokio_serial::SerialPortInfo>,
    vid: u16,
    pid: u16,
    device_index: usize,
) -> Result<tokio_serial::SerialPortInfo> {
    let mut matches = ports
        .into_iter()
        .filter(|port| {
            matches!(
                &port.port_type,
                tokio_serial::SerialPortType::UsbPort(info)
                    if info.vid == vid && info.pid == pid
            )
        })
        .collect::<Vec<_>>();
    matches.sort_by(|left, right| left.port_name.cmp(&right.port_name));
    matches.into_iter().nth(device_index).with_context(|| {
        format!("no USB serial device usb:{vid:04x}:{pid:04x} at index {device_index}")
    })
}

fn parse_usb_selector(endpoint: &str) -> Result<Option<(u16, u16)>> {
    let Some(value) = endpoint.strip_prefix("usb:") else {
        return Ok(None);
    };
    let (vid, pid) = value
        .split_once(':')
        .filter(|(_, rest)| !rest.contains(':'))
        .ok_or_else(|| {
            anyhow::anyhow!("USB device must use usb:VID:PID, for example usb:33c3:f101")
        })?;
    ensure!(
        vid.len() == 4 && pid.len() == 4,
        "USB VID and PID must each be four hexadecimal digits"
    );
    let vid = u16::from_str_radix(vid, 16).context("invalid USB vendor ID")?;
    let pid = u16::from_str_radix(pid, 16).context("invalid USB product ID")?;
    Ok(Some((vid, pid)))
}

fn validate_driver(driver_id: &str) -> Result<&'static str> {
    ensure!(
        DRIVER_IDS.contains(&driver_id),
        "unknown display driver {driver_id:?}; supported: {}",
        DRIVER_IDS.join(", ")
    );
    Ok(match driver_id {
        "soeyi-dls" => "soeyi-dls",
        _ => unreachable!(),
    })
}

fn driver_for_usb(vid: u16, pid: u16, forced_driver: Option<&str>) -> Result<&'static str> {
    if let Some(driver_id) = forced_driver {
        return validate_driver(driver_id);
    }
    ensure!(
        (vid, pid) == SOEYI_USB,
        "no driver is registered for usb:{vid:04x}:{pid:04x}; use --force-driver"
    );
    Ok("soeyi-dls")
}

#[derive(Debug, PartialEq, Eq)]
enum Endpoint<'a> {
    Serial(&'a str),
    Unix(&'a str),
    Pipe(&'a str),
}

fn endpoint_kind(endpoint: &str) -> Result<Endpoint<'_>> {
    ensure!(!endpoint.is_empty(), "display endpoint cannot be empty");
    if let Some(path) = endpoint.strip_prefix("serial:") {
        ensure!(!path.is_empty(), "serial endpoint needs an OS device path");
        return Ok(Endpoint::Serial(path));
    }
    if let Some(path) = endpoint.strip_prefix("unix:") {
        ensure!(
            !path.is_empty(),
            "Unix socket endpoint needs a filesystem path"
        );
        return Ok(Endpoint::Unix(path));
    }
    if let Some(path) = endpoint.strip_prefix("pipe:") {
        ensure!(
            path.starts_with(r"\\.\pipe\") && path.len() > 9,
            "named pipe endpoint must be pipe:\\\\.\\pipe\\name"
        );
        return Ok(Endpoint::Pipe(path));
    }
    if endpoint.starts_with(r"\\.\pipe\") {
        ensure!(endpoint.len() > 9, "named pipe endpoint needs a pipe name");
        return Ok(Endpoint::Pipe(endpoint));
    }
    let com = endpoint.strip_prefix(r"\\.\").unwrap_or(endpoint);
    if com.len() > 3
        && com
            .get(..3)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("COM"))
        && com.as_bytes()[3..].iter().all(|byte| byte.is_ascii_digit())
    {
        return Ok(Endpoint::Serial(endpoint));
    }
    if endpoint.starts_with("/dev/") {
        return Ok(Endpoint::Serial(endpoint));
    }
    ensure!(
        !endpoint.contains("://"),
        "unsupported endpoint {endpoint:?}; use serial:, unix:, or pipe:"
    );
    Ok(Endpoint::Unix(endpoint))
}

async fn open_endpoint(endpoint: &str, baud: u32) -> Result<Stream> {
    match endpoint_kind(endpoint)? {
        Endpoint::Serial(path) => {
            #[cfg(unix)]
            ensure!(
                !path.starts_with(r"\\.\")
                    && !path
                        .get(..3)
                        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("COM")),
                "Windows serial endpoint {path:?} is unsupported on this OS"
            );
            #[cfg(windows)]
            ensure!(
                !path.starts_with("/dev/"),
                "Unix serial endpoint {path:?} is unsupported on Windows"
            );
            ensure!(baud > 0, "serial baud rate must be positive");
            let mut serial = tokio_serial::new(path, baud)
                .data_bits(DataBits::Eight)
                .parity(Parity::None)
                .stop_bits(StopBits::One)
                .flow_control(FlowControl::None)
                .open_native_async()
                .with_context(|| format!("cannot open serial device {path:?}; check device path, permissions, and whether another process owns it"))?;
            serial.write_data_terminal_ready(true)
                .with_context(|| format!("cannot assert DTR on serial device {path:?}; DLS requires a real DTR-capable serial endpoint"))?;
            Ok(Box::new(serial))
        }
        Endpoint::Unix(path) => {
            #[cfg(unix)]
            {
                let stream = tokio::net::UnixStream::connect(path)
                    .await
                    .with_context(|| {
                        format!("cannot connect Unix socket {path:?}; check the relay is listening")
                    })?;
                Ok(Box::new(stream))
            }
            #[cfg(not(unix))]
            bail!("Unix socket endpoint {path:?} is unsupported on this OS")
        }
        Endpoint::Pipe(path) => {
            #[cfg(windows)]
            {
                let stream = tokio::net::windows::named_pipe::ClientOptions::new().open(path)
                    .with_context(|| format!("cannot open named pipe {path:?}; check the relay is listening and the pipe is available"))?;
                Ok(Box::new(stream))
            }
            #[cfg(not(windows))]
            bail!("Windows named pipe endpoint {path:?} is unsupported on this OS")
        }
    }
}

async fn negotiate(mut stream: Stream, quality: u8) -> Result<Display> {
    let (size, identity) = timeout(IO_TIMEOUT, async {
        stream.write_all(&MAGIC).await.context("cannot write DLS handshake magic")?;
        let mut reply = Vec::with_capacity(HANDSHAKE_LIMIT);
        let mut incoming = [0; 512];
        loop {
            let count = stream.read(&mut incoming).await.context("cannot read DLS handshake reply")?;
            ensure!(count != 0, "endpoint closed before a complete DLS handshake reply");
            ensure!(reply.len() + count <= HANDSHAKE_LIMIT, "DLS handshake exceeds {HANDSHAKE_LIMIT} bytes");
            reply.extend_from_slice(&incoming[..count]);
            if let Some(parsed) = parse_reply(&reply)? {
                return Ok(parsed);
            }
        }
    }).await.context("DLS handshake timed out after 5 seconds; check relay forwarding, serial baud rate, and DTR")??;
    let rotation = rotation_for_identity(&identity);
    Ok(Display {
        stream,
        size,
        identity,
        quality,
        rotation,
    })
}

fn parse_reply(reply: &[u8]) -> Result<Option<(ScreenSize, String)>> {
    ensure!(
        reply.len() <= HANDSHAKE_LIMIT,
        "DLS handshake exceeds {HANDSHAKE_LIMIT} bytes"
    );
    ensure!(reply.is_ascii(), "DLS handshake reply is not ASCII");
    let text = std::str::from_utf8(reply)?;
    let Some(star) = text.find('*') else {
        return Ok(None);
    };
    let Some(end) = text[star + 1..].find('S').map(|offset| star + 1 + offset) else {
        return Ok(None);
    };
    ensure!(
        star >= 4,
        "DLS reply needs four width digits immediately before '*'"
    );
    let width = &text[star - 4..star];
    let height = &text[star + 1..end];
    ensure!(
        width.bytes().all(|b| b.is_ascii_digit()),
        "DLS width is not four decimal digits"
    );
    ensure!(
        !height.is_empty() && height.bytes().all(|b| b.is_ascii_digit()),
        "DLS height before 'S' is not decimal digits"
    );
    let size = ScreenSize::new(width.parse()?, height.parse()?)
        .context("invalid dimensions in DLS handshake")?;
    // Touch messages are ASCII and self-delimiting through their four fields.
    let mut start = 0;
    while text[start..star - 4].starts_with("Touch") {
        let prefix = &text[start + 5..star - 4];
        ensure!(
            prefix.starts_with('D') || prefix.starts_with('U'),
            "invalid touch event before DLS identity"
        );
        let mut cursor = 1;
        for _ in 0..3 {
            ensure!(
                prefix.as_bytes().get(cursor) == Some(&b','),
                "incomplete touch message before DLS identity"
            );
            cursor += 1;
            let begin = cursor;
            while prefix
                .as_bytes()
                .get(cursor)
                .is_some_and(u8::is_ascii_digit)
            {
                cursor += 1;
            }
            ensure!(cursor > begin, "invalid touch field before DLS identity");
        }
        start += 5 + cursor;
    }
    let identity = text[start..=end].trim().to_owned();
    Ok(Some((size, identity)))
}

fn rotation_for_identity(identity: &str) -> u16 {
    if identity.contains("VMAXA") {
        180
    } else if identity.contains("VMAXB") {
        270
    } else if identity.contains("VMAXD") {
        90
    } else {
        0
    }
}

impl Display {
    pub fn size(&self) -> ScreenSize {
        self.size
    }

    pub fn logical_size(&self) -> ScreenSize {
        if matches!(self.rotation, 90 | 270) {
            ScreenSize {
                width: self.size.height,
                height: self.size.width,
            }
        } else {
            self.size
        }
    }

    /// The ASCII SN through the resolution's `S` delimiter, without leading touch messages.
    pub fn identity(&self) -> &str {
        &self.identity
    }

    pub fn set_rotation(&mut self, rotation: Option<u16>) -> Result<()> {
        if let Some(rotation) = rotation {
            ensure!(
                matches!(rotation, 0 | 90 | 180 | 270),
                "invalid DLS output rotation"
            );
            self.rotation = rotation;
        }
        if self.rotation != 0 {
            eprintln!("DLS output rotation: {}° clockwise", self.rotation);
        }
        Ok(())
    }

    /// Send complete raw JPEGs, coalescing pending frames and discarding incoming touch bytes.
    /// Shutdown finishes the current write (bounded by five seconds); EOF/read errors abort it.
    pub async fn run(
        self,
        frames: watch::Receiver<Option<Frame>>,
        shutdown: watch::Receiver<bool>,
    ) -> Result<()> {
        let (reader, writer) = tokio::io::split(self.stream);
        tokio::select! {
            result = drain(reader) => result,
            result = send_frames(writer, self.size, self.rotation, self.quality, frames, shutdown) => result,
        }
    }
}

async fn drain(mut reader: impl AsyncRead + Unpin) -> Result<()> {
    let mut incoming = [0; 1024];
    loop {
        let count = reader.read(&mut incoming).await.context(
            "DLS endpoint read failed; reconnect explicitly after checking the device or relay",
        )?;
        ensure!(
            count != 0,
            "DLS endpoint closed (EOF); reconnect explicitly after checking the device or relay"
        );
    }
}

async fn send_frames(
    mut writer: impl AsyncWrite + Unpin,
    size: ScreenSize,
    rotation: u16,
    quality: u8,
    mut frames: watch::Receiver<Option<Frame>>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let mut jpeg = Vec::new();
    loop {
        if *shutdown.borrow() {
            return Ok(());
        }
        let frame = frames.borrow_and_update().clone();
        if let Some(frame) = frame {
            ensure!(
                frame.size
                    == if matches!(rotation, 90 | 270) {
                        ScreenSize {
                            width: size.height,
                            height: size.width,
                        }
                    } else {
                        size
                    },
                "frame dimensions {}x{} do not match DLS screen {}x{} at {rotation}°",
                frame.size.width,
                frame.size.height,
                size.width,
                size.height
            );
            let sequence = frame.sequence;
            jpeg = tokio::task::spawn_blocking(move || encode_jpeg(frame, quality, rotation, jpeg))
                .await
                .context("DLS JPEG encoder worker failed")??;
            if *shutdown.borrow() {
                return Ok(());
            }
            timeout(IO_TIMEOUT, writer.write_all(&jpeg)).await
                .with_context(|| format!("DLS JPEG frame {sequence} write timed out after 5 seconds; endpoint closed to avoid a truncated stream"))?
                .with_context(|| format!("cannot write DLS JPEG frame {sequence}; check the device or relay"))?;
        }
        loop {
            tokio::select! {
                biased;
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() { return Ok(()) }
                }
                changed = frames.changed() => {
                    if changed.is_err() { return Ok(()) }
                    break;
                }
            }
        }
    }
}

struct NativeImage<'a> {
    image: ImageBuffer<Rgba<u8>, &'a [u8]>,
    rotation: u16,
}

impl GenericImageView for NativeImage<'_> {
    type Pixel = Rgba<u8>;

    fn dimensions(&self) -> (u32, u32) {
        let (width, height) = self.image.dimensions();
        if matches!(self.rotation, 90 | 270) {
            (height, width)
        } else {
            (width, height)
        }
    }

    fn get_pixel(&self, x: u32, y: u32) -> Self::Pixel {
        let (width, height) = self.image.dimensions();
        let (x, y) = match self.rotation {
            0 => (x, y),
            90 => (y, height - 1 - x),
            180 => (width - 1 - x, height - 1 - y),
            270 => (width - 1 - y, x),
            _ => unreachable!("validated DLS output rotation"),
        };
        *self.image.get_pixel(x, y)
    }
}

fn encode_jpeg(frame: Frame, quality: u8, rotation: u16, mut output: Vec<u8>) -> Result<Vec<u8>> {
    ensure!(
        matches!(rotation, 0 | 90 | 180 | 270),
        "invalid DLS output rotation {rotation}"
    );
    let image = ImageBuffer::<Rgba<u8>, &[u8]>::from_raw(
        frame.size.width,
        frame.size.height,
        frame.rgba.as_slice(),
    )
    .context("RGBA frame byte length does not match its dimensions")?;
    output.clear();
    JpegEncoder::new_with_quality(&mut output, quality)
        .encode_image(&NativeImage { image, rotation })
        .context("cannot encode DLS JPEG frame")?;
    Ok(output)
}

#[cfg(test)]
mod tests;
