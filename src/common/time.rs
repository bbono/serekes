use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static POLYMARKET_TIME_OFFSET_MS: AtomicI64 = AtomicI64::new(i64::MIN);

pub fn now_ms() -> i64 {
    let offset = POLYMARKET_TIME_OFFSET_MS.load(Ordering::Relaxed);
    if offset == i64::MIN {
        panic!("now_ms() called before Polymarket time offset was fetched");
    }
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
        + offset
}

pub fn store_time_offset(offset_ms: i64) {
    POLYMARKET_TIME_OFFSET_MS.store(offset_ms, Ordering::Relaxed);
}
