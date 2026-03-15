use crate::domain::OrderIntent;
use crate::ports::exchange::{ExchangePort, OrderResult};
use log::warn;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use tokio::task::JoinHandle;
use tokio::time::{Duration, interval};

/// Dual token-bucket rate limiter that decorates an ExchangePort.
/// Non-blocking: returns Err immediately if either bucket is empty.
pub struct RateLimitedExchange {
    inner: Arc<dyn ExchangePort>,
    burst: Arc<TokenBucket>,
    sustained: Arc<TokenBucket>,
    _burst_task: JoinHandle<()>,
    _sustained_task: JoinHandle<()>,
}

impl RateLimitedExchange {
    /// Wrap an exchange port with dual token-bucket rate limiting.
    ///
    /// Polymarket POST /order limits:
    ///   burst:     3,500 requests per 10 seconds
    ///   sustained: 36,000 requests per 10 minutes
    pub fn new(inner: Arc<dyn ExchangePort>) -> Self {
        let burst = Arc::new(TokenBucket::new(3_500));
        let sustained = Arc::new(TokenBucket::new(36_000));

        // Refill burst: 3,500 tokens over 10s => 1 token every ~2.857ms
        let burst_clone = burst.clone();
        let burst_task = tokio::spawn(async move {
            // Refill 350 tokens every second (equivalent rate, less overhead)
            let mut tick = interval(Duration::from_secs(1));
            loop {
                tick.tick().await;
                burst_clone.refill(350);
            }
        });

        // Refill sustained: 36,000 tokens over 10min => 60 tokens per second
        let sustained_clone = sustained.clone();
        let sustained_task = tokio::spawn(async move {
            let mut tick = interval(Duration::from_secs(1));
            loop {
                tick.tick().await;
                sustained_clone.refill(60);
            }
        });

        Self {
            inner,
            burst,
            sustained,
            _burst_task: burst_task,
            _sustained_task: sustained_task,
        }
    }
}

#[async_trait::async_trait]
impl ExchangePort for RateLimitedExchange {
    async fn submit_order(
        &self,
        token_id: &str,
        intent: &OrderIntent,
    ) -> Result<OrderResult, String> {
        if !self.burst.try_acquire() || !self.sustained.try_acquire() {
            warn!("Order rate limited");
            return Err("rate limited".into());
        }
        self.inner.submit_order(token_id, intent).await
    }

    fn is_paper_mode(&self) -> bool {
        self.inner.is_paper_mode()
    }
}

struct TokenBucket {
    remaining: AtomicU32,
    capacity: u32,
}

impl TokenBucket {
    fn new(capacity: u32) -> Self {
        Self {
            remaining: AtomicU32::new(capacity),
            capacity,
        }
    }

    /// Try to consume one token. Returns false if bucket is empty.
    fn try_acquire(&self) -> bool {
        loop {
            let current = self.remaining.load(Ordering::Relaxed);
            if current == 0 {
                return false;
            }
            if self
                .remaining
                .compare_exchange_weak(current, current - 1, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                return true;
            }
        }
    }

    /// Add tokens back, capped at capacity.
    fn refill(&self, count: u32) {
        loop {
            let current = self.remaining.load(Ordering::Relaxed);
            let new = (current + count).min(self.capacity);
            if self
                .remaining
                .compare_exchange_weak(current, new, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                return;
            }
        }
    }
}
