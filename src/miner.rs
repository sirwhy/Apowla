use anyhow::{anyhow, Context, Result};
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tokio::sync::oneshot;

use crate::api::Challenge;
use crate::stats::Stats;

/// Count trailing zero bits of a SHA-256 digest, matching the reference
/// implementation in the rpow2 web miner worker. The byte at index N-1 is
/// treated as the least-significant byte; within each byte the LSB is bit 0.
#[inline]
pub fn trailing_zero_bits(digest: &[u8; 32]) -> u32 {
    let mut count = 0u32;
    for &b in digest.iter().rev() {
        if b == 0 {
            count += 8;
            continue;
        }
        return count + b.trailing_zeros();
    }
    count
}

#[derive(Debug, Clone)]
pub struct Solution {
    pub nonce: u64,
    pub trailing_bits: u32,
    pub elapsed: Duration,
}

/// Outcome of a single mining attempt.
#[derive(Debug)]
pub enum SolveOutcome {
    /// A nonce satisfying the difficulty requirement was found before the
    /// deadline elapsed.
    Found(Solution),
    /// The configured deadline (typically derived from the challenge TTL
    /// minus a safety buffer) elapsed before any worker found a solution.
    /// The caller should drop this challenge and request a fresh one.
    DeadlineReached { hashes: u64, elapsed: Duration },
    /// The outer cancel flag was raised (graceful shutdown). The supervisor
    /// should exit.
    Cancelled,
}

