use anyhow::{Context, Result};

#[derive(Debug, Clone)]
pub struct Config {
    /// Base URL of the RPOW API. Defaults to https://api.rpow2.com (the
    /// canonical/production site). Override to e.g. https://api.rpow3.com if
    /// you want to point the miner at the experimental sandbox instead.
    pub api_base: String,
    /// Cookie header value used to authenticate requests, e.g.
    /// `rpow_session=eyJ...`. The easiest way to obtain it is to run
    /// `rpow-miner login --email you@example.com`, which sends a magic link
    /// and exchanges it for the cookie.
    pub cookie: String,
    /// Number of mining worker threads. Defaults to all logical CPUs.
    pub threads: usize,
    /// Origin header value sent with API requests. Defaults to https://rpow2.com.
    pub origin: String,
    /// Optional User-Agent override.
    pub user_agent: String,
    /// HTTP port for the status endpoint. Defaults to $PORT or 8080.
    pub status_port: u16,
    /// Whether to bind the status server (set to false to disable).
    pub status_enabled: bool,
}

/// Read an env var, accepting either the canonical `RPOW_*` name or the
/// `RPOW2_*` alias (the same project is hosted at rpow2.com, so users often
/// reach for that prefix). The two names are equivalent.
fn env_var(canonical: &str, legacy: &str) -> Option<String> {
    std::env::var(canonical)
        .ok()
        .or_else(|| std::env::var(legacy).ok())
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let cookie = env_var("RPOW_COOKIE", "RPOW2_COOKIE")
            .context(
                "RPOW_COOKIE env var is required. Run `rpow-miner login --email YOU@example.com` \
                 to obtain it, or copy the `rpow_session` cookie from DevTools.",
            )?
            .trim()
            .to_string();
        if cookie.is_empty() {
            anyhow::bail!("RPOW_COOKIE is empty");
        }

        let api_base = env_var("RPOW_API_BASE", "RPOW2_API_BASE")
            .unwrap_or_else(|| "https://api.rpow2.com".to_string())
            .trim_end_matches('/')
            .to_string();

        let origin = env_var("RPOW_ORIGIN", "RPOW2_ORIGIN")
            .unwrap_or_else(|| "https://rpow2.com".to_string());

        let user_agent = env_var("RPOW_USER_AGENT", "RPOW2_USER_AGENT").unwrap_or_else(|| {
            "rpow-miner/0.1 (+https://github.com/) reqwest".to_string()
        });

        let threads = match env_var("RPOW_THREADS", "RPOW2_THREADS") {
            Some(v) => v
                .parse::<usize>()
                .context("RPOW_THREADS must be a positive integer")?,
            None => num_cpus::get(),
        };
        let threads = threads.max(1);

        let status_port = std::env::var("PORT")
            .ok()
            .or_else(|| env_var("RPOW_STATUS_PORT", "RPOW2_STATUS_PORT"))
            .and_then(|v| v.parse::<u16>().ok())
            .unwrap_or(8080);

        let status_enabled = env_var("RPOW_STATUS_DISABLED", "RPOW2_STATUS_DISABLED")
            .map(|v| v != "1" && !v.eq_ignore_ascii_case("true"))
            .unwrap_or(true);

        Ok(Self {
            api_base,
            cookie,
            threads,
            origin,
            user_agent,
            status_port,
            status_enabled,
        })
    }
}
