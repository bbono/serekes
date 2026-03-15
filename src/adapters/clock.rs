use crate::ports::ClockPort;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Adapter that provides Polymarket-server-synced time.
pub struct PolymarketClock {
    offset_ms: AtomicI64,
}

impl PolymarketClock {
    pub fn new() -> Self {
        Self {
            offset_ms: AtomicI64::new(i64::MIN),
        }
    }

    /// Fetch the time offset from the Polymarket CLOB server and store it.
    /// Retries up to 3 times, then exits if sync is not possible.
    pub async fn sync(&self) {
        use log::{debug, error, warn};
        use polymarket_client_sdk::clob::{Client as ClobClient, Config as ClobConfig};

        let clob =
            ClobClient::new("https://clob.polymarket.com", ClobConfig::builder().build()).unwrap();
        let local_ms = || {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis() as i64
        };

        for attempt in 1..=3 {
            match clob.server_time().await {
                Ok(server_ts) => {
                    let offset_ms: i64 = (server_ts * 1000) - local_ms();
                    self.offset_ms.store(offset_ms, Ordering::Relaxed);
                    debug!(
                        "Time sync: server={}s local={}ms offset={}ms",
                        server_ts,
                        local_ms(),
                        offset_ms
                    );
                    if offset_ms.abs() > 300_000 {
                        warn!("Large time skew: offset={offset_ms}ms");
                    }
                    return;
                }
                Err(e) => {
                    warn!("Time sync failed (attempt {}/3): {e}", attempt);
                    if attempt < 3 {
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    }
                }
            }
        }

        error!("Time sync failed after 3 attempts. Exiting.");
        std::process::exit(1);
    }
}

impl ClockPort for PolymarketClock {
    fn now_ms(&self) -> i64 {
        let offset = self.offset_ms.load(Ordering::Relaxed);
        if offset == i64::MIN {
            panic!("now_ms() called before Polymarket time offset was fetched");
        }
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64
            + offset
    }
}
