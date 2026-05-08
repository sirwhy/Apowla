use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

#[derive(Debug)]
pub struct Stats {
    pub started: Instant,
    pub total_hashes: AtomicU64,
    pub challenges_fetched: AtomicU64,
    pub tokens_minted: AtomicU64,
    pub mint_failures: AtomicU64,
    pub deadline_misses: AtomicU64,
    pub current_difficulty: AtomicU64,
    pub last_solution_ms: AtomicU64,
    pub last_token_ts: AtomicU64,
}

impl Stats {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            started: Instant::now(),
            total_hashes: AtomicU64::new(0),
            challenges_fetched: AtomicU64::new(0),
            tokens_minted: AtomicU64::new(0),
            mint_failures: AtomicU64::new(0),
            deadline_misses: AtomicU64::new(0),
            current_difficulty: AtomicU64::new(0),
            last_solution_ms: AtomicU64::new(0),
            last_token_ts: AtomicU64::new(0),
        })
    }

    pub fn add_hashes(&self, n: u64) {
        self.total_hashes.fetch_add(n, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> StatsSnapshot {
        let uptime_secs = self.started.elapsed().as_secs_f64();
        let total = self.total_hashes.load(Ordering::Relaxed);
        let hashrate = if uptime_secs > 0.0 {
            total as f64 / uptime_secs
        } else {
            0.0
        };
        StatsSnapshot {
            uptime_secs,
            total_hashes: total,
            hashrate_per_sec: hashrate,
            challenges_fetched: self.challenges_fetched.load(Ordering::Relaxed),
            tokens_minted: self.tokens_minted.load(Ordering::Relaxed),
            mint_failures: self.mint_failures.load(Ordering::Relaxed),
            deadline_misses: self.deadline_misses.load(Ordering::Relaxed),
            current_difficulty: self.current_difficulty.load(Ordering::Relaxed),
            last_solution_ms: self.last_solution_ms.load(Ordering::Relaxed),
            last_token_unix_ts: self.last_token_ts.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, serde::Serialize)]
pub struct StatsSnapshot {
    pub uptime_secs: f64,
    pub total_hashes: u64,
    pub hashrate_per_sec: f64,
    pub challenges_fetched: u64,
    pub tokens_minted: u64,
    pub mint_failures: u64,
    pub deadline_misses: u64,
    pub current_difficulty: u64,
    pub last_solution_ms: u64,
    pub last_token_unix_ts: u64,
}
