# Dida Box

> [!WARNING]
> This project was produced almost entirely by fast vibe coding. Code quality and project structure are thoroughly chaotic, coverage is thin, and many paths were never exercised against real hardware. Read the source before relying on it; do not treat it as production software.

A Rust + GPUI offscreen renderer with an independent asynchronous display driver, a persistent daemon, terminal settings editor and desktop tray. Spotify/Spicetify is the default live provider; the `source` setting can select offline demo data. Physical display output and OpenRGB are both disabled by default.

## Architecture

`src/main.rs` dispatches the daemon and its clients. `src/daemon.rs` supervises the provider, display and lighting workers; `src/config.rs` owns validated persistent settings; `src/control.rs` exposes the session D-Bus API. `src/tui.rs`, `src/tray.rs` and `src/preview.rs` are separate clients of that daemon.

| Crate | Responsibility | Boundary |
| --- | --- | --- |
| `dida-core` | Generic `DisplayState`, `DisplayContent`, `TimedText`, `DisplayCommand`, validated RGBA `Frame` | No GPUI, data provider, or device dependencies |
| `dida-ui` | Generic `Panel<State>` offscreen renderer, reusable metadata/timeline/progress components, optional frame preview | Consumes caller-owned state; publishes frames; knows no USB or JPEG protocol |
| `dida-drivers` | Endpoint selection, DLS handshake, per-model native-scan rotation, JPEG encoding and asynchronous output | Consumes upright logical frames; knows no GPUI or playback metadata |

The daemon's display path is `provider -> watch<DisplayState> -> GPUI -> watch<Option<Frame>> -> driver`. `src/spotify.rs` receives playback state and lyrics from the local Spicetify extension over a loopback WebSocket; `src/demo.rs` is the offline provider. `src/lighting/` consumes playback state and PipeWire audio independently of the renderer. A daemon with no physical display does not start GPUI or require a GPU; OpenRGB can run on its own.

GPUI publishes upright logical RGBA frames. The DLS driver rotates them for its native scan direction and encodes JPEGs. Frame pixels are shared through `Arc`; slow consumers receive the latest frame rather than an unbounded queue. GPUI renders on its own thread, serial I/O runs on Tokio, and JPEG encoding uses `spawn_blocking` with a reusable output buffer. Shutdown waits for the current JPEG write, bounded by a five-second timeout.

Preview and snapshot fetch playback state over D-Bus and run their own local GPUI renderer. They do not receive the physical display's framebuffer. Playback commands reach the provider through a bounded `mpsc` channel; the provider owns command behavior and its clock.

UI modules are Rust panels, not dynamically loaded shared libraries. Implement `dida_ui::panels::Panel<YourState>` and pass it through `RenderConfig<YourState>` with a `watch::Receiver<YourState>`; the renderer does not require `DisplayState`. The provided `timeline` and `summary` panels compose reusable components from `dida_ui::components` against `DisplayState`. Register an ID in `PANEL_IDS` and `create` to expose a new built-in panel through the `ui` setting. A data provider publishes `DisplayState` and handles `DisplayCommand`; the renderer and driver do not change. Timed text entries must be sorted by `start`; `observed_at` anchors the local monotonic clock. `SeekBy` offsets are signed milliseconds and are resolved by the provider.

## Prerequisites

Use `nix-shell` for the Rust toolchain and native build/runtime libraries. The workspace declares Rust 1.95 and pins GPUI to a Git revision in `Cargo.toml`, with resolved dependencies in `Cargo.lock`. The daemon and clients need the same user's session D-Bus. Physical display rendering, preview and snapshot need a working GPU adapter and system fonts (`Noto Sans` and `Source Han Sans SC`, including its heavy weights for Chinese lyrics). Offscreen rendering and snapshot do not require `DISPLAY` or `WAYLAND_DISPLAY`; the preview window requires a graphical session. `shell.nix` uses the NixOS GPU driver path and system font configuration.

