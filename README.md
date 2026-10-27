# Dida Box

> [!WARNING]
> This project was produced almost entirely by fast vibe coding. Code quality and project structure are thoroughly chaotic, coverage is thin, and many paths were never exercised against real hardware. Read the source before relying on it; do not treat it as production software.

A Rust + GPUI offscreen renderer with an independent asynchronous display driver. Spotify/Spicetify is the default live provider; `--source demo` selects the offline placeholder.

## Architecture

`src/main.rs` is the composition root; it connects these independent crates:

| Crate | Responsibility | Boundary |
| --- | --- | --- |
| `dida-core` | Generic `DisplayState`, `DisplayContent`, `TimedText`, `DisplayCommand`, validated RGBA `Frame` | No GPUI, data provider, or device dependencies |
| `dida-ui` | Generic `Panel<State>` offscreen renderer, reusable metadata/timeline/progress components, optional frame preview | Consumes caller-owned state; publishes frames; knows no USB or JPEG protocol |
| `dida-drivers` | Endpoint selection, DLS handshake, per-model native-scan rotation, JPEG encoding and asynchronous output | Consumes upright logical frames; knows no GPUI or playback metadata |

The data path is `provider -> watch<DisplayState> -> GPUI -> watch<Option<Frame>> -> driver / preview`. `src/spotify.rs` receives playback state and lyrics from the local Spicetify extension over a loopback WebSocket; `src/demo.rs` is the offline provider. GPUI publishes upright logical RGBA frames. Preview displays those frames unchanged; the DLS driver rotates them for its native scan direction and encodes JPEGs. Frame pixels are shared through `Arc`; slow consumers receive the latest frame rather than an unbounded queue. Preview commands use a bounded `mpsc` channel; the provider owns command behavior and its clock. GPUI renders on its own thread, serial I/O runs on Tokio, and JPEG encoding uses `spawn_blocking` with a reusable output buffer. Shutdown waits for the current JPEG write, bounded by a five-second timeout.

UI modules are Rust panels, not dynamically loaded shared libraries. Implement `dida_ui::panels::Panel<YourState>` and pass it through `RenderConfig<YourState>` with a `watch::Receiver<YourState>`; the renderer does not require `DisplayState`. The provided `timeline` and `summary` panels compose reusable components from `dida_ui::components` against `DisplayState`. Register an ID in `PANEL_IDS` and `create` to expose a new built-in panel through `--ui`. A data provider publishes `DisplayState` and handles `DisplayCommand`; the renderer and driver do not change. Timed text entries must be sorted by `start`; `observed_at` anchors the local monotonic clock. `SeekBy` offsets are signed milliseconds and are resolved by the provider, so preview does not read source state.

## Prerequisites

Use `nix-shell` for the Rust toolchain and native build/runtime libraries. The workspace declares Rust 1.95 and pins GPUI to a Git revision in `Cargo.toml`, with resolved dependencies in `Cargo.lock`. Runtime needs a working GPU adapter and system fonts (`Noto Sans` and `Source Han Sans SC`, including its heavy weights for Chinese lyrics). Offscreen rendering does not require `DISPLAY` or `WAYLAND_DISPLAY`; `--preview` requires a graphical session. `shell.nix` uses the NixOS GPU driver path and system font configuration.

## Nix

The flake exposes `packages.default`, built with a pinned `Cargo.lock` and wrapped with `/run/opengl-driver/lib` plus Vulkan on `LD_LIBRARY_PATH`:

```nix
inputs.dida-box.url = "github:ShellWen/dida-box/v0.2.0";
```

```nix
environment.systemPackages = [ inputs.dida-box.packages.${pkgs.system}.default ];
```

A user-scoped service keeps the panel fed without a desktop session:

```nix
systemd.user.services.dida-box = {
  Unit.Description = "Dida Box for the Soeyi CH1L screen";
  Install.WantedBy = [ "default.target" ];
  Service = {
    ExecStart = "${inputs.dida-box.packages.${pkgs.system}.default}/bin/dida-box --device usb:33c3:f101 --device-index 0";
    Restart = "on-failure";
    RestartSec = 5;
  };
};
```

