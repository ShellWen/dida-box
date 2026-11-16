use std::{
    io::{self, IsTerminal, Write},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute, queue,
    style::{Attribute, Print, SetAttribute},
    terminal::{
        self, Clear, ClearType, DisableLineWrap, EnableLineWrap, EnterAlternateScreen,
        LeaveAlternateScreen,
    },
};
use serde_json::Value;

use crate::{config::Settings, control::Client};

const FIELDS: &[(&str, &str, &str)] = &[
    ("device", "Display device", "JSON string path, or null to disable the display"),
    ("device_index", "Device index", "Nonnegative JSON integer"),
    ("force_driver", "Forced display driver", "JSON string driver name, or null for automatic selection"),
    ("size", "Display size", "JSON [width,height], or null for the default size"),
    ("rotation", "Rotation", "JSON integer degrees, or null for the default rotation"),
    ("ui", "Display layout", "JSON string layout name"),
    ("source", "Playback source", "JSON string source name"),
    ("spotify_listen", "Spotify listener", "JSON string socket address, e.g. \"127.0.0.1:17876\""),
    ("fps", "Frames per second", "Positive JSON integer"),
    ("quality", "JPEG quality", "JSON integer quality"),
    ("baud", "Serial baud rate", "Positive JSON integer"),
    ("openrgb", "OpenRGB server", "JSON string socket address, or null to disable lighting"),
    ("openrgb_device", "OpenRGB device names", "JSON array of strings; [] clears the selection"),
    ("openrgb_when_paused", "OpenRGB without playback", "Enter toggles true/false; e edits a JSON boolean"),
];

struct Terminal;

impl Terminal {
    fn enter() -> Result<Self> {
        let guard = Self;
        terminal::enable_raw_mode()?;
        execute!(io::stdout(), EnterAlternateScreen, DisableLineWrap, Hide)?;
        Ok(guard)
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            SetAttribute(Attribute::Reset),
            Show,
            EnableLineWrap,
            LeaveAlternateScreen
        );
    }
}

struct Edit {
    text: String,
    cursor: usize,
}

struct Tui {
    client: Option<Client>,
    settings: Option<Value>,
    status: Option<Value>,
    selected: usize,
    first_visible: usize,
    draft: Option<Edit>,
    connection: String,
    notice: String,
    next_refresh: Instant,
}

impl Tui {
    async fn refresh(&mut self) {
        let result: Result<(Value, Value)> = async {
            if self.client.is_none() {
                self.client = Some(Client::connect().await?);
            }
            let client = self.client.as_ref().context("Daemon connection unavailable")?;
            let (settings, status) = tokio::try_join!(client.config(), client.status())?;
            let status: Value = serde_json::from_str(&status).context("Invalid daemon status JSON")?;
            if !status.is_object() {
                bail!("Daemon status is not a JSON object");
            }
            Ok((serde_json::to_value(settings)?, status))
        }
        .await;

        match result {
            Ok((settings, status)) => {
                self.settings = Some(settings);
                self.status = Some(status);
                self.connection = "Connected to daemon".into();
            }
            Err(error) => {
                self.client = None;
                self.connection = format!("Unavailable: {error:#} (r retries; automatic retry in 2s)");
            }
        }
        self.next_refresh = Instant::now() + Duration::from_secs(if self.client.is_some() { 1 } else { 2 });
    }

    async fn save(&mut self, text: &str) -> Result<()> {
        let key = FIELDS[self.selected].0;
        let value: Value = serde_json::from_str(text).context("Enter a complete JSON value; strings need quotes")?;
        let mut candidate = self.settings.clone().context("No settings received from daemon yet")?;
        candidate[key] = value.clone();
        let settings: Settings = serde_json::from_value(candidate.clone()).context("Wrong JSON type for this setting")?;
        settings.validate()?;
        let client = self.client.as_ref().context("Daemon unavailable; draft kept, retry when connected")?;
        client.set(key, &serde_json::to_string(&value)?).await?;
        self.settings = Some(candidate);
        self.draft = None;
        self.notice = format!("Saved {}", FIELDS[self.selected].1);
        self.next_refresh = Instant::now();
        Ok(())
    }

