//! `credentials`: encrypted per-user secrets. The store only ever sees
//! ciphertext; encryption lives in `kiln-vault`.

use crate::row::{RowExt, enum_str, now, to_json, ts};
use crate::{Result, Store, StoreError};
use kiln_types::{
    CredentialHealth, CredentialId, CredentialKind, CredentialSummary, Timestamp, UserId,
};
use sqlx::sqlite::SqliteRow;

/// A credential to insert. The id is chosen by the caller because it is part
/// of the AEAD associated data.
#[derive(Clone)]
pub struct NewCredential {
    pub id: CredentialId,
    pub user_id: UserId,
    pub kind: CredentialKind,
    pub name: String,
    pub ciphertext: Vec<u8>,
    pub nonce: Vec<u8>,
    pub masked: String,
    pub base_url: Option<String>,
    pub env_var: Option<String>,
    pub expires_at: Option<Timestamp>,
}

impl std::fmt::Debug for NewCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NewCredential")
            .field("id", &self.id)
            .field("user_id", &self.user_id)
            .field("kind", &self.kind)
            .field("name", &self.name)
            .field("masked", &self.masked)
            .finish_non_exhaustive()
    }
}

/// Encrypted material of one credential, as needed to decrypt it.
#[derive(Clone)]
pub struct CredentialSecret {
    pub kind: CredentialKind,
    pub ciphertext: Vec<u8>,
    pub nonce: Vec<u8>,
    pub base_url: Option<String>,
    pub env_var: Option<String>,
}

impl std::fmt::Debug for CredentialSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialSecret")
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

/// An active credential with its ciphertext, for dispatch.
#[derive(Clone)]
pub struct StoredCredential {
    pub id: CredentialId,
    pub user_id: UserId,
    pub kind: CredentialKind,
    pub name: String,
    pub ciphertext: Vec<u8>,
    pub nonce: Vec<u8>,
    pub base_url: Option<String>,
    pub env_var: Option<String>,
    pub created_at: Timestamp,
}

impl std::fmt::Debug for StoredCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoredCredential")
            .field("id", &self.id)
            .field("kind", &self.kind)
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

fn summary_from_row(r: &SqliteRow) -> Result<CredentialSummary> {
    Ok(CredentialSummary {
        id: r.id("id")?,
        kind: r.enumv("kind")?,
        name: r.str("name")?,
        masked: r.str("masked")?,
        base_url: r.opt_str("base_url")?,
        env_var: r.opt_str("env_var")?,
        status: r.json("health_json")?,
        created_at: r.time("created_at")?,
        expires_at: r.opt_time("expires_at")?,
    })
}

/// Columns safe to return to clients (no ciphertext/nonce).
const SUMMARY_COLS: &str =
    "id, kind, name, masked, base_url, env_var, health_json, created_at, expires_at";

