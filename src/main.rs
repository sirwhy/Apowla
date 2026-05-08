mod api;
mod config;
mod miner;
mod server;
mod stats;

use anyhow::Result;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::signal;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use crate::api::ApiCallError;
use crate::config::Config;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    init_tracing();

    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("login") => return run_login(&args[2..]).await,
        Some("--help") | Some("-h") | Some("help") => {
            print_usage();
            return Ok(());
        }
        Some("--version") | Some("-V") => {
            println!("rpow-miner {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        _ => {}
    }

    if std::env::var("RPOW_SELFTEST")
        .or_else(|_| std::env::var("RPOW2_SELFTEST"))
        .as_deref()
        == Ok("1")
    {
        return run_selftest().await;
    }

    let cfg = Config::from_env()?;
    info!(
        api_base = %cfg.api_base,
        threads = cfg.threads,
        status_enabled = cfg.status_enabled,
        status_port = cfg.status_port,
        "starting rpow-miner"
    );

    let api = api::ApiClient::new(&cfg)?;

    // Verify the configured cookie is valid before we start mining; if the
    // user is unauthenticated the server returns 401 from /me and we can fail
    // fast with a useful message.
    match api.me().await {
        Ok(me) => {
            info!(
                email = me.email.unwrap_or_default(),
                balance = me.balance.unwrap_or(0),
                minted = me.minted.unwrap_or(0),
                "authenticated"
            );
        }
        Err(ApiCallError::Unauthorized(msg)) => {
            error!(
                "session cookie rejected by /me ({msg}). Run `rpow-miner login --email YOU@example.com` \
                 to obtain a fresh cookie (or log in at https://rpow2.com and copy the `rpow_session` \
                 cookie from DevTools -> Application -> Cookies), then set RPOW_COOKIE."
            );
            std::process::exit(2);
        }
        Err(e) => {
            warn!("/me probe failed: {e}. Will continue and let the mining loop retry.");
        }
    }

    let stats = stats::Stats::new();
    let cancel = Arc::new(AtomicBool::new(false));

    // Status HTTP server (best-effort).
    if cfg.status_enabled {
        let stats = Arc::clone(&stats);
        let port = cfg.status_port;
        tokio::spawn(async move {
            if let Err(e) = server::serve(stats, port).await {
                warn!("status server stopped: {e}");
            }
        });
    }

    // Periodic heartbeat logger.
    {
        let stats = Arc::clone(&stats);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(30));
            interval.tick().await; // skip the first immediate tick
            loop {
                interval.tick().await;
                let s = stats.snapshot();
                let mh = s.hashrate_per_sec / 1_000_000.0;
                info!(
                    "[stats] uptime={:.0}s hashes={} hashrate={:.2}MH/s minted={} mint_failures={} deadline_misses={} difficulty={}",
                    s.uptime_secs,
                    s.total_hashes,
                    mh,
                    s.tokens_minted,
                    s.mint_failures,
                    s.deadline_misses,
                    s.current_difficulty,
                );
            }
        });
    }

    // Spawn the mining supervisor.
    let cancel_for_mine = Arc::clone(&cancel);
    let stats_for_mine = Arc::clone(&stats);
    let mine_task = tokio::spawn(async move {
        mining_supervisor(cfg, api, stats_for_mine, cancel_for_mine).await;
    });

    // Wait for shutdown signal.
    wait_for_shutdown().await;
    info!("shutdown signal received; stopping mining loop...");
    cancel.store(true, Ordering::Relaxed);

    // Give the supervisor a moment to wind down.
    let _ = tokio::time::timeout(Duration::from_secs(5), mine_task).await;
    info!("bye");
    Ok(())
}

fn init_tracing() {
    let filter = EnvFilter::try_from_env("RPOW_LOG")
        .or_else(|_| EnvFilter::try_from_env("RPOW2_LOG"))
        .or_else(|_| EnvFilter::try_from_default_env())
        .unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_level(true)
        .compact()
        .init();
}

async fn wait_for_shutdown() {
    let ctrl_c = async {
        let _ = signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut sig) = signal::unix::signal(signal::unix::SignalKind::terminate()) {
            sig.recv().await;
        } else {
            std::future::pending::<()>().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = term => {},
    }
}

