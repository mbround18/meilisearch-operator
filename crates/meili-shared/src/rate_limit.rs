use once_cell::sync::Lazy;
use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};

static RL_STATE: Lazy<Mutex<HashMap<String, Instant>>> = Lazy::new(|| Mutex::new(HashMap::new()));

/// Returns true if the message should be logged (i.e., not suppressed).
/// Suppresses identical keys for `window` duration.
pub fn allow(key: &str, window: Duration) -> bool {
    let mut map = RL_STATE.lock().expect("rate_limit mutex poisoned");
    let now = Instant::now();
    match map.get(key) {
        Some(last) if now.duration_since(*last) < window => false,
        _ => {
            use std::collections::hash_map::Entry;
            match map.entry(key.to_string()) {
                Entry::Occupied(mut o) => {
                    o.insert(now);
                }
                Entry::Vacant(v) => {
                    v.insert(now);
                }
            }
            true
        }
    }
}
