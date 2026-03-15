use crate::domain::Market;
use std::sync::Arc;

/// Port for connecting to per-market live bid/ask price streams.
#[async_trait::async_trait]
pub trait MarketPricePort: Send + Sync {
    /// Connect to the price stream for a specific market.
    /// Blocks until the first price update is received.
    async fn connect(&self, market: &Market);

    /// Get the current market snapshot with live bid/ask data.
    fn current_market(&self) -> Option<Arc<Market>>;

    /// Disconnect from the current market's price stream.
    fn disconnect(&self);
}
