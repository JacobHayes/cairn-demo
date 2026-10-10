//! Optional degraded service conditions, selected once at startup by the host.
use std::sync::atomic::{AtomicBool, Ordering};

static ENABLED: AtomicBool = AtomicBool::new(false);

/// Selects whether the server exercises degraded upstream conditions.
pub fn configure(enabled: bool) {
    ENABLED.store(enabled, Ordering::Relaxed);
}

fn selected() -> bool {
    if !ENABLED.load(Ordering::Relaxed) {
        return false;
    }
    let mut draw = [0_u8; 1];
    getrandom::fill(&mut draw).is_ok() && draw[0] < 3
}

pub(crate) fn available() -> Result<(), crate::error::ApiError> {
    if selected() {
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
    if selected() {
        tokio::time::sleep(std::time::Duration::from_millis(1800)).await;
    }
}
