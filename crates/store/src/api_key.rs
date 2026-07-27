//! API key persistence. The hot path here is `find_active_by_hash`, called by
//! the auth middleware on every authenticated request — so it's a single
//! indexed lookup, and it excludes revoked keys in the query rather than
//! fetching then checking in Rust.
//!
//! Follows the same shape as `merchant.rs` (the reference repository): a
//! private row struct maps rows -> domain types in ONE place, so adding a
//! column means updating `ApiKeyRow`/`into_domain` once instead of every
//! query function that reads one.

use domain::api_key::{ApiKey, ApiKeyKind};
use domain::id::{ApiKeyId, MerchantId};
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{StoreError, StoreResult};
use crate::types::scopes;

/// Private mirror of an `api_keys` row. See `MerchantRow` in `merchant.rs` for
/// why this exists: one place to map columns -> domain fields, instead of a
/// copy-pasted struct literal in every read function.
struct ApiKeyRow {
    id: Uuid,
    merchant_id: Uuid,
    kind: ApiKeyKind,
    label: String,
    scopes: Vec<String>,
    secret_hash: String,
    last_used_at: Option<OffsetDateTime>,
    revoked_at: Option<OffsetDateTime>,
    created_at: OffsetDateTime,
}

impl ApiKeyRow {
    fn into_domain(self) -> ApiKey {
        ApiKey {
            id: ApiKeyId::from_uuid(self.id),
            merchant_id: MerchantId::from_uuid(self.merchant_id),
            kind: self.kind,
            label: self.label,
            scopes: scopes::from_strings(&self.scopes),
            secret_hash: self.secret_hash,
            last_used_at: self.last_used_at,
            revoked_at: self.revoked_at,
            created_at: self.created_at,
        }
    }
}

/// The Postgres enum `api_key_kind` maps to `ApiKeyKind` via sqlx::Type on the
/// domain enum. We bind it directly.
pub async fn insert(pool: &PgPool, key: &ApiKey) -> StoreResult<()> {
    sqlx::query!(
        r#"
        INSERT INTO api_keys
            (id, merchant_id, kind, label, scopes, secret_hash,
             last_used_at, revoked_at, created_at)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
        "#,
        key.id.as_uuid(),
        key.merchant_id.as_uuid(),
        key.kind as ApiKeyKind,
        key.label,
        &scopes::to_strings(&key.scopes),
        key.secret_hash,
        key.last_used_at,
        key.revoked_at,
        key.created_at,
    )
    .execute(pool)
    .await?;

    Ok(())
}

/// Auth hot path: given the Argon2id hash of the presented secret, return the
/// key ONLY if it exists and is not revoked. The `revoked_at IS NULL` predicate
/// means a revoked key simply doesn't resolve — no separate active-check needed.
pub async fn find_active_by_hash(pool: &PgPool, secret_hash: &str) -> StoreResult<ApiKey> {
    let row = sqlx::query_as!(
        ApiKeyRow,
        r#"
        SELECT id, merchant_id, kind AS "kind: ApiKeyKind", label, scopes,
               secret_hash, last_used_at, revoked_at, created_at
        FROM api_keys
        WHERE secret_hash = $1 AND revoked_at IS NULL
        "#,
        secret_hash,
    )
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| StoreError::not_found("api_key"))?;

    Ok(row.into_domain())
}

/// List a merchant's keys (for the dashboard). Revoked keys included, so the
/// merchant can see their history.
pub async fn list_for_merchant(pool: &PgPool, merchant_id: MerchantId) -> StoreResult<Vec<ApiKey>> {
    let rows = sqlx::query_as!(
        ApiKeyRow,
        r#"
        SELECT id, merchant_id, kind AS "kind: ApiKeyKind", label, scopes,
               secret_hash, last_used_at, revoked_at, created_at
        FROM api_keys
        WHERE merchant_id = $1
        ORDER BY created_at DESC
        "#,
        merchant_id.as_uuid(),
    )
    .fetch_all(pool)
    .await?;

    Ok(rows.into_iter().map(ApiKeyRow::into_domain).collect())
}

/// Mark a key revoked. Scoped to `merchant_id` as well as `id` — a caller can
/// never revoke another tenant's key by guessing or being handed a wrong id,
/// even if the id-vs-merchant check upstream has a bug. Returns `NotFound` if
/// no row matches both, so a bad id doesn't silently no-op. Idempotent on an
/// already-revoked key: `COALESCE` keeps the original timestamp rather than
/// overwriting it, but the row still counts as matched/affected.
pub async fn revoke(
    pool: &PgPool,
    merchant_id: MerchantId,
    id: ApiKeyId,
    now: OffsetDateTime,
) -> StoreResult<()> {
    let affected = sqlx::query!(
        r#"
        UPDATE api_keys
        SET revoked_at = COALESCE(revoked_at, $3)
        WHERE id = $1 AND merchant_id = $2
        "#,
        id.as_uuid(),
        merchant_id.as_uuid(),
        now,
    )
    .execute(pool)
    .await?
    .rows_affected();

    if affected == 0 {
        return Err(StoreError::not_found("api_key"));
    }
    Ok(())
}

