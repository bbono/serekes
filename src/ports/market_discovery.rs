use crate::domain::Market;

/// Port for discovering active markets on an exchange.
#[async_trait::async_trait]
pub trait MarketDiscoveryPort: Send + Sync {
    /// Discover and return the next active market. Retries internally until found.
    async fn discover(&self, asset: &str, interval_minutes: u32) -> Market;
}
