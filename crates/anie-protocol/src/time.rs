use std::time::{SystemTime, UNIX_EPOCH};

/// Return the current time in milliseconds since the Unix epoch.
pub fn now_millis() -> u64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_millis() as u64,
        Err(_) => 0,
    }
}