/// Record last-used timestamp. Best-effort; the auth path calls this but does
/// not fail the request if it errors.
pub async fn touch_last_used(pool: &PgPool, id: ApiKeyId, now: OffsetDateTime) -> StoreResult<()> {
    sqlx::query!(
        r#"UPDATE api_keys SET last_used_at = $2 WHERE id = $1"#,
        id.as_uuid(),
        now,
    )
    .execute(pool)
    .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::api_key::Scope;
    use domain::merchant::Merchant;
    use time::macros::datetime;

    async fn seed_merchant(pool: &PgPool) -> MerchantId {
        let m = Merchant::new("Acme", "a@acme.dev", datetime!(2026-01-01 00:00:00 UTC)).unwrap();
        crate::merchant::insert(pool, &m).await.unwrap();
        m.id
    }

    async fn seed_merchant_2(pool: &PgPool) -> MerchantId {
        let m =
            Merchant::new("Acme", "a@acme.runtiva", datetime!(2026-01-01 00:00:00 UTC)).unwrap();
        crate::merchant::insert(pool, &m).await.unwrap();
        m.id
    }

    fn key(merchant_id: MerchantId, hash: &str) -> ApiKey {
        ApiKey::new(
            merchant_id,
            ApiKeyKind::Secret,
            "default",
            hash,
            datetime!(2026-01-01 00:00:00 UTC),
        )
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn insert_then_resolve_by_hash(pool: PgPool) {
        let mid = seed_merchant(&pool).await;
        let k = key(mid, "hash_abc");
        insert(&pool, &k).await.unwrap();

        let found = find_active_by_hash(&pool, "hash_abc").await.unwrap();
        assert_eq!(found.id, k.id);
        assert!(found.has_scope(Scope::PaymentsWrite));
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn revoked_key_does_not_resolve(pool: PgPool) {
        let mid = seed_merchant(&pool).await;
        let k = key(mid, "hash_xyz");
        insert(&pool, &k).await.unwrap();

        revoke(&pool, mid, k.id, datetime!(2026-02-01 00:00:00 UTC))
            .await
            .unwrap();

        let err = find_active_by_hash(&pool, "hash_xyz").await.unwrap_err();
        assert!(err.is_not_found());
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn revoke_is_idempotent(pool: PgPool) {
        let mid = seed_merchant(&pool).await;
        let k = key(mid, "hash_idem");
        insert(&pool, &k).await.unwrap();

        revoke(&pool, mid, k.id, datetime!(2026-02-01 00:00:00 UTC))
            .await
            .unwrap();
        // Second revoke on an already-revoked key succeeds without moving the
        // timestamp, and without erroring as "not found".
        revoke(&pool, mid, k.id, datetime!(2026-06-01 00:00:00 UTC))
            .await
            .unwrap();

        let all = list_for_merchant(&pool, mid).await.unwrap();
        assert_eq!(all[0].revoked_at, Some(datetime!(2026-02-01 00:00:00 UTC)));
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn revoke_missing_key_is_not_found(pool: PgPool) {
        let mid = seed_merchant(&pool).await;
        let err = revoke(
            &pool,
            mid,
            ApiKeyId::new(),
            datetime!(2026-02-01 00:00:00 UTC),
        )
        .await
        .unwrap_err();
        assert!(err.is_not_found());
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn revoke_cannot_cross_tenants(pool: PgPool) {
        let owner = seed_merchant(&pool).await;
        let k = key(owner, "hash_tenant");
        insert(&pool, &k).await.unwrap();

        // A different merchant id must not be able to revoke this key.
        let other = seed_merchant_2(&pool).await;
        let err = revoke(&pool, other, k.id, datetime!(2026-02-01 00:00:00 UTC))
            .await
            .unwrap_err();
        assert!(err.is_not_found());

        // It's still active under its real owner.
        assert!(find_active_by_hash(&pool, "hash_tenant").await.is_ok());
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn scopes_survive_roundtrip(pool: PgPool) {
        let mid = seed_merchant(&pool).await;
        let k = key(mid, "hash_scopes");
        insert(&pool, &k).await.unwrap();

        let found = find_active_by_hash(&pool, "hash_scopes").await.unwrap();
        // Secret keys get full access by default.
        assert_eq!(found.scopes.len(), Scope::full_access().len());
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn list_includes_revoked(pool: PgPool) {
        let mid = seed_merchant(&pool).await;
        let k = key(mid, "hash_list");
        insert(&pool, &k).await.unwrap();
        revoke(&pool, mid, k.id, datetime!(2026-02-01 00:00:00 UTC))
            .await
            .unwrap();

        let all = list_for_merchant(&pool, mid).await.unwrap();
        assert_eq!(all.len(), 1);
        assert!(all[0].is_revoked());
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn touch_last_used_persists(pool: PgPool) {
        let mid = seed_merchant(&pool).await;
        let k = key(mid, "hash_touch");
        insert(&pool, &k).await.unwrap();

        touch_last_used(&pool, k.id, datetime!(2026-03-01 00:00:00 UTC))
            .await
            .unwrap();

        let found = find_active_by_hash(&pool, "hash_touch").await.unwrap();
        assert_eq!(found.last_used_at, Some(datetime!(2026-03-01 00:00:00 UTC)));
    }
}
