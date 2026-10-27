use std::{
    sync::{Arc, LazyLock},
    time::{Duration, Instant},
};

use tokio::sync::{mpsc, watch};
use tokio::time::MissedTickBehavior;

use dida_core::{DisplayCommand, DisplayContent, DisplayState, TimedText};

fn contents() -> &'static [Arc<DisplayContent>; 2] {
    static CONTENTS: LazyLock<[Arc<DisplayContent>; 2]> = LazyLock::new(|| {
        let make =
            |id: &str, title: &str, subtitle: &str, duration, accent, lines: &[(u64, &str)]| {
                Arc::new(DisplayContent {
                    id: id.into(),
                    title: title.into(),
                    subtitle: subtitle.into(),
                    body: "".into(),
                    duration: Duration::from_secs(duration),
                    album_art: None,
                    accent,
                    timeline: lines
                        .iter()
                        .map(|(second, text)| TimedText {
                            start: Duration::from_secs(*second),
                            text: (*text).into(),
                        })
                        .collect::<Vec<_>>()
                        .into(),
                })
            };
        [
            make(
                "demo-paper-lantern",
                "Paper Lantern (Demo)",
                "Dida Box Demo",
                64,
                0xF2B84B,
                &[
                    (0, "Fold a little light into the evening"),
                    (7, "Leave the window open for the rain"),
                    (14, "Every drop remembers where the river bends"),
                    (21, "And every quiet road can lead us home again"),
                    (28, "纸灯轻轻亮，照着回家的方向"),
                    (35, "We carry only what our hands can hold"),
                    (42, "A name, a note, a story never told"),
                    (49, "风吹过长街，我们慢慢走"),
                    (56, "Keep this little light until the morning"),
                ],
            ),
            make(
                "demo-open-water",
                "Open Water (Demo)",
                "Dida Box Demo",
                72,
                0x50CDB1,
                &[
                    (0, "Morning draws a silver line across the water"),
                    (8, "I set my heavy thoughts beside the shore"),
                    (16, "The boats are small but all their sails are open"),
                    (24, "There is a wider sky than we have seen before"),
                    (32, "海风吹来，把昨日轻轻放开"),
                    (40, "One step beyond the place we used to stand"),
                    (48, "We find a rhythm neither one of us had planned"),
                    (56, "浪花在唱，明天就在前方"),
                    (64, "Let the open water teach our hearts to wander"),
                ],
            ),
        ]
    });
    &CONTENTS
}

pub fn initial() -> DisplayState {
    DisplayState {
        content: contents()[0].clone(),
        position: Duration::ZERO,
        active: true,
        observed_at: Instant::now(),
    }
}

fn next(state: &mut DisplayState) {
    let index = if state.content.id == contents()[0].id {
        1
    } else {
        0
    };
    state.content = contents()[index].clone();
    state.position = Duration::ZERO;
}

fn update(state: &mut DisplayState, now: Instant, command: Option<DisplayCommand>) {
    state.position = state.position_at(now);
    state.observed_at = now;
    match command {
        Some(DisplayCommand::ToggleActive) => state.active = !state.active,
        Some(DisplayCommand::Seek(position)) => {
            state.position = position.min(state.content.duration)
        }
        Some(DisplayCommand::SeekBy(milliseconds)) => {
            let offset = Duration::from_millis(milliseconds.unsigned_abs());
            state.position = if milliseconds < 0 {
                state.position.saturating_sub(offset)
            } else {
                state
                    .position
                    .saturating_add(offset)
                    .min(state.content.duration)
            };
        }
        Some(DisplayCommand::Next) => next(state),
        None if state.active && state.position >= state.content.duration => next(state),
        None => {}
    }
}

pub async fn run(
    sender: watch::Sender<DisplayState>,
    mut commands: mpsc::Receiver<DisplayCommand>,
    mut shutdown: watch::Receiver<bool>,
) {
    if *shutdown.borrow() || sender.is_closed() {
        return;
    }
    let mut state = initial();
    if sender.send(state.clone()).is_err() {
        return;
    }
    let mut ticker = tokio::time::interval(Duration::from_millis(200));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut commands_open = true;
    loop {
        let command = tokio::select! {
            biased;
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() { break; }
                continue;
            }
            _ = sender.closed() => break,
            command = commands.recv(), if commands_open => {
                match command {
                    Some(command) => Some(command),
                    None => { commands_open = false; continue; }
                }
            }
            _ = ticker.tick() => None,
        };
        update(&mut state, Instant::now(), command);
        if sender.send(state.clone()).is_err() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_transitions_preserve_clock_and_reset_content() {
        let mut state = initial();
        let start = state.observed_at;
        update(
            &mut state,
            start + Duration::from_secs(10),
            Some(DisplayCommand::ToggleActive),
        );
        assert!(!state.active);
        assert_eq!(state.position, Duration::from_secs(10));
        update(&mut state, start + Duration::from_secs(20), None);
        assert_eq!(state.position, Duration::from_secs(10));
        update(
            &mut state,
            start + Duration::from_secs(20),
            Some(DisplayCommand::ToggleActive),
        );
        update(&mut state, start + Duration::from_secs(25), None);
        assert_eq!(state.position, Duration::from_secs(15));
        update(
            &mut state,
            start + Duration::from_secs(25),
            Some(DisplayCommand::Seek(Duration::from_secs(3))),
        );
        update(&mut state, start + Duration::from_secs(27), None);
        assert_eq!(state.position, Duration::from_secs(5));
        update(
            &mut state,
            start + Duration::from_secs(27),
            Some(DisplayCommand::SeekBy(i64::MIN)),
        );
        assert_eq!(state.position, Duration::ZERO);
        update(
            &mut state,
            start + Duration::from_secs(27),
            Some(DisplayCommand::SeekBy(i64::MAX)),
        );
        assert_eq!(state.position, state.content.duration);
        update(
            &mut state,
            start + Duration::from_secs(27),
            Some(DisplayCommand::Next),
        );
        assert_eq!(state.content.id.as_ref(), "demo-open-water");
        assert_eq!(state.position, Duration::ZERO);
        assert!(state.active);
        update(
            &mut state,
            start + Duration::from_secs(28),
            Some(DisplayCommand::ToggleActive),
        );
        update(
            &mut state,
            start + Duration::from_secs(29),
            Some(DisplayCommand::Next),
        );
        assert!(!state.active);
        assert_eq!(state.position, Duration::ZERO);
        update(
            &mut state,
            start + Duration::from_secs(30),
            Some(DisplayCommand::Seek(Duration::MAX)),
        );
        assert_eq!(state.position, state.content.duration);
        update(
            &mut state,
            start + Duration::from_secs(31),
            Some(DisplayCommand::ToggleActive),
        );
        update(&mut state, start + Duration::from_secs(31), None);
        assert_eq!(state.content.id.as_ref(), "demo-open-water");
        assert_eq!(state.position, Duration::ZERO);
    }
}
