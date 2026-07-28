//! Authentication middleware.
//!
//! `Authorization: Bearer sk_test_...` — secret keys only. Publishable keys
//! (`pk_test_`) authenticate the checkout flow through a separate, far more
//! restricted path (v-checkout milestone) and are explicitly rejected here
//! with a message explaining why, because sending a publishable key to the
//! server API is the single most common integration mistake.
//!
//! Lookup: deterministic SHA-256 of the token against the unique index on
//! `api_keys.secret_hash`. A revoked key simply doesn't resolve (the query
//! filters `revoked_at IS NULL`), so revocation is instant.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::header::AUTHORIZATION;
use axum::middleware::Next;
use axum::response::Response;
use domain::api_key::Scope;
use domain::id::{ApiKeyId, MerchantId};
use time::OffsetDateTime;

use crate::error::ApiError;
use crate::state::AppState;

/// The authenticated caller, inserted into request extensions by the
/// middleware and read by handlers via `Extension<AuthCtx>`.
#[derive(Debug, Clone)]
pub struct AuthCtx {
    pub merchant_id: MerchantId,
    pub key_id: ApiKeyId,
    pub scopes: Vec<Scope>,
}

impl AuthCtx {
    /// Handlers call this first. A key without the scope gets a 403 that names
    /// the missing scope — actionable, without leaking anything else.
    pub fn require(&self, scope: Scope) -> Result<(), ApiError> {
        if self.scopes.contains(&scope) {
            Ok(())
        } else {
            Err(ApiError::forbidden(format!(
                "This API key does not have the '{}' scope.",
                scope.as_str()
            )))
        }
    }
}

pub async fn require_auth(
    State(state): State<Arc<AppState>>,
    mut req: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let token = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or_else(|| {
            ApiError::unauthorized("Missing API key. Send 'Authorization: Bearer sk_test_...'.")
        })?
        .trim()
        .to_string();

    if token.starts_with("pk_test_") {
        return Err(ApiError::unauthorized(
            "Publishable keys cannot call the server API. Use your secret key (sk_test_...) — and never expose it in browser code.",
        ));
    }
    if !token.starts_with("sk_test_") {
        return Err(ApiError::unauthorized("Invalid API key format."));
    }

    let hash = crypto::api_key::lookup_hash(&token);
    let key = store::api_key::find_active_by_hash(&state.pool, &hash)
        .await
        .map_err(|_| ApiError::unauthorized("Invalid API key."))?;

    // Best-effort usage stamp — must never fail or slow the request.
    let pool = state.pool.clone();
    let key_id = key.id;
    let now: OffsetDateTime = state.clock.now();
    tokio::spawn(async move {
        let _ = store::api_key::touch_last_used(&pool, key_id, now).await;
    });

    req.extensions_mut().insert(AuthCtx {
        merchant_id: key.merchant_id,
        key_id: key.id,
        scopes: key.scopes,
    });

    Ok(next.run(req).await)
}