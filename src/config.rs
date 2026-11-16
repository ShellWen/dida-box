use std::{
    env,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    net::SocketAddr,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub version: u32,
    pub device: Option<String>,
    pub device_index: usize,
    pub force_driver: Option<String>,
    pub size: Option<[u32; 2]>,
    pub rotation: Option<u16>,
    pub ui: String,
    pub source: String,
    pub spotify_listen: SocketAddr,
    pub fps: u32,
    pub quality: u8,
    pub baud: u32,
    pub openrgb: Option<SocketAddr>,
    pub openrgb_device: Vec<String>,
    pub openrgb_when_paused: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            version: 1,
            device: None,
            device_index: 0,
            force_driver: None,
            size: None,
            rotation: None,
            ui: "timeline".into(),
            source: "spotify".into(),
            spotify_listen: SocketAddr::from(([127, 0, 0, 1], 17876)),
            fps: 10,
            quality: 60,
            baud: 9600,
            openrgb: None,
            openrgb_device: Vec::new(),
            openrgb_when_paused: false,
        }
    }
}

impl Settings {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.version == 1, "unsupported configuration version {}", self.version);
        ensure!((1..=30).contains(&self.fps), "fps must be between 1 and 30");
        ensure!((1..=100).contains(&self.quality), "quality must be between 1 and 100");
        ensure!(self.baud > 0, "baud must be positive");
        ensure!(matches!(self.source.as_str(), "spotify" | "demo"), "source must be spotify or demo");
        ensure!(dida_ui::panels::PANEL_IDS.contains(&self.ui.as_str()), "unknown UI panel");
        if let Some(driver) = &self.force_driver {
            ensure!(dida_drivers::DRIVER_IDS.contains(&driver.as_str()), "unknown display driver");
        }
        if let Some(device) = &self.device {
            ensure!(!device.trim().is_empty() && !device.contains('\0'), "device must be a nonempty endpoint");
        }
        if let Some([width, height]) = self.size {
            dida_core::ScreenSize::new(width, height)?;
        }
        ensure!(matches!(self.rotation, None | Some(0 | 90 | 180 | 270)), "rotation must be null, 0, 90, 180, or 270");
        ensure!(self.spotify_listen.ip().is_loopback(), "Spotify bridge must bind to loopback");
        ensure!(self.spotify_listen.port() != 0, "Spotify bridge port must not be zero");
        if let Some(address) = self.openrgb {
            ensure!(address.port() != 0, "OpenRGB port must not be zero");
        }
        ensure!(self.openrgb_device.iter().all(|name| !name.is_empty() && !name.contains('\0')), "OpenRGB device names must not be empty or contain NUL");
        Ok(())
    }

    pub fn with_setting(&self, key: &str, value_json: &str) -> Result<Self> {
        ensure!(key != "version", "configuration version is immutable");
        ensure!(value_json.len() <= 64 * 1024, "setting value exceeds 64 KiB");
        let mut value = serde_json::to_value(self)?;
        let object = value.as_object_mut().context("configuration is not an object")?;
        let slot = object.get_mut(key).with_context(|| format!("unknown setting {key}"))?;
        *slot = serde_json::from_str(value_json).context("invalid JSON setting value")?;
        let candidate: Self = serde_json::from_value(value).context("invalid setting type")?;
        candidate.validate()?;
        Ok(candidate)
    }
}

#[derive(Clone)]
pub struct Store {
    path: PathBuf,
}

impl Store {
    pub fn load() -> Result<(Self, Settings, Option<String>)> {
        let directory = if let Some(path) = env::var_os("STATE_DIRECTORY") {
            let path = PathBuf::from(path);
            ensure!(path.is_absolute(), "STATE_DIRECTORY must be absolute");
            path
        } else if let Some(path) = env::var_os("XDG_STATE_HOME").map(PathBuf::from).filter(|path| path.is_absolute()) {
            path.join("dida-box")
        } else {
            let home = env::var_os("HOME").map(PathBuf::from).context("HOME is not set")?;
            ensure!(home.is_absolute(), "HOME must be absolute");
            home.join(".local/state/dida-box")
        };
        let store = Self { path: directory.join("config.json") };
        let loaded = match File::open(&store.path).and_then(|file| {
            let mut bytes = Vec::new();
            file.take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
            Ok(bytes)
        }) {
            Ok(bytes) => (|| {
                ensure!(bytes.len() <= 1024 * 1024, "configuration exceeds 1 MiB");
                let settings: Settings = serde_json::from_slice(&bytes).context("parsing configuration")?;
                settings.validate()?;
                Ok(settings)
            })(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok((store, Settings::default(), None)),
            Err(error) => Err(error).context("reading configuration"),
        };
        match loaded {
            Ok(settings) => Ok((store, settings, None)),
            Err(error) => {
                let error = format!("{}: {error:#}", store.path.display());
                Ok((store, Settings::default(), Some(error)))
            }
        }
    }

    pub fn initialize(&self, settings: &Settings) -> Result<()> {
        match fs::symlink_metadata(&self.path) {
            Ok(_) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => self.save(settings),
            Err(error) => Err(error).context("checking configuration path"),
        }
    }

    pub fn save(&self, settings: &Settings) -> Result<()> {
        static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
        settings.validate()?;
        let directory = self.path.parent().context("configuration has no directory")?;
        fs::DirBuilder::new().recursive(true).mode(0o700).create(directory)
            .with_context(|| format!("creating {}", directory.display()))?;
        let bytes = serde_json::to_vec_pretty(settings)?;
        ensure!(bytes.len() <= 1024 * 1024, "configuration exceeds 1 MiB");
        let (temporary, mut file) = loop {
            let suffix = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
            let temporary = directory.join(format!(".config.json.{}.{suffix}.tmp", std::process::id()));
            match OpenOptions::new().write(true).create_new(true).mode(0o600).open(&temporary) {
                Ok(file) => break (temporary, file),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => bail!("creating configuration temporary file: {error}"),
            }
        };
        let result = (|| {
            file.write_all(&bytes).context("writing configuration")?;
            file.write_all(b"\n")?;
            file.sync_all().context("syncing configuration")?;
            fs::rename(&temporary, &self.path).context("replacing configuration")?;
            File::open(directory)?.sync_all().context("syncing configuration directory")?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }
}