The TUI requires an interactive terminal. The desktop configuration launcher opens a terminal, and the tray's configuration action needs a supported terminal executable: `xdg-terminal-exec`, `gnome-terminal`, `konsole`, `foot`, `kitty`, `alacritty` or `xterm`. The tray also needs a desktop StatusNotifierItem host.

## Run and configure

There are no startup configuration flags. Running without arguments starts the foreground daemon; keep it running, or use the user service described below. All other commands connect to that daemon:

```sh
dida-box                         # Start the daemon
dida-box tui                     # Edit settings and inspect worker status
dida-box tray                    # Run independent desktop tray controls
dida-box preview                 # Open an independent local preview
dida-box snapshot /tmp/dida.png   # Render the current playback state to a PNG and exit
dida-box status                  # Print runtime status as JSON
```

For development, start `nix-shell --run 'cargo run'` in one terminal and use, for example, `nix-shell --run 'cargo run -- tui'` in another. A second daemon on the same session bus is rejected before it initializes or changes the configuration file. Closing the TUI, preview or tray leaves the daemon running; the tray has a separate **Stop daemon (all outputs)** action. Ctrl-C or SIGTERM stops a foreground daemon.

In the TUI, use the arrow keys to select a setting, then Enter or `e` to edit its JSON value. Strings need JSON quotes, arrays use JSON array syntax, and `null` disables optional outputs or clears an override. Enter validates and saves that field immediately; Escape cancels the draft. Enter toggles the boolean `openrgb_when_paused` directly, while `e` edits its JSON value. `r` refreshes/reconnects; `q` or Escape outside an edit closes only the TUI. Invalid values and save failures are shown without applying the edit.

| Setting | Default | Values / meaning |
| --- | --- | --- |
| `device` | `null` | Endpoint string; `null` disables the physical display |
| `device_index` | `0` | Zero-based USB match index |
| `force_driver` | `null` | Automatic driver selection, or `"soeyi-dls"` |
| `size` | `null` | `[width,height]`; physical native-size check, and local preview/snapshot canvas size |
| `rotation` | `null` | Driver default, or clockwise `0`, `90`, `180`, `270` |
| `ui` | `"timeline"` | `"timeline"` or `"summary"` |
| `source` | `"spotify"` | `"spotify"` or `"demo"` |
| `spotify_listen` | `"127.0.0.1:17876"` | Loopback socket address with a nonzero port |
| `fps` | `10` | Integer from 1 to 30 |
| `quality` | `60` | JPEG quality from 1 to 100 |
| `baud` | `9600` | Positive serial baud rate |
| `openrgb` | `null` | SDK socket address string; `null` disables lighting |
| `openrgb_device` | `[]` | Exact device-name strings; empty selects all eligible devices |
| `openrgb_when_paused` | `false` | Allow audio-reactive lighting without active playback |

Settings are saved before being published to workers. Changes take effect while the daemon runs by restarting the affected provider, display or lighting worker; the daemon itself need not restart. Worker failures appear in status and retry after two seconds. Preview reads `size`, `ui` and `fps` when launched; reopen it to pick up changes to those rendering settings.

### Persistent state and recovery

The daemon is the single configuration writer for its session bus. It serializes client edits and atomically replaces `config.json` with a private `0600` file, creating a missing state directory with mode `0700`. Do not run daemons on different session buses against the same state directory, or edit the file underneath a running daemon; there is no file watcher.

The configuration path is chosen in this order:

1. `$STATE_DIRECTORY/config.json` when `STATE_DIRECTORY` is set to an absolute directory.
2. `$XDG_STATE_HOME/dida-box/config.json` when `XDG_STATE_HOME` is absolute.
3. `$HOME/.local/state/dida-box/config.json` otherwise.

