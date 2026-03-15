/// Port for obtaining the current time (server-synced).
pub trait ClockPort: Send + Sync {
    fn now_ms(&self) -> i64;
}
