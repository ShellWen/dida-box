use std::{io::ErrorKind, process::Stdio, time::Duration};

use anyhow::{Context, Result, bail};
use ksni::{TrayMethods, menu::*};
use serde::Deserialize;
use tokio::{process::{Child, Command}, sync::mpsc, time};

use crate::{config::Settings, control::Client};

#[derive(Clone)]
enum Action {
    Set(&'static str, serde_json::Value),
    Playback(&'static str),
    Configure,
    Preview,
    StopDaemon,
    QuitTray,
    ClearError,
}

#[derive(Deserialize)]
struct DaemonStatus {
    provider: String,
    display: String,
    lighting: String,
    playing: bool,
    title: String,
    config_error: Option<String>,
}

struct Tray {
    actions: mpsc::Sender<Action>,
    config: Option<Settings>,
    daemon: Option<DaemonStatus>,
    connection_error: Option<String>,
    action_error: Option<String>,
}

impl Tray {
    fn send(&mut self, action: Action) {
        if let Err(error) = self.actions.try_send(action) {
            self.action_error = Some(format!("Cannot queue action: {error}"));
        }
    }

    fn item(&self, label: &str, action: Action, enabled: bool) -> ksni::MenuItem<Self> {
        StandardItem {
            label: label.into(),
            enabled,
            activate: Box::new(move |tray: &mut Self| tray.send(action.clone())),
            ..Default::default()
        }.into()
    }

    fn status_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        if let Some(error) = &self.connection_error {
            lines.push(format!("Daemon unavailable: {error}"));
        }
        if let Some(status) = &self.daemon {
            lines.push(format!("{}: {}", if status.playing { "Playing" } else { "Paused / idle" }, status.title));
            lines.push(format!("Provider: {}", status.provider));
            lines.push(format!("Screen: {}", status.display));
            lines.push(format!("OpenRGB: {}", status.lighting));
            if let Some(error) = &status.config_error {
                lines.push(format!("Configuration error: {error}"));
            }
        }
        if let Some(error) = &self.action_error {
            lines.push(format!("Last action error: {error}"));
        }
        lines
    }
}

impl ksni::Tray for Tray {
    const MENU_ON_ACTIVATE: bool = true;

    fn id(&self) -> String {
        "dida-box".into()
    }

    fn title(&self) -> String {
        "Dida Box".into()
    }

    fn icon_name(&self) -> String {
        "video-display".into()
    }

    fn attention_icon_name(&self) -> String {
        "dialog-warning".into()
    }

    fn status(&self) -> ksni::Status {
        if self.connection_error.is_some() || self.action_error.is_some()
            || self.daemon.as_ref().is_some_and(|status| status.config_error.is_some())
        {
            ksni::Status::NeedsAttention
        } else {
            ksni::Status::Active
        }
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            icon_name: self.icon_name(),
            title: self.title(),
            description: self.status_lines().join("\n"),
            ..Default::default()
        }
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        let connected = self.config.is_some();
        let mut menu: Vec<_> = self.status_lines().into_iter().map(|line| {
            StandardItem {
                label: line.replace('_', "__"),
                enabled: false,
                ..Default::default()
            }.into()
        }).collect();
        if self.action_error.is_some() {
            menu.push(self.item("Dismiss last error", Action::ClearError, true));
        }
        menu.push(ksni::MenuItem::Separator);
        menu.push(CheckmarkItem {
            label: "OpenRGB without Spotify playback".into(),
            checked: self.config.as_ref().is_some_and(|config| config.openrgb_when_paused),
            enabled: connected,
            activate: Box::new(|tray: &mut Self| {
                if let Some(config) = &tray.config {
                    tray.send(Action::Set("openrgb_when_paused", (!config.openrgb_when_paused).into()));
                }
            }),
            ..Default::default()
        }.into());
        menu.push(SubMenu {
            label: "Panel".into(),
            enabled: connected,
            submenu: vec![RadioGroup {
                selected: self.config.as_ref().and_then(|config| {
                    dida_ui::panels::PANEL_IDS.iter().position(|id| *id == config.ui)
                }).unwrap_or(usize::MAX),
                options: dida_ui::panels::PANEL_IDS.iter().map(|id| RadioItem {
                    label: (*id).into(),
                    enabled: connected,
                    ..Default::default()
                }).collect(),
                select: Box::new(|tray: &mut Self, index| {
                    if let Some(id) = dida_ui::panels::PANEL_IDS.get(index) {
                        tray.send(Action::Set("ui", (*id).into()));
                    }
                }),
            }.into()],
            ..Default::default()
        }.into());
        menu.push(SubMenu {
            label: "Source".into(),
            enabled: connected,
            submenu: vec![RadioGroup {
                selected: self.config.as_ref().and_then(|config| {
                    ["spotify", "demo"].iter().position(|id| *id == config.source)
                }).unwrap_or(usize::MAX),
                options: ["Spotify", "Demo"].iter().map(|label| RadioItem {
                    label: (*label).into(),
                    enabled: connected,
                    ..Default::default()
                }).collect(),
                select: Box::new(|tray: &mut Self, index| {
                    if let Some(source) = ["spotify", "demo"].get(index) {
                        tray.send(Action::Set("source", (*source).into()));
                    }
                }),
            }.into()],
            ..Default::default()
        }.into());
        menu.push(ksni::MenuItem::Separator);
        menu.push(self.item("Play / pause", Action::Playback("toggle"), connected));
        menu.push(self.item("Next track", Action::Playback("next"), connected));
        menu.push(self.item("Seek backward", Action::Playback("seek-back"), connected));
        menu.push(self.item("Seek forward", Action::Playback("seek-forward"), connected));
        menu.push(ksni::MenuItem::Separator);
        menu.push(self.item("Open full configuration (terminal)", Action::Configure, true));
        menu.push(self.item("Open preview", Action::Preview, connected));
        menu.push(ksni::MenuItem::Separator);
        menu.push(self.item("Stop daemon (all outputs)", Action::StopDaemon, connected));
        menu.push(self.item("Quit tray only", Action::QuitTray, true));
        menu
    }