A missing file is initialized with defaults. A malformed, oversized or invalid configuration is preserved on disk: the daemon starts with defaults and reports `config_error` in status, the TUI and the tray. Back up or inspect the damaged file before saving. An explicit successful setting save replaces that file with the in-memory defaults plus the chosen edit, and clears the error; it does not salvage other settings from the invalid file. The schema uses immutable `version: 1`, rejects unknown keys and limits the file to 1 MiB.

### D-Bus control

The session bus service and interface are both `io.github.shellwen.DidaBox1`, at `/io/github/shellwen/DidaBox1`. These methods are available to clients on that user's bus:

| Method | Arguments | Result |
| --- | --- | --- |
| `GetConfig` | None | Settings JSON string |
| `SetSetting` | Two strings: key, JSON value | Validate, persist and apply one setting |
| `GetStatus` | None | JSON with provider/display/lighting status, playback summary and configuration error |
| `GetPlayback` | None | Playback JSON, including lyrics and base64 artwork |
| `Command` | String: `toggle`, `next`, `seek-back`, `seek-forward` | Queue a provider command; seeks are five seconds |
| `Quit` | None | Request orderly daemon shutdown |

For example, enable an existing physical display and OpenRGB on a running daemon. The shell quotes below preserve the JSON string quotes; `ss` is the D-Bus signature for the two string arguments:

```sh
busctl --user call io.github.shellwen.DidaBox1 /io/github/shellwen/DidaBox1 io.github.shellwen.DidaBox1 SetSetting ss device '"usb:33c3:f101"'
busctl --user call io.github.shellwen.DidaBox1 /io/github/shellwen/DidaBox1 io.github.shellwen.DidaBox1 SetSetting ss device_index '0'
busctl --user call io.github.shellwen.DidaBox1 /io/github/shellwen/DidaBox1 io.github.shellwen.DidaBox1 SetSetting ss openrgb '"127.0.0.1:6742"'
busctl --user call io.github.shellwen.DidaBox1 /io/github/shellwen/DidaBox1 io.github.shellwen.DidaBox1 SetSetting ss openrgb_device '["LED Strip","Keyboard"]'
busctl --user call io.github.shellwen.DidaBox1 /io/github/shellwen/DidaBox1 io.github.shellwen.DidaBox1 SetSetting ss openrgb_when_paused 'true'
```

Each call saves independently. Use JSON `null` for `device` or `openrgb` to disable that output. Unknown keys, bad JSON/types, invalid settings and unknown commands return D-Bus errors; failed saves do not publish the candidate setting. A setting value is limited to 64 KiB and playback JSON to 2 MiB. Commands fail when the provider is unavailable or its bounded queue is full. Built-in clients bound each D-Bus operation to five seconds and report communication errors rather than silently starting a replacement daemon.

## Nix and the user service

The flake exposes `packages.default`, built with the pinned `Cargo.lock` and wrapped with `/run/opengl-driver/lib` plus Vulkan on `LD_LIBRARY_PATH`:

```nix
inputs.dida-box.url = "github:ShellWen/dida-box";
```

The package installs `lib/systemd/user/dida-box.service` with `Type=dbus`, `BusName=io.github.shellwen.DidaBox1`, an argument-free `ExecStart`, `Restart=on-failure` and `StateDirectory=dida-box`. Once that unit is on the user manager's unit search path:

```sh
systemctl --user daemon-reload
systemctl --user enable --now dida-box.service
dida-box tui
```

On NixOS, putting the package in `environment.systemPackages` alone may not make its user unit discoverable. Add it to `systemd.packages`, or define the user unit explicitly:

```nix
let
  didaBox = inputs.dida-box.packages.${pkgs.system}.default;
in {
  environment.systemPackages = [ didaBox ];
  systemd.packages = [ didaBox ];
}
```

An explicit NixOS user service is also useful when replacing an existing service that supplied old command-line options:

