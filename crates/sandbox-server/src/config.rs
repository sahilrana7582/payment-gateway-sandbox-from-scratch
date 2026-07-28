//! Environment configuration. Plain env vars, no config framework — the
//! `.env.example` at the repo root documents every one of these.

use envconfig::Envconfig;

#[derive(Debug, Envconfig)]
pub struct Config {
    #[envconfig(from = "DATABASE_URL")]
    pub database_url: String,

    #[envconfig(from = "SERVER_HOST", default = "0.0.0.0")]
    pub host: String,

    #[envconfig(from = "SERVER_PORT", default = "8080")]
    pub port: u16,

    /// Server-side pepper for card fingerprints. Never stored in the DB, so a
    /// database dump alone can't attack the fingerprints.
    #[envconfig(from = "CRYPTO_CARD_FINGERPRINT_PEPPER")]
    pub fingerprint_pepper: String,

    /// KEK for envelope encryption. Loaded and validated now; used when the
    /// vault features land.
    #[envconfig(from = "CRYPTO_MASTER_KEY")]
    pub master_key: String,

    /// Sleep for the simulator's latency before responding to payment
    /// creation, so integrators see realistic timing. On by default; tests
    /// and CI turn it off.
    #[envconfig(from = "SIMULATE_LATENCY", default = "true")]
    pub simulate_latency: bool,
}

impl Config {
    /// Loud warnings for footguns; hard errors only for things that would
    /// produce silently-broken crypto.
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.fingerprint_pepper.contains("change_me") {
            tracing::warn!(
                "CRYPTO_CARD_FINGERPRINT_PEPPER is still the example value — fine for local dev, never for a shared deployment"
            );
        }
        if self.master_key.contains("change_me") {
            tracing::warn!(
                "CRYPTO_MASTER_KEY is still the example value — generate one with: openssl rand -hex 32"
            );
        }
        Ok(())
    }

    pub fn bind_addr(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}