/// Solve the given challenge using `threads` worker threads. Each worker
/// searches a disjoint subset of the 64-bit nonce space (worker `i` of `n`
/// tries nonces `i, i+n, i+2n, ...`), computing SHA-256 in a tight loop and
/// stopping as soon as any worker finds a nonce whose SHA-256 digest has at
/// least `difficulty_bits` trailing zero bits.
///
/// `deadline`, if `Some`, bounds how long mining will run before giving up
/// (used to abort before the server-side challenge expiry of 5 minutes —
/// see `apps/server/src/routes/challenge.ts`).
pub async fn solve(
    challenge: Challenge,
    threads: usize,
    stats: Arc<Stats>,
    cancel: Arc<AtomicBool>,
    deadline: Option<Instant>,
) -> Result<SolveOutcome> {
    let prefix = hex::decode(&challenge.nonce_prefix)
        .with_context(|| format!("decoding nonce_prefix hex: {:?}", challenge.nonce_prefix))?;
    let difficulty = challenge.difficulty_bits;
    if difficulty == 0 {
        return Err(anyhow!("server returned difficulty_bits=0"));
    }

    stats
        .current_difficulty
        .store(difficulty as u64, Ordering::Relaxed);

    let stop = Arc::new(AtomicBool::new(false));
    let solution: Arc<Mutex<Option<Solution>>> = Arc::new(Mutex::new(None));
    let (tx, rx) = oneshot::channel::<Solution>();
    let tx = Arc::new(Mutex::new(Some(tx)));

    let started = Instant::now();
    let prefix = Arc::new(prefix);

    let mut handles = Vec::with_capacity(threads);
    for worker_id in 0..threads {
        let prefix = Arc::clone(&prefix);
        let stop = Arc::clone(&stop);
        let solution = Arc::clone(&solution);
        let stats = Arc::clone(&stats);
        let tx = Arc::clone(&tx);
        let n_workers = threads as u64;

        let handle = thread::Builder::new()
            .name(format!("rpow-miner-{worker_id}"))
            .spawn(move || {
                worker_loop(
                    worker_id as u64,
                    n_workers,
                    &prefix,
                    difficulty,
                    started,
                    stop,
                    solution,
                    stats,
                    tx,
                );
            })
            .context("spawning miner worker thread")?;
        handles.push(handle);
    }

    // Watcher: stop workers when (a) the outer cancel flag flips, or
    // (b) the deadline elapses. We track which one tripped so we can return
    // the right SolveOutcome.
    let stop_for_watch = Arc::clone(&stop);
    let cancel_for_watch = Arc::clone(&cancel);
    let deadline_hit = Arc::new(AtomicBool::new(false));
    let deadline_hit_for_watch = Arc::clone(&deadline_hit);
    let cancel_watcher = tokio::spawn(async move {
        loop {
            if cancel_for_watch.load(Ordering::Relaxed) {
                stop_for_watch.store(true, Ordering::Relaxed);
                return;
            }
            if let Some(d) = deadline {
                if Instant::now() >= d {
                    deadline_hit_for_watch.store(true, Ordering::Relaxed);
                    stop_for_watch.store(true, Ordering::Relaxed);
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    });

    // Wait for any worker to find a solution. If `stop` is raised by the
    // watcher (cancel/deadline) all workers exit, the senders are dropped,
    // and `rx.await` resolves to `Err`.
    let result = rx.await;

    // Tell remaining workers to stop and join them.
    stop.store(true, Ordering::Relaxed);
    for h in handles {
        let _ = h.join();
    }
    cancel_watcher.abort();

    let elapsed = started.elapsed();
    let hashes = stats.total_hashes.load(Ordering::Relaxed);
    match result {
        Ok(sol) => Ok(SolveOutcome::Found(sol)),
        Err(_) => {
            if cancel.load(Ordering::Relaxed) {
                Ok(SolveOutcome::Cancelled)
            } else if deadline_hit.load(Ordering::Relaxed) {
                Ok(SolveOutcome::DeadlineReached { hashes, elapsed })
            } else {
                Err(anyhow!("all worker threads exited without finding a solution"))
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn worker_loop(
    worker_id: u64,
    n_workers: u64,
    prefix: &[u8],
    difficulty: u32,
    started: Instant,
    stop: Arc<AtomicBool>,
    solution: Arc<Mutex<Option<Solution>>>,
    stats: Arc<Stats>,
    tx: Arc<Mutex<Option<oneshot::Sender<Solution>>>>,
) {
    // Pre-build a buffer that we'll mutate the trailing 8 bytes of for each nonce.
    let mut buf = Vec::with_capacity(prefix.len() + 8);
    buf.extend_from_slice(prefix);
    buf.extend_from_slice(&[0u8; 8]);
    let nonce_offset = prefix.len();

    // Each worker iterates: nonce = worker_id, worker_id + n_workers, ...
    // Wraps at u64 boundary.
    let mut nonce = worker_id;
    let mut local_hashes: u64 = 0;
    const FLUSH_EVERY: u64 = 1 << 14; // flush hash counter every ~16K hashes

    loop {
        if stop.load(Ordering::Relaxed) {
            stats.add_hashes(local_hashes);
            return;
        }

        // Run a batch without checking the stop flag, for tight inner loop perf.
        let batch = 1u64 << 12; // 4096
        for _ in 0..batch {
            // Encode nonce in 8 little-endian bytes at the tail of the buffer.
            buf[nonce_offset..nonce_offset + 8].copy_from_slice(&nonce.to_le_bytes());

            let digest = Sha256::digest(&buf);
            let arr: [u8; 32] = digest.into();
            let tz = trailing_zero_bits(&arr);
            if tz >= difficulty {
                let elapsed = started.elapsed();
                let sol = Solution {
                    nonce,
                    trailing_bits: tz,
                    elapsed,
                };
                {
                    let mut guard = solution.lock().unwrap();
                    if guard.is_none() {
                        *guard = Some(sol.clone());
                    }
                }
                if let Some(sender) = tx.lock().unwrap().take() {
                    let _ = sender.send(sol);
                }
                stop.store(true, Ordering::Relaxed);
                local_hashes += 1; // this nonce counted
                stats.add_hashes(local_hashes);
                return;
            }

            local_hashes += 1;
            // Advance to this worker's next nonce slot. Wrapping_add keeps us
            // safe at the u64 boundary (in practice we'd never get there).
            nonce = nonce.wrapping_add(n_workers);
        }

        if local_hashes >= FLUSH_EVERY {
            stats.add_hashes(local_hashes);
            local_hashes = 0;
        }
    }
}

/// Helper used by tests / sanity checks: compute SHA-256 of `prefix || nonce_le8`
/// and return the trailing zero bit count.
#[allow(dead_code)]
pub fn check_nonce(prefix: &[u8], nonce: u64) -> (u32, [u8; 32]) {
    let mut buf = Vec::with_capacity(prefix.len() + 8);
    buf.extend_from_slice(prefix);
    buf.extend_from_slice(&nonce.to_le_bytes());
    let digest: [u8; 32] = Sha256::digest(&buf).into();
    (trailing_zero_bits(&digest), digest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trailing_zero_bits_examples() {
        // All zeros -> 256 trailing zero bits.
        let z = [0u8; 32];
        assert_eq!(trailing_zero_bits(&z), 256);

        // Last byte = 0x01 -> 0 trailing zeros.
        let mut a = [0u8; 32];
        a[31] = 0x01;
        assert_eq!(trailing_zero_bits(&a), 0);

        // Last byte = 0x02 -> 1 trailing zero.
        let mut b = [0u8; 32];
        b[31] = 0x02;
        assert_eq!(trailing_zero_bits(&b), 1);

        // Last two bytes zero, third-from-last = 0x01 -> 16 trailing zeros.
        let mut c = [0u8; 32];
        c[29] = 0x01;
        assert_eq!(trailing_zero_bits(&c), 16);

        // Last byte zero, second-to-last = 0xFC -> 8 + 2 = 10 trailing zeros.
        let mut d = [0u8; 32];
        d[31] = 0x00;
        d[30] = 0xFC;
        assert_eq!(trailing_zero_bits(&d), 10);
    }

    #[test]
    fn check_nonce_known_value() {
        // SHA-256(8 zero bytes) is well-known. We verify both the digest and
        // that trailing_zero_bits agrees with the JS implementation's output
        // for that digest. Last byte is 0xfc (binary 11111100) -> 2 trailing
        // zero bits.
        let (tz, digest) = check_nonce(&[], 0);
        let expected =
            hex::decode("af5570f5a1810b7af78caf4bc70a660f0df51e42baf91d4de5b2328de0e83dfc")
                .unwrap();
        assert_eq!(digest.to_vec(), expected);
        assert_eq!(tz, 2);
    }

    #[test]
    fn solver_finds_low_difficulty_quickly() {
        // Trivial difficulty (1 trailing zero bit) must always be solvable.
        let prefix = b"unit-test-prefix";
        let mut nonce: u64 = 0;
        loop {
            let (tz, _) = check_nonce(prefix, nonce);
            if tz >= 1 {
                break;
            }
            nonce += 1;
            assert!(nonce < 10_000, "should find a 1-bit solution quickly");
        }
    }
}

