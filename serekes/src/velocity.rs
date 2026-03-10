use std::time::{Instant, Duration};

pub struct VelocityLockout {
    last_price: f64,
    last_update: Instant,
    lockout_until: Option<Instant>,
    threshold_pct: f64,
    duration: Duration,
}

impl VelocityLockout {
    pub fn new(threshold_pct: f64, duration_secs: u64) -> Self {
        Self {
            last_price: 0.0,
            last_update: Instant::now(),
            lockout_until: None,
            threshold_pct,
            duration: Duration::from_secs(duration_secs),
        }
    }

    pub fn update(&mut self, current_price: f64) {
        let now = Instant::now();

        if self.last_price > 0.0 {
            let pct_move = (current_price - self.last_price).abs() / self.last_price;
            let elapsed = now.duration_since(self.last_update);

            if pct_move > self.threshold_pct && elapsed < Duration::from_secs(3) {
                println!("[VELOCITY] Lockout triggered! Move: {:.4}% in {:.2}s", pct_move * 100.0, elapsed.as_secs_f64());
                self.lockout_until = Some(now + self.duration);
            }
        }

        self.last_price = current_price;
        self.last_update = now;
    }

    pub fn is_locked(&self) -> bool {
        if let Some(until) = self.lockout_until {
            if Instant::now() < until {
                return true;
            }
        }
        false
    }
}
