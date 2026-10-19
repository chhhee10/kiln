//! Encoding/decoding helpers shared by every query module.

use crate::{Result, StoreError};
use chrono::{DateTime, SecondsFormat, Utc};
use kiln_types::Timestamp;
use serde::Serialize;
use serde::de::DeserializeOwned;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;
use std::str::FromStr;

/// Current time in the canonical storage format.
pub(crate) fn now() -> String {
    ts(&Utc::now())
}

/// Canonical storage format for a timestamp (fixed width, so string order is
/// time order).
pub(crate) fn ts(t: &Timestamp) -> String {
    t.to_rfc3339_opts(SecondsFormat::Micros, true)
}

pub(crate) fn parse_ts(s: &str) -> Result<Timestamp> {
    DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&Utc))
        .map_err(|_| StoreError::Corrupt(format!("bad timestamp {s:?}")))
}

/// snake_case serde name of a unit enum variant.
pub(crate) fn enum_str<T: Serialize>(v: &T) -> String {
    match serde_json::to_value(v) {
        Ok(serde_json::Value::String(s)) => s,
        // Only unit enums are passed here; anything else is a programming
        // error that we surface as a JSON string so it is visible in the DB.
        Ok(other) => other.to_string(),
        Err(_) => String::new(),
    }
}

/// Parse a snake_case serde name back into a unit enum.
pub(crate) fn parse_enum<T: DeserializeOwned>(s: &str, what: &str) -> Result<T> {
    serde_json::from_value(serde_json::Value::String(s.to_owned()))
        .map_err(|_| StoreError::Corrupt(format!("bad {what} {s:?}")))
}

pub(crate) fn to_json<T: Serialize + ?Sized>(v: &T) -> Result<String> {
    Ok(serde_json::to_string(v)?)
}

/// Typed column accessors with consistent error mapping.
pub(crate) trait RowExt {
    fn str(&self, col: &str) -> Result<String>;
    fn opt_str(&self, col: &str) -> Result<Option<String>>;
    fn int(&self, col: &str) -> Result<i64>;
    fn opt_int(&self, col: &str) -> Result<Option<i64>>;
    fn float(&self, col: &str) -> Result<f64>;
    fn flag(&self, col: &str) -> Result<bool> {
        Ok(self.int(col)? != 0)
    }
    fn blob(&self, col: &str) -> Result<Vec<u8>>;
    fn time(&self, col: &str) -> Result<Timestamp> {
        parse_ts(&self.str(col)?)
    }
    fn opt_time(&self, col: &str) -> Result<Option<Timestamp>> {
        self.opt_str(col)?.map(|s| parse_ts(&s)).transpose()
    }
    fn id<T: FromStr>(&self, col: &str) -> Result<T> {
        let s = self.str(col)?;
        s.parse()
            .map_err(|_| StoreError::Corrupt(format!("bad id in {col}: {s:?}")))
    }
    fn opt_id<T: FromStr>(&self, col: &str) -> Result<Option<T>> {
        match self.opt_str(col)? {
            Some(s) => s
                .parse()
                .map(Some)
                .map_err(|_| StoreError::Corrupt(format!("bad id in {col}: {s:?}"))),
            None => Ok(None),
        }
    }
    fn enumv<T: DeserializeOwned>(&self, col: &str) -> Result<T> {
        parse_enum(&self.str(col)?, col)
    }
    fn opt_enumv<T: DeserializeOwned>(&self, col: &str) -> Result<Option<T>> {
        self.opt_str(col)?.map(|s| parse_enum(&s, col)).transpose()
    }
    fn json<T: DeserializeOwned>(&self, col: &str) -> Result<T> {
        Ok(serde_json::from_str(&self.str(col)?)?)
    }
    fn opt_json<T: DeserializeOwned>(&self, col: &str) -> Result<Option<T>> {
        match self.opt_str(col)? {
            Some(s) => Ok(Some(serde_json::from_str(&s)?)),
            None => Ok(None),
        }
    }
    /// Non-negative integer column as `u64` (clamps corrupt negatives to 0).
    fn uint(&self, col: &str) -> Result<u64> {
        Ok(self.int(col)?.max(0) as u64)
    }
    fn u32v(&self, col: &str) -> Result<u32> {
        Ok(self.int(col)?.clamp(0, u32::MAX as i64) as u32)
    }
}

impl RowExt for SqliteRow {
    fn str(&self, col: &str) -> Result<String> {
        Ok(self.try_get::<String, _>(col)?)
    }
    fn opt_str(&self, col: &str) -> Result<Option<String>> {
        Ok(self.try_get::<Option<String>, _>(col)?)
    }
    fn int(&self, col: &str) -> Result<i64> {
        Ok(self.try_get::<i64, _>(col)?)
    }
    fn opt_int(&self, col: &str) -> Result<Option<i64>> {
        Ok(self.try_get::<Option<i64>, _>(col)?)
    }
    fn float(&self, col: &str) -> Result<f64> {
        Ok(self.try_get::<f64, _>(col)?)
    }
    fn blob(&self, col: &str) -> Result<Vec<u8>> {
        Ok(self.try_get::<Vec<u8>, _>(col)?)
    }
}

/// Escape `%`, `_` and `\` for a `LIKE ... ESCAPE '\'` pattern.
pub(crate) fn like_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}
