/// Summary of a completed market trading session.
pub struct MarketSummary {
    pub trades: u32,
    pub cost: f64,
    pub shares_up: f64,
    pub shares_down: f64,
}
