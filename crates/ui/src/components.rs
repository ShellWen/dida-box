use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use dida_core::TimedText;
use gpui::{
    AnyElement, AvailableSpace, ContentMask, Div, FontWeight, ObjectFit, Pixels, RenderImage,
    SharedString, canvas, div, img, linear_color_stop, linear_gradient, point, prelude::*, px,
    relative, rgb, rgba, size,
};

pub fn text_line(text: Arc<str>, font_size: f32, color: u32) -> Div {
    div()
        .w_full()
        .text_size(px(font_size))
        .line_height(px(font_size * 1.3))
        .text_color(rgb(color))
        .child(SharedString::from(text))
}

pub fn metadata(title: Arc<str>, subtitle: Arc<str>, accent: u32, compact: bool) -> Div {
    let title_size = if compact { 28. } else { 32. };
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap_2()
        .flex_shrink_0()
        .child(
            text_line(title, title_size, 0xffffff)
                .line_height(px(title_size * 1.2))
                .font_weight(FontWeight::BLACK)
                .line_clamp(2),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .size(px(6.))
                        .flex_shrink_0()
                        .rounded_full()
                        .bg(rgb(accent)),
                )
                .child(
                    text_line(subtitle, if compact { 22. } else { 26. }, 0xbabac0)
                        .min_w_0()
                        .font_weight(FontWeight::BOLD)
                        .line_clamp(1),
                ),
        )
}

#[derive(Clone, Copy)]
struct LyricRow {
    top: f32,
    height: f32,
}

impl LyricRow {
    fn center(self) -> f32 {
        self.top + self.height / 2.
    }
}

struct LyricScroll {
    from: f32,
    target: f32,
    started: Instant,
    previous: Option<usize>,
    current: Option<usize>,
}

impl LyricScroll {
    fn progress(&self, now: Instant) -> f32 {
        if self.from == self.target {
            return 1.;
        }
        let t = (now.saturating_duration_since(self.started).as_secs_f32() / 0.6).min(1.);
        1. - (1. - t).powi(3)
    }

    fn center(&self, now: Instant) -> f32 {
        self.from + (self.target - self.from) * self.progress(now)
    }

    fn retarget(&mut self, index: Option<usize>, center: f32, now: Instant) {
        if self.current != index {
            self.from = self.center(now);
            self.target = center;
            self.started = now;
            self.previous = self.current;
            self.current = index;
        }
    }
}

struct LyricLayout {
    source: Arc<[TimedText]>,
    width: Pixels,
    font_size: f32,
    rows: Vec<LyricRow>,
    scroll: LyricScroll,
}

fn lyric_text(text: Arc<str>, font_size: f32) -> Div {
    text_line(text, font_size, 0xffffff)
        .line_height(px(font_size * 1.22))
        .font_weight(FontWeight::BLACK)
}

pub fn timeline(
    lines: Arc<[TimedText]>,
    index: Option<usize>,
    now: Instant,
    background: u32,
) -> Div {
    div()
        .flex_1()
        .min_w_0()
        .min_h_0()
        .relative()
        .overflow_hidden()
        .child(
            canvas(
                move |bounds, window, cx| {
                    let font_size = (f32::from(bounds.size.width) * 0.17).clamp(40., 52.);
                    let current = index.filter(|&i| i < lines.len());
                    let layout =
                        window.use_keyed_state("lyric-layout", cx, |_, _| None::<LyricLayout>);
                    layout.update(cx, |cached, cx| {
                        let rebuild = cached.as_ref().is_none_or(|old| {
                            !Arc::ptr_eq(&old.source, &lines) || old.width != bounds.size.width
                        });
                        if rebuild {
                            let mut top = 0.;
                            let rows: Vec<_> = lines
                                .iter()
                                .map(|line| {
                                    let mut text =
                                        lyric_text(line.text.clone(), font_size).into_any_element();
                                    let measured = text.layout_as_root(
                                        size(
                                            AvailableSpace::Definite(bounds.size.width),
                                            AvailableSpace::MinContent,
                                        ),
                                        window,
                                        cx,
                                    );
                                    let height = f32::from(measured.height).max(font_size * 1.22);
                                    let row = LyricRow { top, height };
                                    top += height + font_size * 0.7;
                                    row
                                })
                                .collect();
                            let center = rows
                                .get(current.unwrap_or(0))
                                .map_or(0., |row| row.center());
                            *cached = Some(LyricLayout {
                                source: lines.clone(),
                                width: bounds.size.width,
                                font_size,
                                rows,
                                scroll: LyricScroll {
                                    from: center,
                                    target: center,
                                    started: now,
                                    previous: current,
                                    current,
                                },
                            });
                        }
                        let layout = cached.as_mut().unwrap();
                        let target = layout
                            .rows
                            .get(current.unwrap_or(0))
                            .map_or(0., |row| row.center());
                        layout.scroll.retarget(current, target, now);
                        let offset =
                            f32::from(bounds.size.height) * 0.44 - layout.scroll.center(now);
                        let progress = layout.scroll.progress(now);
                        let mut visible = Vec::<AnyElement>::new();
                        let first = layout
                            .rows
                            .partition_point(|row| row.top + row.height + offset <= 0.);
                        let end = layout.rows.partition_point(|row| {
                            row.top + offset < f32::from(bounds.size.height)
                        });
                        for i in first..end {
                            let row = layout.rows[i];
                            let y = row.top + offset;
                            let emphasis = if Some(i) == current {
                                progress
                            } else if Some(i) == layout.scroll.previous {
                                1. - progress
                            } else {
                                0.
                            };
                            let opacity = 0.22 + 0.78 * emphasis;
                            let mut text = lyric_text(lines[i].text.clone(), layout.font_size)
                                .opacity(opacity)
                                .into_any_element();
                            text.layout_as_root(
                                size(
                                    AvailableSpace::Definite(bounds.size.width),
                                    AvailableSpace::MinContent,
                                ),
                                window,
                                cx,
                            );
                            window.with_content_mask(Some(ContentMask { bounds }), |window| {
                                text.prepaint_at(bounds.origin + point(px(0.), px(y)), window, cx);
                            });
                            visible.push(text);
                        }
                        visible
                    })
                },
                |bounds, mut visible, window, cx| {
                    window.with_content_mask(Some(ContentMask { bounds }), |window| {
                        for text in &mut visible {
                            text.paint(window, cx);
                        }
                    });
                },
            )
            .size_full(),
        )
        .children([0., 180.].into_iter().map(|angle| {
            div()
                .absolute()
                .left_0()
                .w_full()
                .h(px(40.))
                .when(angle == 180., |this| this.top_0())
                .when(angle == 0., |this| this.bottom_0())
                .bg(linear_gradient(
                    angle,
                    linear_color_stop(rgb(background), 0.),
                    linear_color_stop(rgba((background << 8) & 0xffffff00), 1.),
                ))
        }))
}