The service needs read/write access to the serial port (`dialout` on NixOS) and a GPU adapter; it binds the Spotify bridge on `127.0.0.1:17876`.

## Run without hardware

```sh
nix-shell --run 'cargo run -- --source demo --snapshot /tmp/dida-box.png --size 360x960 --duration 2'
```

Preview only, with an upright logical 320×1480 canvas:

```sh
nix-shell --run 'cargo run -- --source demo --preview --size 320x1480'
```

Use `--ui summary` for the alternate panel. The DLS driver applies the documented native-scan correction for known identities (`VMAXA*` is 180°). `--rotation` overrides that driver output rotation and accepts `0`, `90`, `180`, or `270`, clockwise; it requires `--device`. `--size` specifies the device's native output dimensions when connected, and a conflicting size is rejected. The driver supplies the logical GPUI size (swapping width and height for 90°/270°). Without a device, `--size` specifies the upright logical canvas. Defaults are 10 FPS, JPEG quality 60, and 9600 baud; `--fps`, `--quality`, and `--baud` remain adjustable. `--duration N` provides a bounded run; Ctrl-C requests orderly shutdown.

The portrait timeline uses a full-width album cover above the title and artist in a fixed-height header, keeping the lyric viewport stable when metadata wraps or artwork is unavailable. Progress stays fixed at the bottom; the landscape panel keeps its artwork column. Timed lyrics use one heavy font size per viewport, natural wrapping and measured row heights. The current lyric settles at 44% of the lyric viewport; line changes scroll the same timeline with a 600 ms ease-out and crossfade the emphasis. Unsynced or unavailable lyric text starts at the top of the lyric viewport. Use `--fps 20` for a smoother local preview; the device still consumes the latest available frame at its own pace.

Use `--preview` to show the upright RGBA framebuffer from GPUI in a local window; it does not mirror device-specific output rotation. The DLS driver's JPEG may be rotated for the physical panel. Preview keys:

- `Space`: pause or resume playback
- `Left` / `Right`: seek five seconds
- `N`: next track
- `Escape`: quit

## Spotify provider

The default source is Spotify. Install `extensions/dida-box.js` as a Spicetify extension and restart Spotify after applying the configuration. The extension lives in this repository and is the only copy; the `boom-in-one` Nix module fetches it from a pinned release instead of keeping a local duplicate. The bridge reads `Spicetify.Player` state and Spotify's authenticated color-lyrics endpoint, then sends only display data to Dida Box. Album-art JPEGs supply the track accent used by the background, progress bar and artist marker; tracks without usable artwork use a neutral fallback. Spotify credentials remain inside the client.

```sh
nix-shell --run 'cargo run -- --preview --size 320x1480'
nix-shell --run 'cargo run -- --device usb:33c3:f101 --device-index 0 --force-driver soeyi-dls --preview'
```

The extension connects to `ws://127.0.0.1:17876/spotify` and reconnects every two seconds after disconnecting. It supports both client state shapes, `data.track` and `data.item`. `--spotify-listen IP:PORT` changes the loopback listener; changing it also requires changing `URL` in the extension. `--source demo` disables the listener and selects offline sample data.

If the display stays on `Waiting for Spotify` while Spotify has a current track, check that the running client's `Apps/xpui/index.html` loads `extensions/dida-box.js`. An empty `bootstrap.js` does not install the bridge. On Nix, the flake fetches the bridge from a pinned release, so a new extension version needs a matching release tag and a `hash` bump in that module; build and activate the updated Spotify package, then fully quit and restart the client. Editing the Nix module alone does not update an already running Spotify. With both applications running, `ss -tnp '( sport = :17876 or dport = :17876 )'` should show the local connection.