    fn watcher_offline(&self, reason: ksni::OfflineReason) -> bool {
        eprintln!("Dida Box tray host unavailable: {reason:?}; waiting for it to return");
        true
    }
}

fn launch(configuration: bool) -> Result<Child> {
    let executable = std::env::current_exe().context("locate Dida Box executable")?;
    if !configuration {
        return Command::new(executable).arg("preview").stdin(Stdio::null())
            .spawn().context("launch preview");
    }
    // These terminals accept the executable and its arguments separately, without a shell.
    for (terminal, args) in [
        ("xdg-terminal-exec", &["--"][..]),
        ("gnome-terminal", &["--wait", "--"][..]),
        ("konsole", &["--separate", "-e"][..]),
        ("foot", &["-e"][..]),
        ("kitty", &["--"][..]),
        ("alacritty", &["-e"][..]),
        ("xterm", &["-e"][..]),
    ] {
        match Command::new(terminal).args(args).arg(&executable).arg("tui")
            .stdin(Stdio::null()).spawn()
        {
            Ok(child) => return Ok(child),
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => return Err(error).with_context(|| format!("launch {terminal}")),
        }
    }
    bail!("No supported terminal found; install xdg-terminal-exec, gnome-terminal, konsole, foot, kitty, alacritty or xterm")
}

async fn refresh(client: &mut Option<Client>) -> Result<(Settings, DaemonStatus)> {
    if client.is_none() {
        *client = Some(Client::connect().await?);
    }
    let connected = client.as_ref().context("daemon disconnected")?;
    let config = connected.config().await?;
    let status = serde_json::from_str(&connected.status().await?)
        .context("decode daemon status")?;
    Ok((config, status))
}

pub async fn run() -> Result<()> {
    let (actions, mut requests) = mpsc::channel(16);
    let tray = Tray {
        actions,
        config: None,
        daemon: None,
        connection_error: Some("Connecting…".into()),
        action_error: None,
    };
    let handle = time::timeout(Duration::from_secs(10), tray.spawn()).await
        .context("timed out connecting to the session tray host")?
        .context("Cannot show tray: a session D-Bus and a desktop StatusNotifier tray host are required")?;
    let mut client: Option<Client> = None;
    let mut children: Vec<(&str, Child)> = Vec::new();
    let mut interval = time::interval(Duration::from_secs(2));
    interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    let interrupted = tokio::signal::ctrl_c();
    tokio::pin!(interrupted);
    let result = async {
        loop {
            if handle.is_closed() {
                bail!("Session tray service closed unexpectedly");
            }
            tokio::select! {
                result = &mut interrupted => {
                    result.context("listen for Ctrl-C")?;
                    break;
                }
                request = requests.recv() => {
                    let Some(action) = request else { break };
                    if matches!(action, Action::QuitTray) {
                        break;
                    }
                    if matches!(action, Action::ClearError) {
                        handle.update(|tray| tray.action_error = None).await;
                        continue;
                    }
                    let result = match action {
                        Action::Configure | Action::Preview => {
                            let configuration = matches!(action, Action::Configure);
                            launch(configuration).map(|child| {
                                children.push((if configuration { "Configuration terminal" } else { "Preview" }, child));
                            })
                        }
                        action => {
                            async {
                                let connected = client.as_ref().context("Daemon unavailable; waiting to reconnect")?;
                                match action {
                                    Action::Set(key, value) => connected.set(key, &value.to_string()).await,
                                    Action::Playback(command) => connected.command(command).await,
                                    Action::StopDaemon => connected.quit().await,
                                    _ => unreachable!(),
                                }
                            }.await
                        }
                    };
                    handle.update(|tray| tray.action_error = result.err().map(|error| format!("{error:#}"))).await;
                    interval.reset_immediately();
                }
                _ = interval.tick() => {
                    let refreshed = time::timeout(Duration::from_secs(10), refresh(&mut client)).await
                        .context("daemon status request timed out").and_then(|result| result);
                    match refreshed {
                        Ok((config, daemon)) => {
                            handle.update(move |tray| {
                                tray.config = Some(config);
                                tray.daemon = Some(daemon);
                                tray.connection_error = None;
                            }).await;
                        }
                        Err(error) => {
                            client = None;
                            handle.update(move |tray| {
                                tray.config = None;
                                tray.daemon = None;
                                tray.connection_error = Some(format!("{error:#}"));
                            }).await;
                        }
                    }
                    let mut index = 0;
                    while index < children.len() {
                        let (label, child) = &mut children[index];
                        let error = match child.try_wait() {
                            Ok(None) => { index += 1; continue; }
                            Ok(Some(status)) if status.success() => None,
                            Ok(Some(status)) => Some(format!("{label} exited with {status}")),
                            Err(error) => Some(format!("Cannot monitor {label}: {error}")),
                        };
                        children.swap_remove(index);
                        if let Some(error) = error {
                            handle.update(move |tray| tray.action_error = Some(error)).await;
                        }
                    }
                }
            }
        }
        Ok(())
    }.await;
    handle.shutdown().await;
    result
}
