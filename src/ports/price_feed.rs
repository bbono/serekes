/// Port for reading live price data from an external price feed.
pub trait PriceFeedPort: Send + Sync {
    /// Get the latest price and its timestamp in ms.
    fn latest(&self) -> (f64, i64);

    /// Whether the feed has received at least one valid price.
    fn is_ready(&self) -> bool;

    /// Look up a historical price by timestamp.
    /// If `exact` is true, requires an exact timestamp match;
    /// otherwise returns the latest price at or before `target_ms`.
    /// Returns 0.0 if not found. Default: no history support.
    fn lookup_history(&self, _target_ms: i64, _exact: bool) -> f64 {
        0.0
    }
}