async fn mining_supervisor(
    cfg: Config,
    api: api::ApiClient,
    stats: Arc<stats::Stats>,
    cancel: Arc<AtomicBool>,
) {
    let mut consecutive_errors: u32 = 0;

    // Server-side challenge TTL (see frkrueger/rpow apps/server/src/routes/challenge.ts:54
    // `new Date(Date.now() + 5 * 60 * 1000)`). We bail a configurable number
    // of seconds before that so the /mint roundtrip lands inside the window.
    let challenge_ttl = Duration::from_secs(
        std::env::var("RPOW_CHALLENGE_TTL_SECS")
            .or_else(|_| std::env::var("RPOW2_CHALLENGE_TTL_SECS"))
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(300),
    );
    let deadline_buffer = Duration::from_secs(
        std::env::var("RPOW_DEADLINE_BUFFER_SECS")
            .or_else(|_| std::env::var("RPOW2_DEADLINE_BUFFER_SECS"))
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(15),
    );
    let solve_budget = challenge_ttl.saturating_sub(deadline_buffer);

    while !cancel.load(Ordering::Relaxed) {
        // 1) Fetch a challenge. Capture the moment we got it so we can
        //    compute the local deadline relative to our own clock.
        let challenge = match api.challenge().await {
            Ok(c) => {
                consecutive_errors = 0;
                stats.challenges_fetched.fetch_add(1, Ordering::Relaxed);
                info!(
                    challenge_id = %c.challenge_id,
                    nonce_prefix_len_bytes = c.nonce_prefix.len() / 2,
                    difficulty_bits = c.difficulty_bits,
                    expires_at = c.expires_at.as_deref().unwrap_or("?"),
                    solve_budget_secs = solve_budget.as_secs(),
                    "received challenge"
                );
                c
            }
            Err(ApiCallError::Unauthorized(msg)) => {
                error!(
                    "challenge request rejected as unauthorized: {msg}. Update RPOW_COOKIE \
                     and restart. Sleeping 60s before retry."
                );
                sleep_or_cancel(Duration::from_secs(60), &cancel).await;
                continue;
            }
            Err(e) => {
                consecutive_errors = consecutive_errors.saturating_add(1);
                let backoff = backoff_for(consecutive_errors);
                warn!(
                    "challenge request failed ({e}); backing off {backoff:?} before retry"
                );
                sleep_or_cancel(backoff, &cancel).await;
                continue;
            }
        };

        let challenge_id = challenge.challenge_id.clone();
        let difficulty = challenge.difficulty_bits;
        let deadline = Some(Instant::now() + solve_budget);

        // 2) Solve, with a deadline so we abort before the server-side
        //    challenge expiry rather than wasting time on an already-dead
        //    challenge.
        let solution = match miner::solve(
            challenge,
            cfg.threads,
            Arc::clone(&stats),
            Arc::clone(&cancel),
            deadline,
        )
        .await
        {
            Ok(miner::SolveOutcome::Found(s)) => s,
            Ok(miner::SolveOutcome::Cancelled) => return,
            Ok(miner::SolveOutcome::DeadlineReached { hashes, elapsed }) => {
                let mh = (hashes as f64) / elapsed.as_secs_f64() / 1_000_000.0;
                warn!(
                    challenge_id = %challenge_id,
                    difficulty_bits = difficulty,
                    elapsed_ms = elapsed.as_millis() as u64,
                    hashes = hashes,
                    hashrate_mh = format!("{mh:.2}"),
                    "challenge solve budget elapsed without finding a nonce; \
                     dropping it and requesting a fresh one (current difficulty \
                     is too high for this hashrate to reliably solve within the \
                     5-minute server TTL — consider scaling up cores)"
                );
                stats.deadline_misses.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            Err(e) => {
                if cancel.load(Ordering::Relaxed) {
                    return;
                }
                warn!("solve failed: {e}; fetching a fresh challenge");
                continue;
            }
        };

        info!(
            challenge_id = %challenge_id,
            nonce = solution.nonce,
            trailing_bits = solution.trailing_bits,
            difficulty_bits = difficulty,
            elapsed_ms = solution.elapsed.as_millis() as u64,
            "FOUND solution; submitting to /mint"
        );
        stats
            .last_solution_ms
            .store(solution.elapsed.as_millis() as u64, Ordering::Relaxed);

        // 3) Submit the proof.
        let nonce_str = solution.nonce.to_string();
        match api.mint(&challenge_id, &nonce_str).await {
            Ok(resp) => {
                stats.tokens_minted.fetch_add(1, Ordering::Relaxed);
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                stats.last_token_ts.store(now, Ordering::Relaxed);
                info!(token_id = %resp.token.id, "minted token");
                consecutive_errors = 0;
            }
            Err(ApiCallError::Unauthorized(msg)) => {
                error!(
                    "mint rejected as unauthorized: {msg}. Update RPOW_COOKIE and restart. \
                     Sleeping 60s."
                );
                stats.mint_failures.fetch_add(1, Ordering::Relaxed);
                sleep_or_cancel(Duration::from_secs(60), &cancel).await;
            }
            Err(e) => {
                stats.mint_failures.fetch_add(1, Ordering::Relaxed);
                consecutive_errors = consecutive_errors.saturating_add(1);
                let backoff = backoff_for(consecutive_errors);
                warn!("mint failed ({e}); backing off {backoff:?} before next challenge");
                sleep_or_cancel(backoff, &cancel).await;
            }
        }
    }
    info!("mining supervisor exited cleanly");
}

fn print_usage() {
    println!(
        "rpow-miner {ver}\n\n\
USAGE:\n\
    rpow-miner [SUBCOMMAND]\n\n\
SUBCOMMANDS:\n\
    (none)            Run the mining loop (default). Requires RPOW_COOKIE.\n\
    login [--email E] Interactive: request a magic link, exchange it for a\n\
                      session cookie, and print the value to set as\n\
                      RPOW_COOKIE in your environment / Railway variables.\n\
                      No account? This is also how you register — first\n\
                      magic-link verification creates the account.\n\
    --help / -h       Show this help.\n\
    --version / -V    Show version.\n\n\
ENVIRONMENT (mining mode):\n\
    RPOW_COOKIE       Required. Cookie header value, e.g. 'rpow_session=...'\n\
    RPOW_API_BASE     Default https://api.rpow2.com\n\
    RPOW_THREADS      Default = all logical CPUs\n\
    RPOW_LOG          Default = info (tracing filter)\n\
    PORT              Status HTTP port (default 8080)\n\n\
ENVIRONMENT (login mode):\n\
    RPOW_LOGIN_EMAIL  Skip the email prompt and use this address.\n\
    RPOW_API_BASE / RPOW_ORIGIN as above\n\n\
ALIASES: Every RPOW_* variable also accepts the equivalent RPOW2_* name\n\
    (e.g. RPOW_COOKIE === RPOW2_COOKIE). Use whichever you prefer.\n\n\
EXAMPLES:\n\
    rpow-miner login --email me@example.com\n\
    RPOW_COOKIE='rpow_session=eyJ...' rpow-miner\n",
        ver = env!("CARGO_PKG_VERSION")
    );
}

/// Interactive register/login flow. No account is required up-front: the
/// rpow2 server auto-creates the user on first magic-link verification, so
/// this is also the registration path.
///
/// Steps:
///   1. POST /auth/request {email} -> server emails a magic link.
///   2. User opens the link from their inbox and pastes the URL back here.
///   3. We GET that URL once with redirects disabled, capturing the
///      `rpow_session` cookie from the Set-Cookie header.
///   4. We print the resulting cookie line so the user can paste it into
///      Railway's `RPOW_COOKIE` variable (or any env file).
async fn run_login(extra_args: &[String]) -> Result<()> {
    use std::io::{BufRead, Write};

    let mut email_arg: Option<String> = None;
    let mut iter = extra_args.iter();
    while let Some(a) = iter.next() {
        match a.as_str() {
            "--email" | "-e" => {
                email_arg = iter.next().cloned();
            }
            other if other.starts_with("--email=") => {
                email_arg = Some(other.trim_start_matches("--email=").to_string());
            }
            "-h" | "--help" => {
                println!(
                    "rpow-miner login [--email EMAIL]\n\n\
Sends a magic link to EMAIL, then prompts you to paste the verify URL from \n\
your inbox. Prints the resulting RPOW_COOKIE value when done."
                );
                return Ok(());
            }
            other => anyhow::bail!("unknown argument to `login`: {other}"),
        }
    }

    let api_base = std::env::var("RPOW_API_BASE")
        .or_else(|_| std::env::var("RPOW2_API_BASE"))
        .unwrap_or_else(|_| "https://api.rpow2.com".to_string())
        .trim_end_matches('/')
        .to_string();
    let origin = std::env::var("RPOW_ORIGIN")
        .or_else(|_| std::env::var("RPOW2_ORIGIN"))
        .unwrap_or_else(|_| "https://rpow2.com".to_string());
    let user_agent = std::env::var("RPOW_USER_AGENT")
        .or_else(|_| std::env::var("RPOW2_USER_AGENT"))
        .unwrap_or_else(|_| "rpow-miner/0.1 (login)".to_string());

    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();

    let email = match email_arg
        .or_else(|| std::env::var("RPOW_LOGIN_EMAIL").ok())
        .or_else(|| std::env::var("RPOW2_LOGIN_EMAIL").ok())
    {
        Some(e) => e.trim().to_string(),
        None => {
            print!("Email address: ");
            stdout.flush().ok();
            let mut line = String::new();
            stdin.lock().read_line(&mut line)?;
            line.trim().to_string()
        }
    };
    if email.is_empty() || !email.contains('@') {
        anyhow::bail!("invalid email: {email:?}");
    }

    println!();
    println!("→ Sending magic link to {email} via {api_base}/auth/request ...");

    let http = api::build_login_client(&origin, &user_agent)?;
    let resp = api::auth_request(&http, &api_base, &email).await?;
    if !resp.ok {
        anyhow::bail!("server did not acknowledge the auth request");
    }
    println!(
        "✓ Magic link sent. Open your inbox and click the link, OR copy the link\n\
   address and paste it below. The link expires in 15 minutes.\n"
    );
    println!("Paste the verification URL from your email here, then press Enter:");
    print!("> ");
    stdout.flush().ok();

    let mut url_line = String::new();
    stdin.lock().read_line(&mut url_line)?;
    let url = url_line.trim();
    if url.is_empty() {
        anyhow::bail!("no URL provided");
    }
    if !url.starts_with("https://") && !url.starts_with("http://") {
        anyhow::bail!("expected a URL starting with http(s)://; got: {url}");
    }

    println!("\n→ Exchanging magic link for a session cookie ...");
    let cookie_value = api::verify_magic_link(&http, url).await?;

    println!();
    println!("============================================================");
    println!("  LOGIN SUCCESSFUL — session cookie obtained");
    println!("============================================================");
    println!();
    println!("Set this in your shell or Railway → Variables:");
    println!();
    println!("  RPOW_COOKIE='{cookie_value}'");
    println!();
    println!("To start mining locally:");
    println!();
    println!("  export RPOW_COOKIE='{cookie_value}'");
    println!("  rpow-miner");
    println!();
    println!("Cookie is valid for ~30 days. If it expires, run `rpow-miner login` again.");
    Ok(())
}

/// Synthetic offline benchmark: solves a fixed-difficulty PoW with the same
/// algorithm the server uses (sha256(prefix || nonce_le8) trailing zero bits).
/// Used to verify the build and measure hashrate without server credentials.
async fn run_selftest() -> Result<()> {
    let threads = std::env::var("RPOW_THREADS")
        .or_else(|_| std::env::var("RPOW2_THREADS"))
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(num_cpus::get)
        .max(1);
    let difficulty: u32 = std::env::var("RPOW_SELFTEST_BITS")
        .or_else(|_| std::env::var("RPOW2_SELFTEST_BITS"))
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(22);

    info!(
        threads,
        difficulty_bits = difficulty,
        "running offline self-test (no network/auth)"
    );

    let prefix_bytes: [u8; 8] = [0xde, 0xad, 0xbe, 0xef, 0x00, 0x11, 0x22, 0x33];
    let challenge = api::Challenge {
        challenge_id: "selftest".to_string(),
        nonce_prefix: hex::encode(prefix_bytes),
        difficulty_bits: difficulty,
        expires_at: None,
    };
    let stats = stats::Stats::new();
    let cancel = Arc::new(AtomicBool::new(false));

    let started = std::time::Instant::now();
    let outcome = miner::solve(challenge, threads, Arc::clone(&stats), cancel, None).await?;
    let sol = match outcome {
        miner::SolveOutcome::Found(s) => s,
        miner::SolveOutcome::DeadlineReached { .. } | miner::SolveOutcome::Cancelled => {
            return Err(anyhow::anyhow!("self-test aborted before finding a solution"));
        }
    };
    let elapsed = started.elapsed();
    let snapshot = stats.snapshot();
    info!(
        nonce = sol.nonce,
        trailing_bits = sol.trailing_bits,
        elapsed_ms = elapsed.as_millis() as u64,
        total_hashes = snapshot.total_hashes,
        hashrate_mh_per_s = format!("{:.2}", snapshot.hashrate_per_sec / 1_000_000.0),
        "self-test SUCCESS"
    );
    Ok(())
}

fn backoff_for(consecutive_errors: u32) -> Duration {
    let secs = 1u64 << consecutive_errors.min(5); // 1,2,4,8,16,32
    Duration::from_secs(secs.min(30))
}

async fn sleep_or_cancel(dur: Duration, cancel: &Arc<AtomicBool>) {
    let deadline = tokio::time::Instant::now() + dur;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return;
        }
        let step = (deadline - now).min(Duration::from_millis(200));
        tokio::time::sleep(step).await;
    }
}
