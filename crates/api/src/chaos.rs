//! Optional degraded service conditions, selected by the host.
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

/// The service conditions the host can select at runtime.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Level {
    Off = 0,
    Normal = 3,
    Incident = 90,
}

static LEVEL: AtomicU8 = AtomicU8::new(Level::Off as u8);

/// Selects the degraded upstream conditions without restarting the server.
pub fn configure(level: Level) {
    LEVEL.store(level as u8, Ordering::Relaxed);
}

fn draw() -> Option<[u8; 2]> {
    let mut draw = [0_u8; 2];
    getrandom::fill(&mut draw).ok().map(|()| draw)
}

pub(crate) fn available() -> Result<(), crate::error::ApiError> {
    if LEVEL.load(Ordering::Relaxed) != 0 && draw().is_some_and(|draw| draw[0] < 3) {
        return Err(
            cairn_service::ServiceError::Store(cairn_store::StoreError::Backend(
                "the read connection closed before the result was available".to_owned(),
            ))
            .into(),
        );
    }
    Ok(())
}

pub(crate) async fn search_ready() {
    let level = LEVEL.load(Ordering::Relaxed);
    if level != 0
        && let Some(duration) = draw().and_then(|draw| search_delay(level, draw))
    {
        tokio::time::sleep(duration).await;
    }
}

fn search_delay(level: u8, draw: [u8; 2]) -> Option<Duration> {
    (draw[0] < level).then(|| {
        Duration::from_millis(if level == Level::Incident as u8 {
            1500 + u64::from(draw[1]) * 1000 / 255
        } else {
            1800
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_rates_and_durations_follow_the_level() {
        for (level, count) in [(Level::Off, 0), (Level::Normal, 3), (Level::Incident, 90)] {
            let delays: Vec<_> = (0..=255)
                .filter_map(|draw| search_delay(level as u8, [draw, draw]))
                .collect();
            assert_eq!(delays.len(), count);
            if level == Level::Normal {
                assert!(
                    delays
                        .iter()
                        .all(|delay| *delay == Duration::from_millis(1800))
                );
            }
        }
        assert_eq!(search_delay(90, [0, 0]), Some(Duration::from_millis(1500)));
        assert_eq!(
            search_delay(90, [0, 255]),
            Some(Duration::from_millis(2500))
        );
    }
}
