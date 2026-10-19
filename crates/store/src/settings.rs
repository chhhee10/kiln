//! `settings_kv`: small server-wide settings stored as JSON values.

use crate::row::{RowExt, to_json};
use crate::{Result, Store};
use serde::Serialize;
use serde::de::DeserializeOwned;

/// Well-known setting keys.
pub mod keys {
    /// `String`: the email domain allowed to self-register (e.g. "example.com").
    pub const ALLOWED_DOMAIN: &str = "allowed_domain";
    /// `bool`: the first-run setup has completed.
    pub const SETUP_DONE: &str = "setup_done";
}

impl Store {
    /// Read a setting. `None` when the key is unset.
    pub async fn get_setting<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        let row = sqlx::query("SELECT value FROM settings_kv WHERE key = ?")
            .bind(key)
            .fetch_optional(&self.pool)
            .await?;
        row.map(|r| r.json::<T>("value")).transpose()
    }

    /// Insert or replace a setting.
    pub async fn set_setting<T: Serialize + ?Sized>(&self, key: &str, value: &T) -> Result<()> {
        sqlx::query(
            "INSERT INTO settings_kv (key, value) VALUES (?, ?)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        )
        .bind(key)
        .bind(to_json(value)?)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Remove a setting. Returns whether it existed.
    pub async fn delete_setting(&self, key: &str) -> Result<bool> {
        let r = sqlx::query("DELETE FROM settings_kv WHERE key = ?")
            .bind(key)
            .execute(&self.pool)
            .await?;
        Ok(r.rows_affected() > 0)
    }
}