    async fn key(&mut self, key: KeyEvent) -> bool {
        if key.kind == KeyEventKind::Release {
            return false;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return true;
        }
        if let Some(edit) = self.draft.as_mut() {
            match key.code {
                KeyCode::Esc => {
                    self.draft = None;
                    self.notice = "Edit cancelled; nothing saved".into();
                }
                KeyCode::Enter => {
                    let text = edit.text.clone();
                    if let Err(error) = self.save(&text).await {
                        self.notice = format!("Not saved: {error:#}");
                    }
                }
                KeyCode::Left => {
                    edit.cursor = edit.text[..edit.cursor].char_indices().next_back().map_or(0, |(i, _)| i);
                }
                KeyCode::Right => {
                    if let Some(ch) = edit.text[edit.cursor..].chars().next() {
                        edit.cursor += ch.len_utf8();
                    }
                }
                KeyCode::Home => edit.cursor = 0,
                KeyCode::End => edit.cursor = edit.text.len(),
                KeyCode::Backspace => {
                    if let Some((start, _)) = edit.text[..edit.cursor].char_indices().next_back() {
                        edit.text.drain(start..edit.cursor);
                        edit.cursor = start;
                    }
                }
                KeyCode::Delete => {
                    if edit.cursor < edit.text.len() {
                        edit.text.remove(edit.cursor);
                    }
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    edit.text.clear();
                    edit.cursor = 0;
                }
                KeyCode::Char(ch) if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
                    edit.text.insert(edit.cursor, ch);
                    edit.cursor += ch.len_utf8();
                }
                _ => {}
            }
            return false;
        }

        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return true,
            KeyCode::Up | KeyCode::Left => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Right => self.selected = (self.selected + 1).min(FIELDS.len() - 1),
            KeyCode::Home => self.selected = 0,
            KeyCode::End => self.selected = FIELDS.len() - 1,
            KeyCode::Char('r') => self.next_refresh = Instant::now(),
            KeyCode::Enter | KeyCode::Char('e') => {
                if let Some(value) = self.settings.as_ref().and_then(|settings| settings.get(FIELDS[self.selected].0)) {
                    if let Some(value) = value.as_bool().filter(|_| key.code == KeyCode::Enter) {
                        if let Err(error) = self.save(if value { "false" } else { "true" }).await {
                            self.notice = format!("Not saved: {error:#}");
                        }
                    } else {
                        let text = value.to_string();
                        self.draft = Some(Edit { cursor: text.len(), text });
                        self.notice.clear();
                    }
                } else {
                    self.notice = "Settings unavailable; press r to reconnect".into();
                }
            }
            _ => {}
        }
        false
    }

    fn draw(&mut self) -> Result<()> {
        let (width, height) = terminal::size()?;
        let mut out = io::stdout().lock();
        queue!(out, Hide, Clear(ClearType::All))?;
        line(&mut out, 0, width, "dida-box settings - changes save immediately; q closes only this client")?;
        if height < 13 {
            if height > 1 {
                line(&mut out, 1, width, "Enlarge terminal to at least 13 rows. q quits; edits remain unsaved.")?;
            }
            out.flush()?;
            return Ok(());
        }
        line(&mut out, 1, width, &self.connection)?;
        let stale = if self.client.is_none() { " [last received; stale]" } else { "" };
        for (row, key, label) in [(2, "provider", "Provider"), (3, "display", "Display"), (4, "lighting", "Lighting")] {
            line(&mut out, row, width, &format!("{label}{stale}: {}", self.status_text(key)))?;
        }
        line(&mut out, 5, width, &format!("Playback{stale}: playing={}  title={}", self.status_text("playing"), self.status_text("title")))?;
        line(&mut out, 6, width, &format!("Configuration error{stale}: {}", self.status_text("config_error")))?;

        let rows = usize::from(height - 12);
        if self.selected < self.first_visible {
            self.first_visible = self.selected;
        } else if self.selected >= self.first_visible + rows {
            self.first_visible = self.selected + 1 - rows;
        }
        for (offset, (key, label, _)) in FIELDS.iter().skip(self.first_visible).take(rows).enumerate() {
            let selected = self.first_visible + offset == self.selected;
            let value = self.settings.as_ref().and_then(|settings| settings.get(*key)).map_or_else(|| "<unavailable>".into(), Value::to_string);
            if selected {
                queue!(out, SetAttribute(Attribute::Reverse))?;
            }
            line(&mut out, 7 + offset as u16, width, &format!("{} {label} ({key}): {value}", if selected { ">" } else { " " }))?;
            queue!(out, SetAttribute(Attribute::Reset))?;
        }
        line(&mut out, height - 5, width, &format!("Setting {}/{}: {}", self.selected + 1, FIELDS.len(), FIELDS[self.selected].2))?;
        line(&mut out, height - 4, width, "null clears optional settings; \"null\" is a string; empty input never clears.")?;
        if let Some(edit) = &self.draft {
            // Keep the insertion point visible even for a long JSON array or device path.
            let budget = usize::from(width.saturating_sub(9));
            let before = &edit.text[..edit.cursor];
            let start = before.char_indices().rev().nth(budget / 2).map_or(0, |(i, _)| i);
            line(&mut out, height - 3, width, &format!("JSON: {}{}|{}", if start > 0 { "..." } else { "" }, &before[start..], &edit.text[edit.cursor..]))?;
        } else {
            line(&mut out, height - 3, width, "Arrows select; Enter edits/toggles; e edits JSON; r refreshes; q/Esc closes TUI")?;
        }
        line(&mut out, height - 2, width, &self.notice)?;
        line(&mut out, height - 1, width, if self.draft.is_some() { "Enter saves; Esc cancels; Left/Right/Home/End move; Ctrl-U clears draft; Ctrl-C quits" } else { "Values are JSON. Status refreshes every second; settings are saved by the daemon." })?;
        out.flush()?;
        Ok(())
    }

    fn status_text(&self, key: &str) -> String {
        match self.status.as_ref().and_then(|status| status.get(key)) {
            Some(Value::String(text)) => text.clone(),
            Some(value) => value.to_string(),
            None => "unavailable".into(),
        }
    }
}