```nix
let
  didaBox = inputs.dida-box.packages.${pkgs.system}.default;
in {
  environment.systemPackages = [ didaBox ];
  systemd.user.services.dida-box = {
    description = "Dida Box display daemon";
    wantedBy = [ "default.target" ];
    serviceConfig = {
      Type = "dbus";
      BusName = "io.github.shellwen.DidaBox1";
      ExecStart = lib.mkForce "${didaBox}/bin/dida-box";
      Restart = "on-failure";
      RestartSec = 5;
      SuccessExitStatus = "SIGTERM";
      StateDirectory = "dida-box";
    };
  };
}
```

Remove the old startup arguments: all configuration now belongs in persistent settings. `lib.mkForce` above replaces a previously defined `ExecStart`; adapt or remove the old declaration rather than retaining its arguments. `StateDirectory` is optional, but it sets `STATE_DIRECTORY` and therefore selects the configuration location. If you omit it or change it, keep the same effective state path when migrating so the service continues to read the saved settings.

The daemon can run without a desktop session, provided its user manager and session bus are running. Physical display output needs read/write access to the serial port (`dialout` on NixOS) and a GPU adapter. The default Spotify bridge binds to `127.0.0.1:17876`; OpenRGB audio capture needs the same user's PipeWire session.

### Desktop tray

The tray is a separate desktop-session process, not part of the daemon service. Start `dida-box tray` manually or choose **Dida Box Tray** in the application launcher. **Dida Box** opens the full TUI in a terminal. The tray shows worker/configuration errors, playback controls, panel/source choices and the persistent **OpenRGB without Spotify playback** toggle. Quitting only the tray leaves all daemon outputs running.

The package installs `io.github.shellwen.DidaBox.Tray.desktop` under `share/applications`; it does not enable tray autostart. To opt in, add that launcher through your desktop's autostart settings, or copy the installed desktop file to `~/.config/autostart/io.github.shellwen.DidaBox.Tray.desktop`. Keep tray startup in the graphical session, separately from the user daemon.

## Run without hardware

Start the daemon, then set `source` to `"demo"` in the TUI, leaving `device` and `openrgb` as `null`. This runs the provider without a renderer or GPU. To render locally, use another terminal:

```sh
dida-box preview
dida-box snapshot /tmp/dida-box.png
```

Preview and snapshot use the configured `size`, or a default upright 360×960 canvas when `size` is `null`. They use their own renderer even when a physical display is connected, do not negotiate that display's resolution, and do not mirror its output rotation. Snapshot is a daemon client: it uses current playback data, saves a locally rendered frame and exits; it does not start a demo provider or capture the device framebuffer. With a physical device enabled, `size` also checks the negotiated native dimensions, so choose a value compatible with that device or leave it `null`.

Set `ui` to `"summary"` for the alternate panel. The portrait timeline uses a full-width album cover above the title and artist in a fixed-height header, keeping the lyric viewport stable when metadata wraps or artwork is unavailable. Progress stays fixed at the bottom; the landscape panel keeps its artwork column. Timed lyrics use one heavy font size per viewport, natural wrapping and measured row heights. The current lyric settles at 44% of the lyric viewport; line changes scroll the same timeline with a 600 ms ease-out and crossfade the emphasis. Unsynced or unavailable lyric text starts at the top of the lyric viewport. Set `fps` to `20` before launching a preview for smoother local rendering; the device still consumes the latest available frame at its own pace.

Preview keys:

- `Space`: pause or resume playback
- `Left` / `Right`: seek five seconds
- `N`: next track
- `Escape`: close the preview

## Spotify provider

The default source is Spotify. Install `extensions/dida-box.js` as a Spicetify extension and restart Spotify after applying the configuration. The extension lives in this repository and is the only copy; the `boom-in-one` Nix module fetches it from a pinned release instead of keeping a local duplicate. The bridge reads `Spicetify.Player` state and Spotify's authenticated color-lyrics endpoint, then sends only display data to Dida Box. Album-art JPEGs supply the track accent used by the background, progress bar and artist marker; tracks without usable artwork use a neutral fallback. Spotify credentials remain inside the client.

