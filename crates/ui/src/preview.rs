use std::{cell::RefCell, rc::Rc, sync::Arc, time::Duration};

use anyhow::Result;
use dida_core::{DisplayCommand, Frame, ScreenSize};
use gpui::{
    AppContext, Bounds, Context, FocusHandle, KeyBinding, ObjectFit, Render, RenderImage,
    TitlebarOptions, Window, WindowBounds, WindowOptions, actions, div, img, prelude::*, px, rgb,
    size,
};
use tokio::sync::{mpsc, watch};

actions!(preview, [ToggleActive, SeekBack, SeekForward, Next, Quit]);

struct Preview {
    image: Option<Arc<RenderImage>>,
    sequence: u64,
    focus: FocusHandle,
    commands: mpsc::Sender<DisplayCommand>,
    shutdown: watch::Sender<bool>,
}

impl Preview {
    fn seek(&self, backward: bool) {
        let _ = self
            .commands
            .try_send(DisplayCommand::SeekBy(if backward { -5000 } else { 5000 }));
    }

    fn set_frame(&mut self, frame: Frame, window: &mut Window, cx: &mut Context<Self>) {
        if frame.sequence == self.sequence {
            return;
        }
        let mut pixels = frame.rgba.to_vec();
        for pixel in pixels.chunks_exact_mut(4) {
            pixel.swap(0, 2);
        }
        let buffer = image::RgbaImage::from_raw(frame.size.width, frame.size.height, pixels)
            .expect("validated frame");
        let next = Arc::new(RenderImage::new(vec![image::Frame::new(buffer)]));
        if let Some(old) = self.image.replace(next) {
            let _ = window.drop_image(old);
        }
        self.sequence = frame.sequence;
        cx.notify();
    }
}

impl Render for Preview {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .bg(rgb(0x080c0b))
            .flex()
            .items_center()
            .justify_center()
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &ToggleActive, _, _| {
                let _ = this.commands.try_send(DisplayCommand::ToggleActive);
            }))
            .on_action(cx.listener(|this, _: &SeekBack, _, _| this.seek(true)))
            .on_action(cx.listener(|this, _: &SeekForward, _, _| this.seek(false)))
            .on_action(cx.listener(|this, _: &Next, _, _| {
                let _ = this.commands.try_send(DisplayCommand::Next);
            }))
            .on_action(cx.listener(|this, _: &Quit, _, cx| {
                this.shutdown.send_replace(true);
                cx.quit();
            }))
            .children(
                self.image
                    .clone()
                    .map(|image| img(image).size_full().object_fit(ObjectFit::Contain)),
            )
    }
}

pub fn run(
    dimensions: ScreenSize,
    frames: watch::Receiver<Option<Frame>>,
    commands: mpsc::Sender<DisplayCommand>,
    shutdown: watch::Sender<bool>,
) -> Result<()> {
    let failed = Rc::new(RefCell::new(None));
    let result = failed.clone();
    gpui_platform::application().run(move |cx| {
        cx.bind_keys([
            KeyBinding::new("space", ToggleActive, None),
            KeyBinding::new("left", SeekBack, None),
            KeyBinding::new("right", SeekForward, None),
            KeyBinding::new("n", Next, None),
            KeyBinding::new("escape", Quit, None),
        ]);
        let scale = (1000. / dimensions.height as f32).min(1.);
        let bounds = Bounds::centered(
            None,
            size(
                px(dimensions.width as f32 * scale),
                px(dimensions.height as f32 * scale),
            ),
            cx,
        );
        let handle = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("Dida Box · Preview".into()),
                    ..Default::default()
                }),
                focus: true,
                ..Default::default()
            },
            |window, cx| {
                cx.new(|cx| {
                    let focus = cx.focus_handle();
                    window.focus(&focus, cx);
                    Preview {
                        image: None,
                        sequence: 0,
                        focus,
                        commands,
                        shutdown: shutdown.clone(),
                    }
                })
            },
        );
        let handle = match handle {
            Ok(handle) => handle,
            Err(error) => {
                *result.borrow_mut() = Some(error);
                shutdown.send_replace(true);
                cx.quit();
                return;
            }
        };
        let closed = shutdown.clone();
        cx.on_window_closed(move |cx, _| {
            closed.send_replace(true);
            cx.quit();
        })
        .detach();
        cx.spawn(async move |cx| {
            let mut frames = frames;
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(16))
                    .await;
                if *shutdown.borrow() {
                    cx.update(|cx| cx.quit());
                    break;
                }
                if frames.has_changed().unwrap_or(false) {
                    let frame = frames.borrow_and_update().clone();
                    if let Some(frame) = frame
                        && handle
                            .update(cx, |view, window, cx| view.set_frame(frame, window, cx))
                            .is_err()
                    {
                        break;
                    }
                }
            }
        })
        .detach();
    });
    if let Some(error) = failed.borrow_mut().take() {
        return Err(error);
    }
    Ok(())
}
