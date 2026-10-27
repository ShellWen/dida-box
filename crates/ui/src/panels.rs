use std::time::Instant;

use dida_core::{DisplayState, ScreenSize};
use gpui::{AnyElement, div, prelude::*, px, rgb};

use crate::components;

pub const PANEL_IDS: &[&str] = &["timeline", "summary"];

pub trait Panel<State>: Send + 'static {
    fn render(&mut self, state: &State, viewport: ScreenSize, now: Instant) -> AnyElement;
}

pub fn create(id: &str) -> anyhow::Result<Box<dyn Panel<DisplayState>>> {
    match id {
        "timeline" => Ok(Box::new(TimelinePanel)),
        "summary" => Ok(Box::new(SummaryPanel)),
        _ => anyhow::bail!(
            "unknown UI module {id:?}; available: {}",
            PANEL_IDS.join(", ")
        ),
    }
}

fn accent_background(accent: u32) -> u32 {
    [16, 8, 0].into_iter().fold(0, |color, shift| {
        let channel = (((accent >> shift) & 0xff) * 8 + 16 * 92) / 100;
        color | (channel << shift)
    })
}

pub struct TimelinePanel;
pub struct SummaryPanel;

impl Panel<DisplayState> for TimelinePanel {
    fn render(&mut self, state: &DisplayState, viewport: ScreenSize, now: Instant) -> AnyElement {
        let portrait = viewport.height > viewport.width;
        let padding = 20.;
        let width = viewport.width as f32 - padding * 2.;
        let album_art_size = width.min(viewport.height as f32 * 0.25);
        let content = &state.content;
        let background = accent_background(content.accent);
        let metadata = components::metadata(
            content.title.clone(),
            content.subtitle.clone(),
            content.accent,
            !portrait,
        );
        let timeline = if content.timeline.is_empty() {
            div().flex_1().min_h_0().pt_8().overflow_hidden().child(
                components::text_line(content.body.clone(), 34., 0xc7c9d1)
                    .font_weight(gpui::FontWeight::BOLD),
            )
        } else {
            components::timeline(
                content.timeline.clone(),
                state.timeline_index_at(now),
                now,
                background,
            )
        };
        let progress =
            components::progress(state.position_at(now), content.duration, content.accent);
        let root = div()
            .size_full()
            .bg(rgb(background))
            .text_color(rgb(0xffffff))
            .font_family("Source Han Sans SC")
            .p(px(padding))
            .overflow_hidden();
        if portrait {
            root.flex()
                .flex_col()
                .gap_3()
                .child(
                    div()
                        .h(px(album_art_size + 132.))
                        .flex_shrink_0()
                        .flex()
                        .flex_col()
                        .items_center()
                        .gap_3()
                        .overflow_hidden()
                        .child(components::album_art(
                            content.album_art.clone(),
                            album_art_size,
                        ))
                        .child(metadata),
                )
                .child(timeline)
                .child(div().flex_shrink_0().pt_2().child(progress))
                .into_any_element()
        } else {
            root.flex()
                .gap_8()
                .child(
                    div()
                        .w(px((width - 32.) * 0.28))
                        .flex_shrink_0()
                        .flex()
                        .h_full()
                        .flex_col()
                        .gap_3()
                        .child(components::album_art(
                            content.album_art.clone(),
                            ((width - 32.) * 0.28 * 0.82)
                                .min((viewport.height as f32 - padding * 2.) * 0.38),
                        ))
                        .child(
                            div()
                                .h(px(104.))
                                .flex_shrink_0()
                                .overflow_hidden()
                                .child(metadata),
                        )
                        .child(div().flex_1())
                        .child(div().flex_shrink_0().child(progress)),
                )
                .child(timeline)
                .into_any_element()
        }
    }
}

impl Panel<DisplayState> for SummaryPanel {
    fn render(&mut self, state: &DisplayState, _: ScreenSize, now: Instant) -> AnyElement {
        let content = &state.content;
        let position = state.position_at(now);
        let background = accent_background(content.accent);
        div()
            .size_full()
            .bg(rgb(background))
            .text_color(rgb(0xffffff))
            .font_family("Source Han Sans SC")
            .p_4()
            .flex()
            .flex_col()
            .gap_4()
            .overflow_hidden()
            .child(components::album_art(content.album_art.clone(), 240.))
            .child(components::metadata(
                content.title.clone(),
                content.subtitle.clone(),
                content.accent,
                false,
            ))
            .child(
                div()
                    .text_size(px(60.))
                    .font_weight(gpui::FontWeight::BLACK)
                    .text_color(rgb(content.accent))
                    .child(format!(
                        "{:02}:{:02}",
                        position.as_secs() / 60,
                        position.as_secs() % 60
                    )),
            )
            .child(components::progress(
                position,
                content.duration,
                content.accent,
            ))
            .into_any_element()
    }
}
