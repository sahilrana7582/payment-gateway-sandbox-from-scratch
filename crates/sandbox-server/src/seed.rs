//! `cargo run -p sandbox-server -- seed`
//!
//! Creates (or reuses) the demo merchant and mints a fresh sk/pk key pair.
//! The plaintext keys are printed HERE and never retrievable again — only
//! their lookup hashes are stored. Running seed repeatedly mints additional
//! keys for the same merchant; old ones keep working until revoked.

use domain::api_key::{ApiKey, ApiKeyKind};
use domain::clock::{Clock, SystemClock};
use domain::merchant::Merchant;

use crate::config::Config;

const DEMO_EMAIL: &str = "demo@sandbox.local";

pub async fn run(config: &Config) -> anyhow::Result<()> {
    let pool = store::pool::connect(&store::pool::PoolConfig::new(&config.database_url)).await?;
    store::pool::run_migrations(&pool).await?;

    let now = SystemClock.now();

    // Reuse the demo merchant if it exists; create it otherwise.
    let merchant = match store::merchant::find_by_email(&pool, DEMO_EMAIL).await {
        Ok(existing) => {
            println!("Using existing demo merchant.");
            existing
        }
        Err(_) => {
            let m = Merchant::new("Demo Merchant", DEMO_EMAIL, now)
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            store::merchant::insert(&pool, &m).await?;
            println!("Created demo merchant.");
            m
        }
    };

    let sk = crypto::api_key::generate("sk_test")?;
    let pk = crypto::api_key::generate("pk_test")?;

    store::api_key::insert(
        &pool,
        &ApiKey::new(merchant.id, ApiKeyKind::Secret, "seeded", &sk.hash, now),
    )
    .await?;
    store::api_key::insert(
        &pool,
        &ApiKey::new(
            merchant.id,
            ApiKeyKind::Publishable,
            "seeded",
            &pk.hash,
            now,
        ),
    )
    .await?;

    println!();
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("  Merchant   {}", merchant.id);
    println!();
    println!("  Secret key      {}", sk.plaintext);
    println!("  Publishable key {}", pk.plaintext);
    println!();
    println!("  These are shown ONCE. Only their hashes are stored.");
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!();
    println!("Try it (server must be running):");
    println!();
    println!(r#"  curl -s -X POST localhost:{}/v1/orders \"#, config.port);
    println!(r#"    -H "Authorization: Bearer {}" \"#, sk.plaintext);
    println!(r#"    -H "Content-Type: application/json" \"#);
    println!(r#"    -d '{{"amount": 150000, "currency": "INR", "receipt": "demo_1"}}'"#);
    println!();
    println!("  # then pay it (use the order id from the response):");
    println!();
    println!(
        r#"  curl -s -X POST localhost:{}/v1/payments \"#,
        config.port
    );
    println!(r#"    -H "Authorization: Bearer {}" \"#, sk.plaintext);
    println!(r#"    -H "Content-Type: application/json" \"#);
    println!(
        r#"    -d '{{"order_id": "<ORDER_ID>", "card": {{"number": "4242424242424242", "exp_month": 12, "exp_year": 2030, "cvc": "123"}}}}'"#
    );
    println!();
    println!("Test cards: 4242424242424242 succeeds · 4000000000009995 insufficient funds");
    println!("            4000000000000077 authorize-only · full table in the docs");
    println!();

    Ok(())
}