#[cfg(test)]
mod tests {
    use super::LyricScroll;
    use std::time::{Duration, Instant};

    #[test]
    fn lyric_scroll_keeps_continuity_when_seeking_during_a_transition() {
        let now = Instant::now();
        let mut scroll = LyricScroll {
            from: 100.,
            target: 100.,
            started: now,
            previous: Some(0),
            current: Some(0),
        };
        scroll.retarget(Some(1), 400., now);
        assert_eq!(scroll.center(now), 100.);
        let midway = now + Duration::from_millis(200);
        let center = scroll.center(midway);
        assert!(center > 100. && center < 400.);
        scroll.retarget(Some(0), 100., midway);
        assert_eq!(scroll.center(midway), center);
        assert_eq!(scroll.center(midway + Duration::from_secs(1)), 100.);
        scroll.retarget(None, 50., midway + Duration::from_secs(1));
        assert_eq!(scroll.center(midway + Duration::from_secs(2)), 50.);
    }
}

#[derive(Default)]
struct Artwork {
    source: Option<Arc<[u8]>>,
    image: Option<Arc<RenderImage>>,
}

pub fn album_art(data: Option<Arc<[u8]>>, edge: f32) -> Div {
    div()
        .w(px(edge))
        .h(px(edge))
        .flex_shrink_0()
        .relative()
        .rounded(px(6.))
        .bg(rgb(0x1c1d22))
        .overflow_hidden()
        .child(
            div()
                .absolute()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .size(px(edge * 0.62))
                        .rounded_full()
                        .bg(rgb(0x101114))
                        .border_1()
                        .border_color(rgb(0x3b3d45))
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(
                            div()
                                .size(px(edge * 0.2))
                                .rounded_full()
                                .bg(rgb(0x343640))
                                .flex()
                                .items_center()
                                .justify_center()
                                .child(
                                    div().size(px(edge * 0.04)).rounded_full().bg(rgb(0x101114)),
                                ),
                        ),
                ),
        )
        .child(
            img(move |window: &mut gpui::Window, cx: &mut gpui::App| {
                let cache = window.use_keyed_state("album-art", cx, |_, _| Artwork::default());
                cache.update(cx, |cache, _| {
                    let unchanged = match (&cache.source, &data) {
                        (Some(old), Some(new)) => Arc::ptr_eq(old, new),
                        (None, None) => true,
                        _ => false,
                    };
                    if !unchanged {
                        if let Some(old) = cache.image.take() {
                            let _ = window.drop_image(old);
                        }
                        cache.source = data.clone();
                        cache.image = data.as_ref().and_then(|bytes| {
                            let mut pixels = image::load_from_memory(bytes).ok()?.into_rgba8();
                            for pixel in pixels.chunks_exact_mut(4) {
                                pixel.swap(0, 2);
                            }
                            Some(Arc::new(RenderImage::new(vec![image::Frame::new(pixels)])))
                        });
                    }
                    cache.image.clone().map(Ok)
                })
            })
            .absolute()
            .size_full()
            .object_fit(ObjectFit::Contain),
        )
}

pub fn progress(position: Duration, duration: Duration, accent: u32) -> Div {
    let fraction = if duration.is_zero() {
        0.
    } else {
        (position.as_secs_f64() / duration.as_secs_f64()).clamp(0., 1.) as f32
    };
    let position = position.as_secs();
    let duration = duration.as_secs();
    div()
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .h(px(4.))
                .w_full()
                .rounded_full()
                .overflow_hidden()
                .bg(rgb(0x34363e))
                .child(div().h_full().w(relative(fraction)).bg(rgb(accent))),
        )
        .child(
            div()
                .flex()
                .justify_between()
                .text_size(px(22.))
                .font_weight(FontWeight::BOLD)
                .text_color(rgb(0xc7c9d1))
                .child(format!("{}:{:02}", position / 60, position % 60))
                .child(format!("{}:{:02}", duration / 60, duration % 60)),
        )
}