Start the daemon or user service, leave `source` set to `"spotify"`, and launch `dida-box preview` or configure a physical device in the TUI. The extension connects to `ws://127.0.0.1:17876/spotify` and reconnects every two seconds after disconnecting. It supports both client state shapes, `data.track` and `data.item`. The `spotify_listen` setting changes the loopback listener; changing it also requires changing `URL` in the extension. Setting `source` to `"demo"` disables the listener and selects offline sample data.

If the display stays on `Waiting for Spotify` while Spotify has a current track, check that the running client's `Apps/xpui/index.html` loads `extensions/dida-box.js`. An empty `bootstrap.js` does not install the bridge. On Nix, the flake fetches the bridge from a pinned release, so a new extension version needs a matching release tag and a `hash` bump in that module; build and activate the updated Spotify package, then fully quit and restart the client. Editing the Nix module alone does not update an already running Spotify. With both applications running, `ss -tnp '( sport = :17876 or dport = :17876 )'` should show the local connection.

Synced lyrics use Spotify's line timestamps; unsynced lyrics remain plain text. No lyrics and request failure produce separate status messages, never fabricated timestamps. Pausing freezes the local clock; seeking relocates the current line. Track changes clear the old timeline; asynchronous fetch generations prevent late A -> B -> A responses from replacing current lyrics. Playback commands target the current track and are ignored when no track is current.

The provider keeps the last received state while the WebSocket stays open and renders `Waiting for Spotify` only when the client disconnects. Chrome throttles timers in an occluded Spotify window, so the extension's own heartbeat and progress events can arrive many seconds apart; the provider therefore sends a `ping` every three seconds and the extension answers it from its WebSocket message handler, which event dispatch delivers without timer throttling. If the client never answers, the display holds the last state instead of falling back. Transient `track: null` messages during Spotify transitions are ignored. The listener accepts one active connection, validates loopback peers, Host and Spotify's browser Origin, and bounds messages and lyrics. Do not expose the bridge beyond loopback. The Spotify lyrics endpoint is an internal API, not a stable public contract; availability depends on the client, account and track.

## OpenRGB lighting

Lighting is optional; set `openrgb` to a JSON address string such as `"127.0.0.1:6742"` in the TUI or through `SetSetting` as shown above. It does not require a physical display or preview. Enable the OpenRGB SDK server (protocol 3 or newer), and manually select **Direct** mode for each device you want to control, including any independently configured zones. Dida Box skips devices that are not in a supported Direct mode; a nonempty zone explicitly using another mode excludes the whole device.

Set `openrgb_device` to an array such as `["LED Strip","Keyboard"]` to select multiple exact, case-sensitive device names, or `[]` to select all eligible devices. All LEDs share the display's single album-art accent color; audio changes brightness, not hue. Linear zones map low-to-high spectrum bands along their LED order. Nonlinear zones, single-LED zones and LEDs outside zones use the overall spectrum energy for uniform brightness.

Audio comes from the PipeWire default output sink's monitor: the system mix, including other applications, rather than a Spotify-only stream. Run Dida Box with access to the same user's PipeWire server and session manager (such as WirePlumber), which supplies the default sink and capture routing. The Nix package bundles the PipeWire tools on its runtime `PATH`, and `nix-shell` includes them; other installations need `pw-record` and `pw-metadata` on `PATH`. These tools do not start or configure the audio services for you.

The default sink is polled every second; capture restarts when it changes or stalls. With `openrgb_when_paused: false` (the default), paused or inactive playback blacks out eligible Direct devices. With `true`, lighting continues to react to system audio even while Spotify is paused or disconnected: a paused track retains its accent, while a disconnected Spotify provider uses the neutral fallback accent. This option changes the playback gate, not audio capture: silent audio or unavailable/stale capture still produces black. It does not enable an otherwise disabled OpenRGB output.

Temporary audio failures retry; an OpenRGB disconnect retries after two seconds without stopping the display. While the SDK is unreachable, Dida Box cannot update or black out the LEDs, so their last colors may remain until connection and control resume.

