//! `sessions`, `devices` and `pairing_codes`.

use crate::row::{RowExt, enum_str, now, ts};
use crate::users::user_from_row;
use crate::{Result, Store, StoreError};
use chrono::{Duration, Utc};
use kiln_types::{ClientKind, Device, DeviceId, SessionId, Timestamp, User, UserId};
use sqlx::sqlite::SqliteRow;

/// A client device to register or refresh.
#[derive(Clone, Debug)]
pub struct NewDevice {
    /// Reuse this id when the client already has one; otherwise the device is
    /// matched by `(user, kind, name)` or created.
    pub id: Option<DeviceId>,
    pub user_id: UserId,
    pub kind: ClientKind,
    pub name: String,
}

fn device_from_row(r: &SqliteRow) -> Result<Device> {
    Ok(Device {
        id: r.id("id")?,
        user_id: r.id("user_id")?,
        kind: r.enumv("kind")?,
        name: r.str("name")?,
        push_endpoint: r.opt_str("push_endpoint")?,
        last_seen: r.time("last_seen_at")?,
    })
}

impl Store {
    // ------------------------------------------------------------ sessions

    /// Create a session for `user` identified by the sha256 hex of its token.
    pub async fn create_session(
        &self,
        user: UserId,
        token_hash: &str,
        device: Option<DeviceId>,
        ttl: Duration,
    ) -> Result<SessionId> {
        let id = SessionId::new();
        let now_t = Utc::now();
        sqlx::query(
            "INSERT INTO sessions (id, user_id, token_hash, device_id, created_at, last_seen_at, expires_at)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(id.to_string())
        .bind(user.to_string())
        .bind(token_hash)
        .bind(device.map(|d| d.to_string()))
        .bind(ts(&now_t))
        .bind(ts(&now_t))
        .bind(ts(&(now_t + ttl)))
        .execute(&self.pool)
        .await
        .map_err(|e| crate::conflict_on_unique(e, "session token"))?;
        Ok(id)
    }

    /// Resolve a session token hash to its user. Returns `None` when the
    /// session does not exist, has expired, or the user is disabled.
    /// Touches `last_seen_at` on success.
    pub async fn session_user(&self, token_hash: &str) -> Result<Option<(User, SessionId)>> {
        let at = now();
        let row = sqlx::query(
            "SELECT u.*, s.id AS session_id FROM sessions s JOIN users u ON u.id = s.user_id
             WHERE s.token_hash = ? AND s.expires_at > ? AND u.status = 'active'",
        )
        .bind(token_hash)
        .bind(&at)
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else { return Ok(None) };
        let user = user_from_row(&row)?;
        let sid: SessionId = row.id("session_id")?;
        sqlx::query("UPDATE sessions SET last_seen_at = ? WHERE id = ?")
            .bind(&at)
            .bind(sid.to_string())
            .execute(&self.pool)
            .await?;
        Ok(Some((user, sid)))
    }

    /// Slide a session's expiry to `now + ttl`.
    pub async fn extend_session(&self, id: SessionId, ttl: Duration) -> Result<()> {
        sqlx::query("UPDATE sessions SET expires_at = ? WHERE id = ?")
            .bind(ts(&(Utc::now() + ttl)))
            .bind(id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Delete the session with this token hash. Returns whether it existed.
    pub async fn delete_session(&self, token_hash: &str) -> Result<bool> {
        let r = sqlx::query("DELETE FROM sessions WHERE token_hash = ?")
            .bind(token_hash)
            .execute(&self.pool)
            .await?;
        Ok(r.rows_affected() > 0)
    }

    /// Delete every session of a user (password change, disable).
    pub async fn delete_user_sessions(&self, user: UserId) -> Result<u64> {
        let r = sqlx::query("DELETE FROM sessions WHERE user_id = ?")
            .bind(user.to_string())
            .execute(&self.pool)
            .await?;
        Ok(r.rows_affected())
    }

    /// Remove expired sessions and pairing codes. Returns rows deleted.
    pub async fn prune_expired_sessions(&self) -> Result<u64> {
        let at = now();
        let a = sqlx::query("DELETE FROM sessions WHERE expires_at <= ?")
            .bind(&at)
            .execute(&self.pool)
            .await?;
        let b =
            sqlx::query("DELETE FROM pairing_codes WHERE expires_at <= ? OR used_at IS NOT NULL")
                .bind(&at)
                .execute(&self.pool)
                .await?;
        Ok(a.rows_affected() + b.rows_affected())
    }

    // ------------------------------------------------------------ devices

    /// Register a device or refresh an existing one (name, last seen).
    pub async fn upsert_device(&self, new: NewDevice) -> Result<Device> {
        let name = new.name.trim();
        if name.is_empty() {
            return Err(StoreError::Invalid("device name is empty".into()));
        }
        let at = now();
        let mut tx = self.begin_write().await?;
        let existing: Option<String> = match new.id {
            Some(id) => {
                sqlx::query_scalar("SELECT id FROM devices WHERE id = ? AND user_id = ?")
                    .bind(id.to_string())
                    .bind(new.user_id.to_string())
                    .fetch_optional(&mut *tx)
                    .await?
            }
            None => None,
        };
        let existing =
            match existing {
                Some(id) => Some(id),
                None => sqlx::query_scalar(
                    "SELECT id FROM devices WHERE user_id = ? AND kind = ? AND name = ? LIMIT 1",
                )
                .bind(new.user_id.to_string())
                .bind(enum_str(&new.kind))
                .bind(name)
                .fetch_optional(&mut *tx)
                .await?,
            };
        let id = match existing {
            Some(id) => {
                sqlx::query("UPDATE devices SET name = ?, kind = ?, last_seen_at = ? WHERE id = ?")
                    .bind(name)
                    .bind(enum_str(&new.kind))
                    .bind(&at)
                    .bind(&id)
                    .execute(&mut *tx)
                    .await?;
                id
            }
            None => {
                let id = new.id.unwrap_or_default().to_string();
                sqlx::query(
                    "INSERT INTO devices (id, user_id, kind, name, push_endpoint, created_at, last_seen_at)
                     VALUES (?, ?, ?, ?, NULL, ?, ?)",
                )
                .bind(&id)
                .bind(new.user_id.to_string())
                .bind(enum_str(&new.kind))
                .bind(name)
                .bind(&at)
                .bind(&at)
                .execute(&mut *tx)
                .await
                .map_err(|e| crate::conflict_on_unique(e, "device"))?;
                id
            }
        };
        let row = sqlx::query("SELECT * FROM devices WHERE id = ?")
            .bind(&id)
            .fetch_one(&mut *tx)
            .await?;
        tx.commit().await?;
        device_from_row(&row)
    }

    /// Fetch one device.
    pub async fn get_device(&self, id: DeviceId) -> Result<Device> {
        let row = sqlx::query("SELECT * FROM devices WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await?
            .ok_or(StoreError::NotFound)?;
        device_from_row(&row)
    }

    /// The user's devices, most recently seen first.
    pub async fn list_devices(&self, user: UserId) -> Result<Vec<Device>> {
        let rows =
            sqlx::query("SELECT * FROM devices WHERE user_id = ? ORDER BY last_seen_at DESC")
                .bind(user.to_string())
                .fetch_all(&self.pool)
                .await?;
        rows.iter().map(device_from_row).collect()
    }

    /// Set (or clear) a device's UnifiedPush endpoint.
    pub async fn set_push_endpoint(&self, device: DeviceId, endpoint: Option<&str>) -> Result<()> {
        let r = sqlx::query("UPDATE devices SET push_endpoint = ?, last_seen_at = ? WHERE id = ?")
            .bind(endpoint)
            .bind(now())
            .bind(device.to_string())
            .execute(&self.pool)
            .await?;
        if r.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    /// Remove a device owned by `user` (and its sessions).
    pub async fn delete_device(&self, user: UserId, device: DeviceId) -> Result<()> {
        let mut tx = self.begin_write().await?;
        sqlx::query("DELETE FROM sessions WHERE device_id = ? AND user_id = ?")
            .bind(device.to_string())
            .bind(user.to_string())
            .execute(&mut *tx)
            .await?;
        let r = sqlx::query("DELETE FROM devices WHERE id = ? AND user_id = ?")
            .bind(device.to_string())
            .bind(user.to_string())
            .execute(&mut *tx)
            .await?;
        if r.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        tx.commit().await?;
        Ok(())
    }

    // ------------------------------------------------------------ pairing

    /// Store a one-time pairing code (by hash) for `user`, valid for `ttl`.
    /// Returns the expiry.
    pub async fn create_pairing_code(
        &self,
        code_hash: &str,
        user: UserId,
        ttl: Duration,
    ) -> Result<Timestamp> {
        let expires = Utc::now() + ttl;
        sqlx::query("INSERT INTO pairing_codes (code_hash, user_id, expires_at, used_at) VALUES (?, ?, ?, NULL)")
            .bind(code_hash)
            .bind(user.to_string())
            .bind(ts(&expires))
            .execute(&self.pool)
            .await
            .map_err(|e| crate::conflict_on_unique(e, "pairing code"))?;
        Ok(expires)
    }

    /// Atomically mark a pairing code used. `None` if it is unknown, expired
    /// or already used.
    pub async fn consume_pairing_code(&self, code_hash: &str) -> Result<Option<UserId>> {
        let at = now();
        let row = sqlx::query(
            "UPDATE pairing_codes SET used_at = ?
             WHERE code_hash = ? AND used_at IS NULL AND expires_at > ?
             RETURNING user_id",
        )
        .bind(&at)
        .bind(code_hash)
        .bind(&at)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|r| r.id("user_id")).transpose()
    }
}