impl Store {
    /// Insert an encrypted credential. Health starts as `Untested`.
    pub async fn insert_credential(&self, new: NewCredential) -> Result<CredentialSummary> {
        if new.name.trim().is_empty() {
            return Err(StoreError::Invalid("credential name is empty".into()));
        }
        sqlx::query(
            "INSERT INTO credentials (id, user_id, kind, name, ciphertext, nonce, masked, base_url, env_var,
                                      health_json, created_at, expires_at, revoked_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL)",
        )
        .bind(new.id.to_string())
        .bind(new.user_id.to_string())
        .bind(enum_str(&new.kind))
        .bind(new.name.trim())
        .bind(&new.ciphertext)
        .bind(&new.nonce)
        .bind(&new.masked)
        .bind(&new.base_url)
        .bind(&new.env_var)
        .bind(to_json(&CredentialHealth::Untested)?)
        .bind(now())
        .bind(new.expires_at.as_ref().map(ts))
        .execute(&self.pool)
        .await
        .map_err(|e| crate::conflict_on_unique(e, "credential"))?;
        self.get_credential(new.user_id, new.id).await
    }

    /// One non-revoked credential of `user`, without its secret.
    pub async fn get_credential(
        &self,
        user: UserId,
        id: CredentialId,
    ) -> Result<CredentialSummary> {
        let sql = format!(
            "SELECT {SUMMARY_COLS} FROM credentials WHERE id = ? AND user_id = ? AND revoked_at IS NULL"
        );
        let row = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(id.to_string())
            .bind(user.to_string())
            .fetch_optional(&self.pool)
            .await?
            .ok_or(StoreError::NotFound)?;
        summary_from_row(&row)
    }

    /// The user's non-revoked credentials, never including secrets.
    pub async fn list_credentials(&self, user: UserId) -> Result<Vec<CredentialSummary>> {
        let sql = format!(
            "SELECT {SUMMARY_COLS} FROM credentials WHERE user_id = ? AND revoked_at IS NULL ORDER BY created_at, id"
        );
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(user.to_string())
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(summary_from_row).collect()
    }

    /// Ciphertext + nonce of one of `user`'s credentials. `None` if it does
    /// not exist, belongs to someone else, or is revoked.
    pub async fn get_credential_secret(
        &self,
        user: UserId,
        id: CredentialId,
    ) -> Result<Option<CredentialSecret>> {
        let row = sqlx::query(
            "SELECT kind, ciphertext, nonce, base_url, env_var FROM credentials
             WHERE id = ? AND user_id = ? AND revoked_at IS NULL",
        )
        .bind(id.to_string())
        .bind(user.to_string())
        .fetch_optional(&self.pool)
        .await?;
        row.map(|r| {
            Ok(CredentialSecret {
                kind: r.enumv("kind")?,
                ciphertext: r.blob("ciphertext")?,
                nonce: r.blob("nonce")?,
                base_url: r.opt_str("base_url")?,
                env_var: r.opt_str("env_var")?,
            })
        })
        .transpose()
    }

    /// Every credential of `user` that is neither revoked nor expired, oldest
    /// first (so the first credential of a kind wins on dispatch).
    pub async fn active_credentials_for_dispatch(
        &self,
        user: UserId,
    ) -> Result<Vec<StoredCredential>> {
        let rows = sqlx::query(
            "SELECT * FROM credentials
             WHERE user_id = ? AND revoked_at IS NULL AND (expires_at IS NULL OR expires_at > ?)
             ORDER BY created_at, id",
        )
        .bind(user.to_string())
        .bind(now())
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| {
                Ok(StoredCredential {
                    id: r.id("id")?,
                    user_id: r.id("user_id")?,
                    kind: r.enumv("kind")?,
                    name: r.str("name")?,
                    ciphertext: r.blob("ciphertext")?,
                    nonce: r.blob("nonce")?,
                    base_url: r.opt_str("base_url")?,
                    env_var: r.opt_str("env_var")?,
                    created_at: r.time("created_at")?,
                })
            })
            .collect()
    }

    /// Replace a credential's secret (and optionally its base URL). Health is
    /// reset to `Untested`.
    pub async fn rotate_credential(
        &self,
        user: UserId,
        id: CredentialId,
        ciphertext: &[u8],
        nonce: &[u8],
        masked: &str,
        base_url: Option<&str>,
    ) -> Result<CredentialSummary> {
        let r = sqlx::query(
            "UPDATE credentials SET ciphertext = ?, nonce = ?, masked = ?, base_url = COALESCE(?, base_url),
                                    health_json = ?
             WHERE id = ? AND user_id = ? AND revoked_at IS NULL",
        )
        .bind(ciphertext)
        .bind(nonce)
        .bind(masked)
        .bind(base_url)
        .bind(to_json(&CredentialHealth::Untested)?)
        .bind(id.to_string())
        .bind(user.to_string())
        .execute(&self.pool)
        .await?;
        if r.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        self.get_credential(user, id).await
    }

    /// Revoke a credential. The ciphertext is wiped; the row stays as an
    /// audit trail.
    pub async fn revoke_credential(&self, user: UserId, id: CredentialId) -> Result<()> {
        let r = sqlx::query(
            "UPDATE credentials SET revoked_at = ?, ciphertext = X'', nonce = X''
             WHERE id = ? AND user_id = ? AND revoked_at IS NULL",
        )
        .bind(now())
        .bind(id.to_string())
        .bind(user.to_string())
        .execute(&self.pool)
        .await?;
        if r.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    /// Record the result of a credential test.
    pub async fn set_credential_health(
        &self,
        user: UserId,
        id: CredentialId,
        health: &CredentialHealth,
    ) -> Result<()> {
        let r = sqlx::query(
            "UPDATE credentials SET health_json = ? WHERE id = ? AND user_id = ? AND revoked_at IS NULL",
        )
        .bind(to_json(health)?)
        .bind(id.to_string())
        .bind(user.to_string())
        .execute(&self.pool)
        .await?;
        if r.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }
}
