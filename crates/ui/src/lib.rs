pub mod components;
pub mod panels;
mod preview;

use std::{
    path::PathBuf,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result};
use dida_core::{Frame, ScreenSize};
use gpui::{
    AppContext, Context, HeadlessAppContext, Render, Window, WindowHandle, prelude::*, px, size,
};
use tokio::sync::watch;

pub use preview::run as run_preview;

pub struct RenderConfig<State> {
    pub size: ScreenSize,
    pub fps: u32,
    pub panel: Box<dyn panels::Panel<State>>,
    pub snapshot: Option<PathBuf>,
}

struct Screen<State> {
    panel: Box<dyn panels::Panel<State>>,
    state: State,
    viewport: ScreenSize,
}

impl<State: 'static> Render for Screen<State> {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        self.panel
            .render(&self.state, self.viewport, Instant::now())
    }
}

pub fn spawn<State: Clone + Send + Sync + 'static>(
    config: RenderConfig<State>,
    states: watch::Receiver<State>,
    frames: watch::Sender<Option<Frame>>,
    shutdown: watch::Receiver<bool>,
) -> thread::JoinHandle<Result<()>> {
    let runtime = tokio::runtime::Handle::current();
    thread::Builder::new()
        .name("gpui-offscreen".into())
        .spawn(move || {
            let result = render_loop(config, states, frames, shutdown, runtime);
            if let Err(error) = &result {
                eprintln!("offscreen renderer: {error:#}");
            }
            result
        })
        .expect("could not create rendering thread")
}

