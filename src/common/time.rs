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

pub async fn fetch_time_offset_ms() -> i64 {
    use log::{debug, error, warn};
    use polymarket_client_sdk::clob::{Client as ClobClient, Config as ClobConfig};

    let clob = ClobClient::new("https://clob.polymarket.com", ClobConfig::builder().build()).unwrap();
    let local_ms = || {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64
    };
    match clob.server_time().await {
        Ok(server_ts) => {
            let offset_ms: i64 = (server_ts * 1000) - local_ms();
            store_time_offset(offset_ms);
            debug!(
                "Time sync: server={}s local={}ms offset={}ms",
                server_ts,
                local_ms(),
                offset_ms
            );
            if offset_ms.abs() > 300_000 {
                warn!(
                    "Large time skew detected! Adjusting timestamps by {}ms",
                    offset_ms
                );
            }
            offset_ms
        }
        Err(e) => {
            error!("Failed to sync time: {}. Defaulting to 0 offset.", e);
            0
        }
    }
}
