//! `users`: accounts, roles, limits and per-user settings.

use crate::row::{RowExt, enum_str, now, to_json};
use crate::{Result, Store, StoreError, conflict_on_unique};
use kiln_types::{User, UserId, UserLimits, UserRole, UserSettings, UserStatus, initials_for};
use sqlx::sqlite::SqliteRow;

/// A user to create. `password_hash` is an already-hashed PHC string.
#[derive(Clone, Debug)]
pub struct NewUser {
    pub email: String,
    pub name: String,
    pub role: UserRole,
    pub password_hash: String,
    pub limits: UserLimits,
}

/// Partial update of a user; `None` fields are left unchanged.
#[derive(Clone, Debug, Default)]
pub struct UserPatch {
    pub name: Option<String>,
    pub role: Option<UserRole>,
    pub status: Option<UserStatus>,
    pub limits: Option<UserLimits>,
    /// New PHC password hash.
    pub password_hash: Option<String>,
}

pub(crate) fn user_from_row(r: &SqliteRow) -> Result<User> {
    let name = r.str("name")?;
    Ok(User {
        id: r.id("id")?,
        email: r.str("email")?,
        initials: initials_for(&name),
        name,
        role: r.enumv("role")?,
        status: r.enumv("status")?,
        limits: r.json("limits_json")?,
        created_at: r.time("created_at")?,
    })
}

/// Normalize an email for storage (trimmed, lowercase).
fn normalize_email(email: &str) -> Result<String> {
    let e = email.trim().to_lowercase();
    if e.is_empty() || !e.contains('@') {
        return Err(StoreError::Invalid("email address is invalid".into()));
    }
    Ok(e)
}

impl Store {
    /// Create a user. `Conflict` if the email is already registered.
    pub async fn create_user(&self, new: NewUser) -> Result<User> {
        let email = normalize_email(&new.email)?;
        let name = new.name.trim().to_string();
        if name.is_empty() {
            return Err(StoreError::Invalid("name is empty".into()));
        }
        let id = UserId::new();
        let at = now();
        sqlx::query(
            "INSERT INTO users (id, email, name, role, status, password_hash, limits_json, created_at, updated_at)
             VALUES (?, ?, ?, ?, 'active', ?, ?, ?, ?)",
        )
        .bind(id.to_string())
        .bind(&email)
        .bind(&name)
        .bind(enum_str(&new.role))
        .bind(&new.password_hash)
        .bind(to_json(&new.limits)?)
        .bind(&at)
        .bind(&at)
        .execute(&self.pool)
        .await
        .map_err(|e| conflict_on_unique(e, "user with this email"))?;
        self.get_user(id).await
    }

    /// Fetch a user by id.
    pub async fn get_user(&self, id: UserId) -> Result<User> {
        let row = sqlx::query("SELECT * FROM users WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await?
            .ok_or(StoreError::NotFound)?;
        user_from_row(&row)
    }

    /// Fetch a user by email (case-insensitive).
    pub async fn get_user_by_email(&self, email: &str) -> Result<Option<User>> {
        let row = sqlx::query("SELECT * FROM users WHERE email = ?")
            .bind(email.trim())
            .fetch_optional(&self.pool)
            .await?;
        row.as_ref().map(user_from_row).transpose()
    }

    /// All users, oldest first.
    pub async fn list_users(&self) -> Result<Vec<User>> {
        let rows = sqlx::query("SELECT * FROM users ORDER BY created_at, id")
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(user_from_row).collect()
    }

    /// Number of users (any status).
    pub async fn count_users(&self) -> Result<u64> {
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
            .fetch_one(&self.pool)
            .await?;
        Ok(n.max(0) as u64)
    }

    /// Apply a partial update and return the updated user.
    pub async fn update_user(&self, id: UserId, patch: UserPatch) -> Result<User> {
        let mut tx = self.begin_write().await?;
        let row = sqlx::query("SELECT * FROM users WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(StoreError::NotFound)?;
        let cur = user_from_row(&row)?;
        let name = match patch.name {
            Some(n) if n.trim().is_empty() => {
                return Err(StoreError::Invalid("name is empty".into()));
            }
            Some(n) => n.trim().to_string(),
            None => cur.name,
        };
        let role = patch.role.unwrap_or(cur.role);
        let status = patch.status.unwrap_or(cur.status);
        let limits = patch.limits.unwrap_or(cur.limits);
        sqlx::query(
            "UPDATE users SET name = ?, role = ?, status = ?, limits_json = ?,
                    password_hash = COALESCE(?, password_hash), updated_at = ?
             WHERE id = ?",
        )
        .bind(name)
        .bind(enum_str(&role))
        .bind(enum_str(&status))
        .bind(to_json(&limits)?)
        .bind(patch.password_hash)
        .bind(now())
        .bind(id.to_string())
        .execute(&mut *tx)
        .await?;
        if status == UserStatus::Disabled {
            // A disabled user loses every live session immediately.
            sqlx::query("DELETE FROM sessions WHERE user_id = ?")
                .bind(id.to_string())
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        self.get_user(id).await
    }

    /// The stored PHC password hash.
    pub async fn user_password_hash(&self, id: UserId) -> Result<String> {
        sqlx::query_scalar("SELECT password_hash FROM users WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await?
            .ok_or(StoreError::NotFound)
    }

    /// Replace the user's settings.
    pub async fn set_user_settings(&self, id: UserId, settings: &UserSettings) -> Result<()> {
        let r = sqlx::query("UPDATE users SET settings_json = ?, updated_at = ? WHERE id = ?")
            .bind(to_json(settings)?)
            .bind(now())
            .bind(id.to_string())
            .execute(&self.pool)
            .await?;
        if r.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    /// The user's settings, or `None` when they never saved any (use defaults).
    pub async fn get_user_settings(&self, id: UserId) -> Result<Option<UserSettings>> {
        let row = sqlx::query("SELECT settings_json FROM users WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await?
            .ok_or(StoreError::NotFound)?;
        row.opt_json("settings_json")
    }
}
