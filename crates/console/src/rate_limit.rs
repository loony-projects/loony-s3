use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Most clients the limiter tracks at once. Past this, new clients share one allowance, so
/// someone cycling through addresses can't grow this without limit.
const MAX_TRACKED: usize = 10_000;

const OVERFLOW_KEY: &str = "\0overflow";

/// Allows so many attempts per client per window. Per process rather than per cluster,
/// which is enough to slow someone down.
pub struct RateLimiter {
    limit: u32,
    window: Duration,
    clients: Mutex<HashMap<String, (Instant, u32)>>,
}

impl RateLimiter {
    pub fn new(limit: u32, window: Duration) -> Self {
        Self {
            limit,
            window,
            clients: Mutex::new(HashMap::new()),
        }
    }

    pub fn allow(&self, client: &str) -> bool {
        self.allow_at(client, Instant::now())
    }

    fn allow_at(&self, client: &str, now: Instant) -> bool {
        let mut clients = self.clients.lock().expect("rate limiter lock poisoned");

        if clients.len() >= MAX_TRACKED && !clients.contains_key(client) {
            clients.retain(|_, (started, _)| now.duration_since(*started) < self.window);
        }
        let key = if clients.len() >= MAX_TRACKED && !clients.contains_key(client) {
            OVERFLOW_KEY
        } else {
            client
        };

        let entry = clients.entry(key.to_string()).or_insert((now, 0));
        if now.duration_since(entry.0) >= self.window {
            *entry = (now, 0);
        }
        if entry.1 >= self.limit {
            return false;
        }
        entry.1 += 1;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOW: Duration = Duration::from_secs(60);

    #[test]
    fn stops_a_client_at_the_limit_until_the_window_passes() {
        let limiter = RateLimiter::new(2, WINDOW);
        let start = Instant::now();
        assert!(limiter.allow_at("a", start));
        assert!(limiter.allow_at("a", start));
        assert!(!limiter.allow_at("a", start));
        assert!(
            limiter.allow_at("b", start),
            "clients are counted separately"
        );
        assert!(limiter.allow_at("a", start + WINDOW));
    }

    #[test]
    fn a_flood_of_clients_shares_one_allowance_and_stays_bounded() {
        let limiter = RateLimiter::new(1, WINDOW);
        let start = Instant::now();
        for i in 0..MAX_TRACKED {
            assert!(limiter.allow_at(&format!("client-{i}"), start));
        }
        assert!(limiter.allow_at("one-more", start));
        assert!(!limiter.allow_at("and-another", start));
        assert!(limiter.clients.lock().unwrap().len() <= MAX_TRACKED + 1);

        // Once the old entries are out of date, there's room for new clients again.
        assert!(limiter.allow_at("later", start + WINDOW));
        assert!(limiter.clients.lock().unwrap().len() < 10);
    }
}