fn render_loop<State: Clone + 'static>(
    config: RenderConfig<State>,
    states: watch::Receiver<State>,
    frames: watch::Sender<Option<Frame>>,
    mut shutdown: watch::Receiver<bool>,
    runtime: tokio::runtime::Handle,
) -> Result<()> {
    anyhow::ensure!(
        (1..=30).contains(&config.fps),
        "frame rate must be between 1 and 30"
    );
    let viewport = config.size;
    let panel = config.panel;
    let text = Arc::new(gpui_wgpu::CosmicTextSystem::new("Noto Sans"));
    let mut cx = HeadlessAppContext::with_platform(text, Arc::new(()), || {
        Ok(Some(Box::new(gpui_wgpu::WgpuHeadlessRenderer::new()?)))
    });
    let initial = states.borrow().clone();
    let window: WindowHandle<Screen<State>> = cx
        .open_window(
            size(px(viewport.width as f32), px(viewport.height as f32)),
            |_, cx| {
                cx.new(|_| Screen {
                    panel,
                    state: initial,
                    viewport,
                })
            },
        )
        .context("creating GPUI offscreen surface")?;
    let mut interval = runtime.block_on(async {
        tokio::time::interval(Duration::from_secs_f64(1. / f64::from(config.fps)))
    });
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut sequence = 0;
    let mut last = None;
    loop {
        if *shutdown.borrow() {
            break;
        }
        let tick = runtime.block_on(async {
            tokio::select! {
                _ = interval.tick() => true,
                _ = shutdown.changed() => false,
            }
        });
        if !tick {
            break;
        }
        let state = states.borrow().clone();
        window.update(&mut cx, |screen, _, cx| {
            screen.state = state;
            cx.notify();
        })?;
        cx.update_window(window.into(), |_, window, cx| {
            window.draw(cx).clear(cx);
        })?;
        let image = cx
            .capture_screenshot(window.into())
            .context("reading GPUI GPU framebuffer")?;
        let image = if image.dimensions() == (viewport.width, viewport.height) {
            image
        } else {
            image::imageops::resize(
                &image,
                viewport.width,
                viewport.height,
                image::imageops::FilterType::Triangle,
            )
        };
        sequence += 1;
        let frame = Frame::new(config.size, image.into_raw(), sequence)?;
        if sequence == 1 {
            eprintln!(
                "GPUI offscreen: {}x{}, {} fps",
                config.size.width, config.size.height, config.fps
            );
        }
        if config.snapshot.is_some() {
            last = Some(frame.clone());
        }
        frames.send_replace(Some(frame));
    }
    if let (Some(path), Some(frame)) = (config.snapshot, last) {
        image::save_buffer_with_format(
            &path,
            &frame.rgba,
            frame.size.width,
            frame.size.height,
            image::ColorType::Rgba8,
            image::ImageFormat::Png,
        )
        .with_context(|| format!("saving snapshot {}", path.display()))?;
        eprintln!("Saved frame {} to {}", frame.sequence, path.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AnyElement, div, rgb};

    struct SplitPanel;

    impl panels::Panel<()> for SplitPanel {
        fn render(&mut self, _: &(), _: ScreenSize, _: Instant) -> AnyElement {
            div()
                .size_full()
                .flex()
                .child(div().flex_1().h_full().bg(rgb(0xff0000)))
                .child(div().flex_1().h_full().bg(rgb(0x00ffff)))
                .into_any_element()
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires a headless GPU adapter and system fonts"]
    async fn offscreen_frame_preserves_logical_orientation() {
        let size = ScreenSize::new(80, 40).unwrap();
        let (_state_tx, states) = watch::channel(());
        let (frames_tx, mut frames) = watch::channel(None);
        let (shutdown_tx, shutdown) = watch::channel(false);
        let renderer = spawn(
            RenderConfig {
                size,
                fps: 10,
                panel: Box::new(SplitPanel),
                snapshot: None,
            },
            states,
            frames_tx,
            shutdown,
        );
        let changed = tokio::time::timeout(Duration::from_secs(30), frames.changed()).await;
        shutdown_tx.send_replace(true);
        tokio::task::spawn_blocking(move || renderer.join().unwrap())
            .await
            .unwrap()
            .unwrap();
        changed.unwrap().unwrap();
        let frame = frames.borrow().clone().unwrap();
        assert_eq!(frame.size, size);
        let pixel = |x: usize, y: usize| {
            let offset = (y * size.width as usize + x) * 4;
            &frame.rgba[offset..offset + 4]
        };
        assert_eq!(pixel(5, 20), &[255, 0, 0, 255]);
        assert_eq!(pixel(75, 20), &[0, 255, 255, 255]);
    }

    struct ArtworkPanel;

    impl panels::Panel<Option<Arc<[u8]>>> for ArtworkPanel {
        fn render(&mut self, art: &Option<Arc<[u8]>>, _: ScreenSize, _: Instant) -> AnyElement {
            div()
                .size_full()
                .bg(rgb(0x000000))
                .flex()
                .flex_col()
                .child(components::album_art(art.clone(), 96.))
                .child(div().w_full().h(px(8.)).flex_shrink_0().bg(rgb(0x00ff00)))
                .into_any_element()
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires a headless GPU adapter and system fonts"]
    async fn artwork_transitions_preserve_layout_and_clear_stale_pixels() {
        fn art(color: [u8; 4]) -> Arc<[u8]> {
            let image = image::RgbaImage::from_pixel(4, 4, image::Rgba(color));
            let mut bytes = std::io::Cursor::new(Vec::new());
            image.write_to(&mut bytes, image::ImageFormat::Png).unwrap();
            bytes.into_inner().into()
        }

        let size = ScreenSize::new(96, 128).unwrap();
        let (state_tx, states) = watch::channel(None);
        let (frames_tx, mut frames) = watch::channel(None);
        let (shutdown_tx, shutdown) = watch::channel(false);
        let renderer = spawn(
            RenderConfig {
                size,
                fps: 10,
                panel: Box::new(ArtworkPanel),
                snapshot: None,
            },
            states,
            frames_tx,
            shutdown,
        );
        let mut results = Vec::new();
        for state in [
            None,
            Some(art([255, 0, 0, 255])),
            None,
            Some(Arc::from(&b"invalid image"[..])),
            Some(art([0, 255, 255, 255])),
        ] {
            state_tx.send_replace(state);
            for _ in 0..2 {
                tokio::time::timeout(Duration::from_secs(30), frames.changed())
                    .await
                    .unwrap()
                    .unwrap();
                frames.borrow_and_update();
            }
            results.push(frames.borrow().clone().unwrap());
        }
        shutdown_tx.send_replace(true);
        tokio::task::spawn_blocking(move || renderer.join().unwrap())
            .await
            .unwrap()
            .unwrap();

        let pixel = |frame: &Frame, x: usize, y: usize| {
            let offset = (y * size.width as usize + x) * 4;
            <[u8; 4]>::try_from(&frame.rgba[offset..offset + 4]).unwrap()
        };
        for frame in &results {
            assert_eq!(pixel(frame, 48, 100), [0, 255, 0, 255]);
            assert_ne!(pixel(frame, 48, 92), [0, 255, 0, 255]);
        }
        assert_eq!(pixel(&results[1], 48, 48), [255, 0, 0, 255]);
        assert_eq!(pixel(&results[4], 48, 48), [0, 255, 255, 255]);
        assert_eq!(pixel(&results[0], 48, 48), pixel(&results[2], 48, 48));
        assert_eq!(pixel(&results[0], 48, 48), pixel(&results[3], 48, 48));
        assert_ne!(pixel(&results[0], 48, 48), [255, 0, 0, 255]);
    }

    struct ClockedTimeline;

    impl panels::Panel<(dida_core::DisplayState, Instant)> for ClockedTimeline {
        fn render(
            &mut self,
            state: &(dida_core::DisplayState, Instant),
            viewport: ScreenSize,
            _: Instant,
        ) -> AnyElement {
            panels::TimelinePanel.render(&state.0, viewport, state.1)
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires a headless GPU adapter and system fonts"]
    async fn lyrics_keep_their_focus_across_wrapping_metadata_and_seeks() {
        use dida_core::{DisplayContent, DisplayState, TimedText};

        let now = Instant::now();
        let content = Arc::new(DisplayContent {
            id: "focus".into(),
            title: "Song".into(),
            subtitle: "Artist".into(),
            body: "".into(),
            duration: Duration::from_secs(40),
            timeline: [
                "Home",
                "Every drop remembers where the river bends",
                "纸灯轻轻亮，照着回家的方向",
                "Hello",
            ]
            .into_iter()
            .enumerate()
            .map(|(i, text)| TimedText {
                start: Duration::from_secs(i as u64 * 10),
                text: text.into(),
            })
            .collect::<Vec<_>>()
            .into(),
            album_art: None,
            accent: 0xf2b84b,
        });
        let state = DisplayState {
            content: content.clone(),
            position: Duration::ZERO,
            active: false,
            observed_at: now,
        };
        let size = ScreenSize::new(320, 1480).unwrap();
        let (states_tx, states) = watch::channel((state.clone(), now));
        let (frames_tx, mut frames) = watch::channel(None);
        let (stop, shutdown) = watch::channel(false);
        let renderer = spawn(
            RenderConfig {
                size,
                fps: 20,
                panel: Box::new(ClockedTimeline),
                snapshot: None,
            },
            states,
            frames_tx,
            shutdown,
        );
        let mut centers = Vec::new();
        for (position, elapsed, settled) in [
            (0, 0, true),
            (10, 1000, false),
            (10, 1700, true),
            (20, 2700, false),
            (20, 3400, true),
            (30, 4400, false),
            (30, 5100, true),
            (0, 6100, false),
            (0, 6800, true),
        ] {
            let mut updated = state.clone();
            updated.position = Duration::from_secs(position);
            if position == 20 {
                let mut long_title = (*content).clone();
                long_title.title = "A title which wraps onto several lines".into();
                updated.content = Arc::new(long_title);
            }
            states_tx.send_replace((updated, now + Duration::from_millis(elapsed)));
            for _ in 0..2 {
                tokio::time::timeout(Duration::from_secs(30), frames.changed())
                    .await
                    .unwrap()
                    .unwrap();
                frames.borrow_and_update();
            }
            if settled {
                let frame = frames.borrow().clone().unwrap();
                let mut top = size.height;
                let mut bottom = 0;
                for y in 500..size.height - 100 {
                    for x in 20..size.width - 20 {
                        let offset = ((y * size.width + x) * 4) as usize;
                        if frame.rgba[offset..offset + 3]
                            .iter()
                            .all(|&channel| channel > 210)
                        {
                            top = top.min(y);
                            bottom = bottom.max(y);
                        }
                    }
                }
                assert!(top < bottom, "current lyric is missing");
                centers.push((top + bottom) as f32 / 2.);
            }
        }
        stop.send_replace(true);
        tokio::task::spawn_blocking(move || renderer.join().unwrap())
            .await
            .unwrap()
            .unwrap();
        for center in &centers[1..] {
            assert!(
                (center - centers[0]).abs() < 10.,
                "lyric focus moved: {centers:?}"
            );
        }
    }
}
