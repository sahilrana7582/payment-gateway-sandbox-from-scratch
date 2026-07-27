// Merchant persistence. This is the reference repository — every other one
// follows the same shape:
//   - functions are free-standing, taking an executor (`&PgPool` for now)
//   - `sqlx::query_as!` verifies SQL against the live DB at compile time
//   - a private row struct maps rows -> domain types in ONE place
//   - domain types cross the boundary; rows never leak upward
//   - `?` converts `sqlx::Error` into `StoreError` via the From impl
//
// ============================================================================
// FUTURE REFACTOR — executor-generic reads (do this when you build `engine`)
// ============================================================================
// Right now every function takes `&PgPool` concretely. That means these
// functions can ONLY run against the pool directly — they CANNOT participate
// in a larger transaction.
//
// The moment `engine` needs to read a merchant as part of a multi-step
// transaction (e.g. "lock the order, read the merchant, post the ledger, all
// or nothing"), a `&PgPool` parameter will not work — you'd be reading OUTSIDE
// the transaction, defeating its purpose.
//
// THE FIX (apply to every read function — insert/update that must be
// transactional too): make them generic over the executor.
//
//     pub async fn find_by_id<'e, E>(executor: E, id: MerchantId) -> StoreResult<Merchant>
//     where
//         E: sqlx::PgExecutor<'e>,
//     {
//         let row = sqlx::query_as!(MerchantRow, "...", id.as_uuid())
//             .fetch_optional(executor)   // <-- executor, not pool
//             .await?
//             ...
//     }
//
// Then the SAME function works with BOTH:
//     find_by_id(&pool, id).await          // standalone read
//     find_by_id(&mut *tx, id).await       // read inside a transaction
//
// Retrofitting this later is mechanical but touches every function, so when
// you see this comment while wiring up `engine`, that's your signal to do it.
// ============================================================================

use domain::id::MerchantId;
use domain::merchant::Merchant;
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{StoreError, StoreResult};

/// Private mirror of a `merchants` row. Exists so the row -> domain mapping is
/// written EXACTLY ONCE (`into_domain`) instead of being copy-pasted into every
/// query function. Add a column to the table? You update this struct and one
/// conversion, not four scattered constructors.
///
/// `query_as!` populates this by matching column names to field names, and
/// verifies at compile time that the SELECT returns exactly these columns with
/// compatible types.
struct MerchantRow {
    id: Uuid,
    name: String,
    email: String,
    checkout_allowed_domains: Vec<String>,
    created_at: OffsetDateTime,
}

impl MerchantRow {
    fn into_domain(self) -> Merchant {
        Merchant {
            id: MerchantId::from_uuid(self.id),
            name: self.name,
            email: self.email,
            checkout_allowed_domains: self.checkout_allowed_domains,
            created_at: self.created_at,
        }
    }
}

/// Insert a new merchant. Returns `Conflict` if the email already exists (the
/// unique index on `lower(email)` enforces it).
pub async fn insert(pool: &PgPool, merchant: &Merchant) -> StoreResult<()> {
    sqlx::query!(
        r#"
        INSERT INTO merchants (id, name, email, checkout_allowed_domains, created_at)
        VALUES ($1, $2, $3, $4, $5)
        "#,
        merchant.id.as_uuid(),
        merchant.name,
        merchant.email,
        &merchant.checkout_allowed_domains,
        merchant.created_at,
    )
    .execute(pool)
    .await?;

    Ok(())
}

/// Fetch a merchant by id. Returns `NotFound` if absent.
pub async fn find_by_id(pool: &PgPool, id: MerchantId) -> StoreResult<Merchant> {
    let row = sqlx::query_as!(
        MerchantRow,
        r#"
        SELECT id, name, email, checkout_allowed_domains, created_at
        FROM merchants
        WHERE id = $1
        "#,
        id.as_uuid(),
    )
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| StoreError::not_found("merchant"))?;

    Ok(row.into_domain())
}

/// Fetch by email (case-insensitive). Used at signup to detect duplicates
/// before attempting the insert, for a friendlier error than a raw conflict.
pub async fn find_by_email(pool: &PgPool, email: &str) -> StoreResult<Merchant> {
    let row = sqlx::query_as!(
        MerchantRow,
        r#"
        SELECT id, name, email, checkout_allowed_domains, created_at
        FROM merchants
        WHERE lower(email) = lower($1)
        "#,
        email,
    )
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| StoreError::not_found("merchant"))?;

    Ok(row.into_domain())
}

/// Update the checkout domain allowlist. The whole array is replaced, matching
/// the domain method's idempotent add semantics upstream.
pub async fn update_checkout_domains(
    pool: &PgPool,
    id: MerchantId,
    domains: &[String],
) -> StoreResult<()> {
    let affected = sqlx::query!(
        r#"
        UPDATE merchants
        SET checkout_allowed_domains = $2
        WHERE id = $1
        "#,
        id.as_uuid(),
        domains,
    )
    .execute(pool)
    .await?
    .rows_affected();

    if affected == 0 {
        return Err(StoreError::not_found("merchant"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    //! These tests hit a real Postgres via `#[sqlx::test]`, which spins up an
    //! isolated database per test and runs migrations automatically. They only
    //! execute when DATABASE_URL is set and the DB is reachable.
    use super::*;
    use domain::merchant::Merchant;
    use time::macros::datetime;

    fn sample() -> Merchant {
        Merchant::new(
            "Acme Co",
            "billing@acme.dev",
            datetime!(2026-01-01 00:00:00 UTC),
        )
        .unwrap()
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn insert_then_find_by_id(pool: PgPool) {
        let m = sample();
        insert(&pool, &m).await.unwrap();

        let found = find_by_id(&pool, m.id).await.unwrap();
        assert_eq!(found.name, "Acme Co");
        assert_eq!(found.email, "billing@acme.dev");
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn find_missing_returns_not_found(pool: PgPool) {
        let err = find_by_id(&pool, MerchantId::new()).await.unwrap_err();
        assert!(err.is_not_found());
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn duplicate_email_is_a_conflict(pool: PgPool) {
        let m1 = sample();
        insert(&pool, &m1).await.unwrap();

        // Same email, different id.
        let m2 = Merchant {
            id: MerchantId::new(),
            ..sample()
        };
        let err = insert(&pool, &m2).await.unwrap_err();
        assert!(err.is_conflict());
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn find_by_email_is_case_insensitive(pool: PgPool) {
        let m = sample();
        insert(&pool, &m).await.unwrap();
        let found = find_by_email(&pool, "BILLING@ACME.DEV").await.unwrap();
        assert_eq!(found.id, m.id);
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn update_domains_persists(pool: PgPool) {
        let m = sample();
        insert(&pool, &m).await.unwrap();

        let domains = vec!["https://shop.acme.dev".to_string()];
        update_checkout_domains(&pool, m.id, &domains)
            .await
            .unwrap();

        let found = find_by_id(&pool, m.id).await.unwrap();
        assert_eq!(found.checkout_allowed_domains, domains);
    }
}