Synced lyrics use Spotify's line timestamps; unsynced lyrics remain plain text. No lyrics and request failure produce separate status messages, never fabricated timestamps. Pausing freezes the local clock; seeking relocates the current line. Track changes clear the old timeline; asynchronous fetch generations prevent late A -> B -> A responses from replacing current lyrics. Preview commands target the current track and are ignored when no track is current.

The provider keeps the last received state while the WebSocket stays open and renders `Waiting for Spotify` only when the client disconnects. Chrome throttles timers in an occluded Spotify window, so the extension's own heartbeat and progress events can arrive many seconds apart; the provider therefore sends a `ping` every three seconds and the extension answers it from its WebSocket message handler, which event dispatch delivers without timer throttling. If the client never answers, the display holds the last state instead of falling back. Transient `track: null` messages during Spotify transitions are ignored. The listener accepts one active connection, validates loopback peers, Host and Spotify's browser Origin, and bounds messages and lyrics. Do not expose the bridge beyond loopback. The Spotify lyrics endpoint is an internal API, not a stable public contract; availability depends on the client, account and track.


## Device selection

Use a direct endpoint when the serial path is already known:

```sh
nix-shell --run 'cargo run -- --device /dev/ttyACM0'
```

Use USB enumeration for the documented Soeyi device (`VID 33c3`, `PID f101`):

```sh
nix-shell --run 'cargo run -- --device usb:33c3:f101 --device-index 0 --preview'
```

`--device-index` is zero-based and selects among matching USB serial ports in stable port-name order. `--force-driver soeyi-dls` bypasses the USB VID/PID-to-driver map; it does not bypass the DLS handshake or frame-size checks.

Choose `--device-index 1` for the second matching port. The ordering is lexical by port name, not persistent physical identity across reconnects; use an explicit `/dev/serial/by-path/...` endpoint when physical-port stability is required. `--force-driver soeyi-dls` also works with a different `usb:VID:PID` selector; it still selects devices matching that supplied pair. For a USB identity not known to the driver map, use the actual enumerated pair rather than the Soeyi pair.

The driver sends `f0 a5 5a 0f`, asserts serial DTR, accepts the ASCII resolution identity, JPEG-encodes each RGBA frame, and writes complete JPEGs asynchronously. It does not reconnect automatically and currently drains touch bytes without forwarding them.

The attached `33c3:f101` device has reported `VMAXA170320*1480S` (320×1480). Serial access needs appropriate device permissions; close competing software before opening the port. Successful writes prove host-side transport, not the physical panel's visible orientation.

A Unix socket relay can emulate the protocol with `--device unix:/path/to/screen.sock` (a bare socket path also works). It must reply to the four-byte magic with an ASCII identity such as `MODEL-0320*1480S`, then accept concatenated complete JPEGs. Width is the four decimal digits immediately before `*`. Windows named-pipe transport exists behind platform configuration but has not been runtime-verified on Windows. EOF and protocol errors terminate the session; no automatic reconnect or touch UI is implemented.

## Checks

```sh
nix-shell --run 'cargo fmt --all --check && cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings'
nix-shell --run 'cargo test -p dida-ui -- --ignored --nocapture'
nix-shell --run 'cargo run -- --source demo --device usb:33c3:f101 --device-index 0 --force-driver soeyi-dls --preview --duration 8 --fps 3'
node extensions/dida-box.test.cjs
```

The ignored GPU regressions check upright logical dimensions and colors, artwork transitions without layout shifts or stale pixels, and a stable lyric focus across English/Chinese wrapping, metadata changes and backward seeks. Driver tests cover fragmented handshakes, malformed replies, timeouts, JPEG orientation and decoding, EOF, frame-size validation, bounded buffering, USB index ordering, and forced driver mapping. The hardware command exercises the display and preview together; it requires the device and a graphical session. Host-side JPEG delivery cannot independently prove physical panel orientation. The Spotify checks cover clock and track transitions, late lyrics, seek bounds, A/B/A fetch races, unsynced text and extension disposal.
