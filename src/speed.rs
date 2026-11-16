//! Token-bucket bandwidth limiter shared by every connection.

use std::time::Duration;
use tokio::sync::Mutex;
use std::time::Instant;

struct State {
    tokens: f64,
    last: Instant,
}

pub struct SpeedLimiter {
    rate: u64,
    state: Mutex<State>,
}

impl SpeedLimiter {
    pub fn new(rate: u64) -> Self {
        Self {
            rate,
            state: Mutex::new(State {
                // start with one second of credit so small downloads are not throttled
                tokens: rate as f64,
                last: Instant::now(),
            }),
        }
    }

    pub fn rate(&self) -> u64 {
        self.rate
    }

    /// Wait until `n` bytes are allowed to proceed.
    pub async fn acquire(&self, n: usize) {
        if self.rate == 0 || n == 0 {
            return;
        }
        let need = n as f64;
        loop {
            let sleep_for = {
                let mut st = self.state.lock().await;
                let now = Instant::now();
                let elapsed = now.duration_since(st.last).as_secs_f64();
                st.tokens = (st.tokens + elapsed * self.rate as f64).min(self.rate as f64);
                st.last = now;
                if st.tokens >= need {
                    st.tokens -= need;
                    return;
                }
                let missing = need - st.tokens;
                Duration::from_secs_f64((missing / self.rate as f64).clamp(0.001, 1.0))
            };
            tokio::time::sleep(sleep_for).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unlimited_is_instant() {
        let limiter = SpeedLimiter::new(0);
        let start = Instant::now();
        limiter.acquire(10 * 1024 * 1024).await;
        assert!(start.elapsed() < Duration::from_millis(50));
    }

    #[tokio::test]
    async fn limited_transfer_is_throttled() {
        // 64 KB/s bucket, one second of credit -> a 256 KB burst must wait
        let limiter = SpeedLimiter::new(64 * 1024);
        let start = Instant::now();
        limiter.acquire(64 * 1024).await; // uses the initial credit
        limiter.acquire(64 * 1024).await; // must wait ~1s
        assert!(start.elapsed() >= Duration::from_millis(500));
    }
}