fn line(out: &mut impl Write, row: u16, width: u16, text: &str) -> io::Result<()> {
    // Daemon titles, settings and errors must never inject terminal control sequences.
    let text: String = text.chars().map(|ch| if ch.is_control() { ' ' } else { ch }).take(usize::from(width.saturating_sub(1))).collect();
    queue!(out, MoveTo(0, row), Print(text))
}

pub async fn run() -> Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        bail!("The settings TUI needs an interactive terminal");
    }
    #[cfg(unix)]
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    #[cfg(unix)]
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let shutdown = async {
        #[cfg(unix)]
        {
            tokio::select! {
                _ = terminate.recv() => {},
                _ = interrupt.recv() => {},
            }
            Ok::<(), io::Error>(())
        }
        #[cfg(not(unix))]
        {
            tokio::signal::ctrl_c().await
        }
    };
    tokio::pin!(shutdown);
    let _terminal = Terminal::enter()?;
    let mut tui = Tui {
        client: None,
        settings: None,
        status: None,
        selected: 0,
        first_visible: 0,
        draft: None,
        connection: "Connecting to daemon...".into(),
        notice: String::new(),
        next_refresh: Instant::now(),
    };
    tui.draw()?;
    let mut redraw = false;
    loop {
        if Instant::now() >= tui.next_refresh {
            tokio::select! {
                result = &mut shutdown => return Ok(result?),
                _ = tui.refresh() => {},
            }
            redraw = true;
        }
        if redraw {
            tui.draw()?;
            redraw = false;
        }
        let mut input = tokio::task::spawn_blocking(|| -> io::Result<Option<Event>> {
            if event::poll(Duration::from_millis(100))? {
                event::read().map(Some)
            } else {
                Ok(None)
            }
        });
        let event = tokio::select! {
            result = &mut input => result.context("Terminal input task failed")??,
            result = &mut shutdown => {
                // Finish the bounded terminal read before restoring the terminal.
                let _ = input.await;
                result?;
                return Ok(());
            }
        };
        redraw |= event.is_some();
        if let Some(Event::Key(key)) = event {
            let quit = tokio::select! {
                result = &mut shutdown => return Ok(result?),
                quit = tui.key(key) => quit,
            };
            if quit {
                return Ok(());
            }
        }
    }
}