Modes are polled before each color update, but the SDK mode check and write are not atomic: avoid other clients changing modes while Dida Box controls the LEDs. Dida Box never selects or saves modes, resizes zones, or saves/restores previous colors or settings. Exit leaves the last colors; it does not restore the previous appearance. Disabling OpenRGB or changing the device selection to exclude a previously controlled device also stops writing to it without blacking it out or restoring its previous colors. Shutdown deliberately avoids extra writes that could touch devices switched to another mode.

## Device selection

Set `device` in the TUI to a JSON endpoint string such as `"/dev/ttyACM0"`, or use `"usb:33c3:f101"` for USB enumeration of the documented Soeyi device (`VID 33c3`, `PID f101`). The D-Bus examples above configure the same settings without a terminal editor.

`device_index` is zero-based and selects among matching USB serial ports in stable port-name order. Set it to `1` for the second matching port. The ordering is lexical by port name, not persistent physical identity across reconnects; use an explicit `"/dev/serial/by-path/..."` endpoint when physical-port stability is required. Setting `force_driver` to `"soeyi-dls"` bypasses the USB VID/PID-to-driver map; it does not bypass the DLS handshake or frame-size checks. It also works with a different `"usb:VID:PID"` selector and still selects devices matching that supplied pair. For a USB identity not known to the driver map, use the actual enumerated pair rather than the Soeyi pair.

The DLS driver applies the documented native-scan correction for known identities (`VMAXA*` is 180°). The `rotation` setting overrides that output rotation with clockwise `0`, `90`, `180` or `270`; `null` uses the driver default. `size`, when non-null, specifies the device's native output dimensions, and a conflicting size fails the display worker. The driver supplies the logical GPUI size, swapping width and height for 90°/270°. With no physical device, `size` controls only the local preview/snapshot canvas.

The driver sends `f0 a5 5a 0f`, asserts serial DTR, accepts the ASCII resolution identity, JPEG-encodes each RGBA frame, and writes complete JPEGs asynchronously. It currently drains touch bytes without forwarding them. EOF and protocol errors end the driver session; the daemon reports the failure and retries the display worker, including connection and renderer startup, after two seconds.

The attached `33c3:f101` device has reported `VMAXA170320*1480S` (320×1480). Serial access needs appropriate device permissions; close competing software before opening the port. Successful writes prove host-side transport, not the physical panel's visible orientation.

A Unix socket relay can emulate the protocol with `device` set to `"unix:/path/to/screen.sock"` (a bare socket path also works). It must reply to the four-byte magic with an ASCII identity such as `MODEL-0320*1480S`, then accept concatenated complete JPEGs. Width is the four decimal digits immediately before `*`. Windows named-pipe transport exists behind platform configuration but has not been runtime-verified on Windows; the daemon and desktop integration documented here target Linux.

## Checks

```sh
nix-shell --run 'cargo fmt --all --check && cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings'
nix-shell --run 'cargo test -p dida-ui -- --ignored --nocapture'
node extensions/dida-box.test.cjs
```

The ignored GPU regressions check upright logical dimensions and colors, artwork transitions without layout shifts or stale pixels, and a stable lyric focus across English/Chinese wrapping, metadata changes and backward seeks. Driver tests cover fragmented handshakes, malformed replies, timeouts, JPEG orientation and decoding, EOF, frame-size validation, bounded buffering, USB index ordering, and forced driver mapping. The Spotify checks cover clock and track transitions, late lyrics, seek bounds, A/B/A fetch races, unsynced text and extension disposal.

For a manual display check, start the daemon, set `source` to `"demo"` and `device` to the real endpoint in the TUI, then run `dida-box preview` in the graphical session. Inspect `dida-box status` for display/renderer errors. Stop the foreground daemon with Ctrl-C, or stop its user service with `systemctl --user stop dida-box.service`, when finished. Host-side JPEG delivery cannot independently prove physical panel orientation.
