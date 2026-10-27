use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Result, ensure};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScreenSize {
    pub width: u32,
    pub height: u32,
}

impl ScreenSize {
    pub fn new(width: u32, height: u32) -> Result<Self> {
        ensure!(
            (1..=4096).contains(&width) && (1..=4096).contains(&height),
            "screen dimensions must be between 1 and 4096"
        );
        ensure!(
            u64::from(width) * u64::from(height) <= 8_388_608,
            "screen exceeds 8 megapixels"
        );
        Ok(Self { width, height })
    }
}

#[derive(Clone, Debug)]
pub struct Frame {
    pub size: ScreenSize,
    pub rgba: Arc<Vec<u8>>,
    pub sequence: u64,
}

impl Frame {
    pub fn new(size: ScreenSize, rgba: Vec<u8>, sequence: u64) -> Result<Self> {
        ScreenSize::new(size.width, size.height)?;
        ensure!(
            rgba.len() == size.width as usize * size.height as usize * 4,
            "RGBA frame length does not match its dimensions"
        );
        Ok(Self {
            size,
            rgba: Arc::new(rgba),
            sequence,
        })
    }
}

#[derive(Clone, Debug)]
pub struct TimedText {
    pub start: Duration,
    pub text: Arc<str>,
}

#[derive(Clone, Debug)]
pub struct DisplayContent {
    pub id: Arc<str>,
    pub title: Arc<str>,
    pub subtitle: Arc<str>,
    pub body: Arc<str>,
    pub duration: Duration,
    pub timeline: Arc<[TimedText]>,
    pub album_art: Option<Arc<[u8]>>,
    pub accent: u32,
}

#[derive(Clone, Debug)]
pub struct DisplayState {
    pub content: Arc<DisplayContent>,
    pub position: Duration,
    pub active: bool,
    pub observed_at: Instant,
}

pub enum DisplayCommand {
    ToggleActive,
    Seek(Duration),
    SeekBy(i64),
    Next,
}

impl DisplayState {
    pub fn position_at(&self, now: Instant) -> Duration {
        let elapsed = if self.active {
            now.saturating_duration_since(self.observed_at)
        } else {
            Duration::ZERO
        };
        self.position
            .saturating_add(elapsed)
            .min(self.content.duration)
    }

    pub fn timeline_index_at(&self, now: Instant) -> Option<usize> {
        let position = self.position_at(now);
        self.content
            .timeline
            .partition_point(|line| line.start <= position)
            .checked_sub(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_state_freezes_seeks_and_respects_timeline_boundaries() {
        let now = Instant::now();
        let content = Arc::new(DisplayContent {
            id: "test".into(),
            title: "Test".into(),
            subtitle: "Source".into(),
            body: "".into(),
            duration: Duration::from_secs(20),
            album_art: None,
            accent: 0,
            timeline: vec![
                TimedText {
                    start: Duration::from_secs(2),
                    text: "one".into(),
                },
                TimedText {
                    start: Duration::from_secs(5),
                    text: "two".into(),
                },
            ]
            .into(),
        });
        let mut state = DisplayState {
            content,
            position: Duration::ZERO,
            active: true,
            observed_at: now,
        };
        assert_eq!(state.timeline_index_at(now), None);
        assert_eq!(
            state.timeline_index_at(now + Duration::from_secs(2)),
            Some(0)
        );
        assert_eq!(
            state.timeline_index_at(now + Duration::from_secs(5)),
            Some(1)
        );
        assert_eq!(
            state.position_at(now + Duration::from_secs(30)),
            Duration::from_secs(20)
        );
        state.active = false;
        state.position = Duration::from_secs(3);
        assert_eq!(
            state.timeline_index_at(now + Duration::from_secs(30)),
            Some(0)
        );
        state.position = Duration::from_secs(1);
        assert_eq!(state.timeline_index_at(now), None);
    }

    #[test]
    fn frame_rejects_invalid_dimensions_and_byte_lengths() {
        assert!(
            Frame::new(
                ScreenSize {
                    width: u32::MAX,
                    height: u32::MAX,
                },
                Vec::new(),
                1,
            )
            .is_err()
        );
        assert!(Frame::new(ScreenSize::new(2, 2).unwrap(), vec![0; 15], 1).is_err());
    }
}
