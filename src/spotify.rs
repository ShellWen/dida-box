use std::{
    borrow::Cow,
    net::{IpAddr, SocketAddr},
    sync::{Arc, LazyLock},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use dida_core::{DisplayCommand, DisplayContent, DisplayState, TimedText};
use futures_util::{SinkExt, StreamExt};
use image::GenericImageView;
use serde::{Deserialize, Serialize};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{mpsc, watch},
    time::{Instant as Deadline, sleep_until, timeout},
};
use tokio_tungstenite::{
    WebSocketStream, accept_hdr_async_with_config,
    tungstenite::{
        Message,
        handshake::server::{Callback, ErrorResponse, Request, Response},
        http::{StatusCode, header, uri::Authority},
        protocol::WebSocketConfig,
    },
};

const MAX_MESSAGE: usize = 256 * 1024;
const MAX_LINES: usize = 2_000;
const MAX_LINE_TEXT: usize = 4_096;
const MAX_LYRIC_TEXT: usize = 128 * 1024;
const MAX_ART_DATA: usize = 128 * 1024;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(3);
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);
const PING_INTERVAL: Duration = Duration::from_secs(3);
const PING: &str = r#"{"type":"ping"}"#;
const FALLBACK_ACCENT: u32 = 0x909098;

fn artwork_accent(image: &image::DynamicImage) -> u32 {
    let (width, height) = image.dimensions();
    let mut bins = [(0u32, [0u32; 3]); 512];
    for y in (0..height).step_by((height / 32).max(1) as usize) {
        for x in (0..width).step_by((width / 32).max(1) as usize) {
            let pixel = image.get_pixel(x, y).0;
            let max = pixel[..3].iter().copied().max().unwrap();
            let min = pixel[..3].iter().copied().min().unwrap();
            if max < 24 || max - min < 24 {
                continue;
            }
            let bin = ((pixel[0] as usize >> 5) << 6)
                | ((pixel[1] as usize >> 5) << 3)
                | (pixel[2] as usize >> 5);
            let (count, sums) = &mut bins[bin];
            *count += 1;
            for (sum, channel) in sums.iter_mut().zip(pixel) {
                *sum += u32::from(channel);
            }
        }
    }
    let (count, sums) = bins.into_iter().max_by_key(|(count, _)| *count).unwrap();
    if count == 0 {
        return FALLBACK_ACCENT;
    }
    let rgb = sums.map(|sum| sum / count);
    let brightness = rgb.into_iter().max().unwrap();
    let scale = 160u32.max(brightness);
    let channels = rgb.map(|channel| (channel * scale / brightness).min(255));
    (channels[0] << 16) | (channels[1] << 8) | channels[2]
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Incoming<'a> {
    State {
        version: u8,
        #[serde(borrow, deserialize_with = "required_option")]
        track: Option<Track<'a>>,
        position_ms: u64,
        playing: bool,
    },
    Lyrics {
        version: u8,
        #[serde(borrow)]
        track_id: Cow<'a, str>,
        status: LyricsStatus,
        #[serde(borrow)]
        lines: Vec<LyricLine<'a>>,
    },
    AlbumArt {
        version: u8,
        #[serde(borrow)]
        track_id: Cow<'a, str>,
        #[serde(borrow)]
        data: Cow<'a, str>,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Track<'a> {
    #[serde(borrow)]
    id: Cow<'a, str>,
    #[serde(borrow)]
    title: Cow<'a, str>,
    #[serde(borrow)]
    artist: Cow<'a, str>,
    duration_ms: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum LyricsStatus {
    Synced,
    Unsynced,
    Unavailable,
    Error,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LyricLine<'a> {
    start_ms: Option<u64>,
    #[serde(borrow)]
    text: Cow<'a, str>,
}

#[derive(Serialize)]
struct Outgoing<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    #[serde(flatten)]
    command: PlaybackCommand,
    track_id: &'a str,
}

#[derive(Serialize)]
#[serde(tag = "command", rename_all = "snake_case")]
enum PlaybackCommand {
    ToggleActive,
    Next,
    Seek { milliseconds: u64 },
    SeekBy { milliseconds: i64 },
}

#[derive(PartialEq, Eq, Debug)]
enum Update {
    State,
    Lyrics,
    AlbumArt,
    Ignored,
}

fn waiting_at(now: Instant) -> DisplayState {
    static CONTENT: LazyLock<Arc<DisplayContent>> = LazyLock::new(|| {
        Arc::new(DisplayContent {
            id: "spotify-waiting".into(),
            title: "Spotify".into(),
            subtitle: "".into(),
            body: "Waiting for Spotify".into(),
            duration: Duration::ZERO,
            timeline: Arc::from([]),
            album_art: None,
            accent: FALLBACK_ACCENT,
        })
    });
    DisplayState {
        content: CONTENT.clone(),
        position: Duration::ZERO,
        active: false,
        observed_at: now,
    }
}

pub fn initial() -> DisplayState {
    waiting_at(Instant::now())
}

pub async fn bind(address: SocketAddr) -> Result<TcpListener> {
    ensure!(
        address.ip().is_loopback(),
        "Spotify bridge must bind to loopback"
    );
    TcpListener::bind(address)
        .await
        .context("binding Spotify bridge")
}

struct Handshake {
    port: u16,
}

impl Callback for Handshake {
    fn on_request(self, request: &Request, response: Response) -> Result<Response, ErrorResponse> {
        let valid_path = request.uri().path() == "/spotify"
            && request.uri().query().is_none()
            && request.uri().scheme().is_none()
            && request.uri().authority().is_none();
        let valid_origin = request.headers().get_all(header::ORIGIN).iter().count() <= 1
            && request
                .headers()
                .get(header::ORIGIN)
                .is_none_or(|value| value.as_bytes() == b"https://xpui.app.spotify.com");
        let valid_host = request.headers().get_all(header::HOST).iter().count() == 1
            && request
                .headers()
                .get(header::HOST)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<Authority>().ok())
                .is_some_and(|authority| {
                    let host = authority.host();
                    let host = host
                        .strip_prefix('[')
                        .and_then(|host| host.strip_suffix(']'))
                        .unwrap_or(host);
                    !authority.as_str().contains('@')
                        && (host.eq_ignore_ascii_case("localhost")
                            || host
                                .parse::<IpAddr>()
                                .is_ok_and(|address| address.is_loopback()))
                        && (authority.port().is_none() || authority.port_u16() == Some(self.port))
                });
        if valid_path && valid_origin && valid_host {
            Ok(response)
        } else {
            let mut response = ErrorResponse::new(Some("Spotify bridge request rejected".into()));
            *response.status_mut() = StatusCode::FORBIDDEN;
            Err(response)
        }
    }
}

fn required_option<'de, D, T>(deserializer: D) -> std::result::Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

fn valid_id(id: &str) -> bool {
    id.len() <= 512
        && id.strip_prefix("spotify:").is_some_and(|path| {
            path.split_once(':')
                .is_some_and(|(kind, id)| !kind.is_empty() && !id.is_empty())
                && !path
                    .chars()
                    .any(|character| character.is_whitespace() || character.is_control())
        })
}

fn reuse_text(previous: &Arc<str>, text: &str) -> Arc<str> {
    if previous.as_ref() == text {
        previous.clone()
    } else {
        text.into()
    }
}

fn validate_lyrics(status: &LyricsStatus, lines: &[LyricLine<'_>]) -> Result<usize> {
    ensure!(lines.len() <= MAX_LINES, "too many lyric lines");
    let mut text_length = lines.len().saturating_sub(1);
    let mut previous = 0;
    for line in lines {
        ensure!(line.text.len() <= MAX_LINE_TEXT, "lyric line too long");
        text_length += line.text.len();
        ensure!(text_length <= MAX_LYRIC_TEXT, "lyric text too long");
        match status {
            LyricsStatus::Synced => {
                let start = line.start_ms.context("synced lyric lacks timing")?;
                ensure!(start >= previous, "lyric timing is not sorted");
                previous = start;
            }
            LyricsStatus::Unsynced => {
                ensure!(line.start_ms.is_none(), "unsynced lyric has timing");
            }
            LyricsStatus::Unavailable | LyricsStatus::Error => {
                anyhow::bail!("unavailable lyrics must not contain lines");
            }
        }
    }
    Ok(text_length)
}

fn apply(state: &mut DisplayState, message: Incoming<'_>, now: Instant) -> Result<Update> {
    match message {
        Incoming::State {
            version,
            track,
            position_ms,
            playing,
        } => {
            ensure!(version == 1, "unsupported Spotify bridge version");
            let Some(track) = track else {
                return Ok(Update::Ignored);
            };
            ensure!(valid_id(&track.id), "invalid Spotify URI");
            ensure!(track.title.len() <= MAX_LINE_TEXT, "track title too long");
            ensure!(track.artist.len() <= MAX_LINE_TEXT, "track artist too long");
            let old = &state.content;
            let same_id = old.id.as_ref() == track.id.as_ref();
            let duration = Duration::from_millis(track.duration_ms);
            if !same_id
                || old.title.as_ref() != track.title.as_ref()
                || old.subtitle.as_ref() != track.artist.as_ref()
                || old.duration != duration
            {
                state.content = Arc::new(DisplayContent {
                    id: reuse_text(&old.id, &track.id),
                    title: reuse_text(&old.title, &track.title),
                    subtitle: reuse_text(&old.subtitle, &track.artist),
                    body: if same_id {
                        old.body.clone()
                    } else {
                        "Loading lyrics...".into()
                    },
                    duration,
                    timeline: if same_id {
                        old.timeline.clone()
                    } else {
                        Arc::from([])
                    },
                    album_art: if same_id { old.album_art.clone() } else { None },
                    accent: if same_id { old.accent } else { FALLBACK_ACCENT },
                });
            }
            state.position = Duration::from_millis(position_ms).min(duration);
            state.active = playing;
            state.observed_at = now;
            Ok(Update::State)
        }
        Incoming::Lyrics {
            version,
            track_id,
            status,
            lines,
        } => {
            ensure!(version == 1, "unsupported Spotify bridge version");
            ensure!(valid_id(&track_id), "invalid lyrics Spotify URI");
            let text_length = validate_lyrics(&status, &lines)?;
            if state.content.id.as_ref() != track_id.as_ref() {
                return Ok(Update::Ignored);
            }
            // The extension fences fetch generations, including A -> B -> A transitions.
            let (body, timeline): (Arc<str>, Arc<[TimedText]>) = match status {
                LyricsStatus::Synced if !lines.is_empty() => (
                    "".into(),
                    lines
                        .into_iter()
                        .map(|line| TimedText {
                            start: Duration::from_millis(line.start_ms.unwrap()),
                            text: Arc::from(line.text.as_ref()),
                        })
                        .collect::<Vec<_>>()
                        .into(),
                ),
                LyricsStatus::Unsynced if !lines.is_empty() => {
                    let mut body = String::with_capacity(text_length);
                    for (index, line) in lines.into_iter().enumerate() {
                        if index != 0 {
                            body.push('\n');
                        }
                        body.push_str(&line.text);
                    }
                    (body.into(), Arc::from([]))
                }
                LyricsStatus::Error => ("Unable to load lyrics".into(), Arc::from([])),
                _ => ("Lyrics unavailable".into(), Arc::from([])),
            };
            state.content = Arc::new(DisplayContent {
                body,
                timeline,
                ..(*state.content).clone()
            });
            Ok(Update::Lyrics)
        }
        Incoming::AlbumArt {
            version,
            track_id,
            data,
        } => {
            ensure!(version == 1, "unsupported Spotify bridge version");
            ensure!(valid_id(&track_id), "invalid album art Spotify URI");
            ensure!(
                data.len() <= MAX_ART_DATA * 2,
                "album art payload too large"
            );
            if state.content.id.as_ref() != track_id.as_ref() {
                return Ok(Update::Ignored);
            }
            let bytes = data
                .strip_prefix("data:image/jpeg;base64,")
                .context("album art must be a JPEG data URL")?;
            use base64::Engine as _;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(bytes)
                .context("decoding album art")?;
            ensure!(bytes.len() <= MAX_ART_DATA, "album art payload too large");
            let decoder = image::codecs::jpeg::JpegDecoder::new(std::io::Cursor::new(&bytes))
                .context("decoding album art JPEG")?;
            use image::ImageDecoder as _;
            let (width, height) = decoder.dimensions();
            ensure!(
                width > 0 && height > 0 && width <= 640 && height <= 640,
                "album art dimensions exceed 640x640"
            );
            let image =
                image::DynamicImage::from_decoder(decoder).context("decoding album art pixels")?;
            state.content = Arc::new(DisplayContent {
                album_art: Some(bytes.into()),
                accent: artwork_accent(&image),
                ..(*state.content).clone()
            });
            Ok(Update::AlbumArt)
        }
    }
}

async fn stopped(shutdown: &mut watch::Receiver<bool>) {
    loop {
        let requested = *shutdown.borrow_and_update();
        if requested {
            return;
        }
        if shutdown.changed().await.is_err() {
            return;
        }
    }
}

async fn write_control(
    socket: &mut WebSocketStream<TcpStream>,
    message: Message,
    sender: &watch::Sender<DisplayState>,
    shutdown: &mut watch::Receiver<bool>,
) -> bool {
    tokio::select! {
        biased;
        _ = stopped(shutdown) => false,
        _ = sender.closed() => false,
        result = timeout(WRITE_TIMEOUT, socket.send(message)) => matches!(result, Ok(Ok(()))),
    }
}

async fn session(
    mut socket: WebSocketStream<TcpStream>,
    listener: &TcpListener,
    sender: &watch::Sender<DisplayState>,
    commands: &mut mpsc::Receiver<DisplayCommand>,
    commands_open: &mut bool,
    shutdown: &mut watch::Receiver<bool>,
) {
    // Disconnected commands cannot be replayed against a newly connected track.
    for _ in 0..commands.len() {
        let _ = commands.try_recv();
    }
    let mut state = initial();
    let mut next_ping = Deadline::now() + PING_INTERVAL;
    loop {
        tokio::select! {
            _ = stopped(shutdown) => break,
            _ = sleep_until(next_ping) => {
                if !write_control(&mut socket, Message::text(PING), sender, shutdown).await {
                    break;
                }
                next_ping = Deadline::now() + PING_INTERVAL;
            }
            _ = sender.closed() => break,
            accepted = listener.accept() => {
                if accepted.is_err() { break; }
                // Drop additional clients while the current connection is active.
            }
            command = commands.recv(), if *commands_open => {
                let Some(command) = command else {
                    *commands_open = false;
                    continue;
                };
                if !valid_id(&state.content.id) { continue; }
                let command = match command {
                    DisplayCommand::ToggleActive => PlaybackCommand::ToggleActive,
                    DisplayCommand::Next => PlaybackCommand::Next,
                    DisplayCommand::Seek(position) => PlaybackCommand::Seek {
                        milliseconds: position.min(state.content.duration).as_millis() as u64,
                    },
                    DisplayCommand::SeekBy(milliseconds) => PlaybackCommand::SeekBy { milliseconds },
                };
                let message = Outgoing {
                    kind: "command",
                    command,
                    track_id: &state.content.id,
                };
                let Ok(text) = serde_json::to_string(&message) else { break; };
                if !write_control(&mut socket, Message::text(text), sender, shutdown).await {
                    break;
                }
            }
            incoming = socket.next() => {
                match incoming {
                    Some(Ok(Message::Text(text))) => {
                        let update = serde_json::from_str::<Incoming<'_>>(text.as_str())
                            .map_err(anyhow::Error::from)
                            .and_then(|message| apply(&mut state, message, Instant::now()));
                        let Ok(update) = update else { break; };
                        if update != Update::Ignored && sender.send(state.clone()).is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Ping(payload))) => {
                        if !write_control(&mut socket, Message::Pong(payload), sender, shutdown).await {
                            break;
                        }
                    }
                    Some(Ok(Message::Pong(_))) => {}
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    Some(Ok(_)) => break,
                }
            }
        }
    }
    sender.send_replace(initial());
    let _ = timeout(WRITE_TIMEOUT, socket.close(None)).await;
}

pub async fn run(
    listener: TcpListener,
    sender: watch::Sender<DisplayState>,
    mut commands: mpsc::Receiver<DisplayCommand>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let address = listener
        .local_addr()
        .context("reading Spotify bridge address")?;
    ensure!(
        address.ip().is_loopback(),
        "Spotify bridge listener must be loopback"
    );
    if *shutdown.borrow() || sender.is_closed() {
        return Ok(());
    }
    sender.send_replace(initial());
    let mut commands_open = true;
    loop {
        let (stream, peer) = tokio::select! {
            _ = stopped(&mut shutdown) => break,
            _ = sender.closed() => break,
            command = commands.recv(), if commands_open => {
                if command.is_none() { commands_open = false; }
                continue;
            }
            accepted = listener.accept() => accepted.context("accepting Spotify bridge connection")?,
        };
        if !peer.ip().is_loopback() {
            continue;
        }
        let config = WebSocketConfig::default()
            .read_buffer_size(16 * 1024)
            .write_buffer_size(0)
            .max_write_buffer_size(MAX_MESSAGE)
            .max_message_size(Some(MAX_MESSAGE))
            .max_frame_size(Some(MAX_MESSAGE));
        let handshake = accept_hdr_async_with_config(
            stream,
            Handshake {
                port: address.port(),
            },
            Some(config),
        );
        let socket = tokio::select! {
            _ = stopped(&mut shutdown) => break,
            _ = sender.closed() => break,
            result = timeout(HANDSHAKE_TIMEOUT, handshake) => {
                let Ok(Ok(socket)) = result else { continue; };
                socket
            }
        };
        session(
            socket,
            &listener,
            &sender,
            &mut commands,
            &mut commands_open,
            &mut shutdown,
        )
        .await;
    }
    sender.send_replace(initial());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artwork_palette_ignores_neutrals_and_brightens_the_dominant_color() {
        let artwork = image::RgbImage::from_fn(64, 64, |x, y| {
            image::Rgb(if y < 32 {
                [240, 240, 240]
            } else if x < 48 {
                [70, 20, 10]
            } else {
                [20, 40, 220]
            })
        });
        let accent = artwork_accent(&artwork.into());
        assert!(accent >> 16 >= 150, "{accent:06x}");
        assert!((accent >> 8) & 0xff < 70, "{accent:06x}");
        assert!(accent & 0xff < 50, "{accent:06x}");
        let grayscale =
            image::RgbImage::from_fn(32, 32, |x, _| image::Rgb([if x < 16 { 8 } else { 240 }; 3]));
        assert_eq!(artwork_accent(&grayscale.into()), FALLBACK_ACCENT);
    }

    #[test]
    fn artwork_stays_with_its_track_and_rejects_oversized_images() {
        use base64::{Engine as _, engine::general_purpose::STANDARD};
        let now = Instant::now();
        let mut state = initial();
        let track = r#"{"type":"state","version":1,"track":{"id":"spotify:track:a","title":"A","artist":"Artist","duration_ms":20000},"position_ms":500,"playing":true}"#;
        apply(&mut state, serde_json::from_str(track).unwrap(), now).unwrap();
        let artwork = |edge, color: [u8; 3]| {
            let mut jpeg = Vec::new();
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 100)
                .encode(
                    &color.repeat(edge as usize * edge as usize),
                    edge,
                    edge,
                    image::ExtendedColorType::Rgb8,
                )
                .unwrap();
            format!("data:image/jpeg;base64,{}", STANDARD.encode(jpeg))
        };
        let data = artwork(16, [190, 45, 35]);
        let message = |id: &str, data: &str| Incoming::AlbumArt {
            version: 1,
            track_id: Cow::Owned(id.to_owned()),
            data: Cow::Owned(data.to_owned()),
        };
        apply(&mut state, message("spotify:track:a", &data), now).unwrap();
        let cover = state.content.album_art.clone().unwrap();
        let red = (state.content.accent >> 16) & 0xff;
        let green = (state.content.accent >> 8) & 0xff;
        assert!(red >= 170 && green < 80, "{:06x}", state.content.accent);
        assert_eq!(image::load_from_memory(&cover).unwrap().width(), 16);
        apply(&mut state, serde_json::from_str(track).unwrap(), now).unwrap();
        assert!(Arc::ptr_eq(
            &cover,
            state.content.album_art.as_ref().unwrap()
        ));
        assert!(state.content.accent >> 16 >= 170);
        assert_eq!(
            state.position_at(now + Duration::from_secs(1)),
            Duration::from_millis(1500)
        );
        assert!(
            apply(
                &mut state,
                message("spotify:track:a", &artwork(641, [255, 255, 255])),
                now
            )
            .is_err()
        );
        assert!(
            apply(
                &mut state,
                message("spotify:track:a", "data:image/jpeg;base64,YmFk"),
                now
            )
            .is_err()
        );
        let next = track.replace("spotify:track:a", "spotify:track:b");
        apply(&mut state, serde_json::from_str(&next).unwrap(), now).unwrap();
        assert_eq!(state.content.accent, FALLBACK_ACCENT);
        assert_eq!(
            apply(&mut state, message("spotify:track:a", &data), now).unwrap(),
            Update::Ignored
        );
        assert_eq!(state.content.accent, FALLBACK_ACCENT);
    }

    #[test]
    fn transitions_preserve_clock_and_drop_previous_track_lyrics() {
        fn feed(state: &mut DisplayState, json: &str, now: Instant) -> Update {
            apply(state, serde_json::from_str(json).unwrap(), now).unwrap()
        }
        let now = Instant::now();
        let mut state = waiting_at(now);
        let a = r#"{"type":"state","version":1,"track":{"id":"spotify:track:a","title":"A","artist":"Artist","duration_ms":20000},"position_ms":500,"playing":true}"#;
        feed(&mut state, a, now);
        let content = state.content.clone();
        feed(&mut state, a, now);
        assert!(Arc::ptr_eq(&content, &state.content));
        assert!(
            serde_json::from_str::<Incoming<'_>>(
                r#"{"type":"state","version":1,"position_ms":0,"playing":false}"#
            )
            .is_err()
        );
        let lyrics_a = r#"{"type":"lyrics","version":1,"track_id":"spotify:track:a","status":"synced","lines":[{"start_ms":0,"text":"first"},{"start_ms":1000,"text":"second"}]}"#;
        feed(&mut state, lyrics_a, now + Duration::from_millis(100));
        assert_eq!(
            state.position_at(now + Duration::from_millis(100)),
            Duration::from_millis(600)
        );
        assert_eq!(
            state.timeline_index_at(now + Duration::from_millis(499)),
            Some(0)
        );
        assert_eq!(
            state.timeline_index_at(now + Duration::from_millis(500)),
            Some(1)
        );

        let changed_a = r#"{"type":"state","version":1,"track":{"id":"spotify:track:a","title":"Revised A","artist":"New artist","duration_ms":2000},"position_ms":9000,"playing":false}"#;
        feed(&mut state, changed_a, now + Duration::from_secs(1));
        assert_eq!(state.content.title.as_ref(), "Revised A");
        assert_eq!(state.content.subtitle.as_ref(), "New artist");
        assert_eq!(state.content.accent, FALLBACK_ACCENT);
        assert_eq!(
            state.position_at(now + Duration::from_secs(30)),
            Duration::from_secs(2)
        );
        assert_eq!(state.content.timeline.len(), 2);

        let b = r#"{"type":"state","version":1,"track":{"id":"spotify:track:b","title":"B","artist":"Artist","duration_ms":10000},"position_ms":0,"playing":false}"#;
        feed(&mut state, b, now + Duration::from_secs(2));
        assert!(state.content.timeline.is_empty());
        assert_eq!(
            feed(&mut state, lyrics_a, now + Duration::from_secs(3)),
            Update::Ignored
        );
        assert!(state.content.timeline.is_empty());
        let lyrics_b = r#"{"type":"lyrics","version":1,"track_id":"spotify:track:b","status":"unsynced","lines":[{"text":"one"},{"text":"two"}]}"#;
        feed(&mut state, lyrics_b, now + Duration::from_secs(4));
        assert_eq!(state.content.body.as_ref(), "one\ntwo");
        assert_eq!(state.timeline_index_at(now + Duration::from_secs(20)), None);
        assert_eq!(
            state.position_at(now + Duration::from_secs(20)),
            Duration::ZERO
        );

        let unsorted = r#"{"type":"lyrics","version":1,"track_id":"spotify:track:b","status":"synced","lines":[{"start_ms":2000,"text":"late"},{"start_ms":1000,"text":"early"}]}"#;
        assert!(apply(&mut state, serde_json::from_str(unsorted).unwrap(), now).is_err());
        assert_eq!(state.content.body.as_ref(), "one\ntwo");
        assert_eq!(
            feed(
                &mut state,
                r#"{"type":"state","version":1,"track":null,"position_ms":9999,"playing":true}"#,
                now + Duration::from_secs(5),
            ),
            Update::Ignored
        );
        assert_eq!(state.content.id.as_ref(), "spotify:track:b");
        assert_eq!(state.content.body.as_ref(), "one\ntwo");
        assert!(!state.active);
        assert_eq!(
            state.position_at(now + Duration::from_secs(20)),
            Duration::ZERO
        );
        assert!(state.content.timeline.is_empty());
    }
}
