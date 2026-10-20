//! Forgiving number parsing for files written by AI workers
//! (`qa_report.json`, `review.json`): models write `586.1` or `"12"` where a
//! count or duration is expected. Floats are rounded, negatives clamp to 0,
//! numeric strings are accepted.

use serde::de::{self, Deserializer, Visitor};
use std::fmt;

struct UintVisitor;

impl Visitor<'_> for UintVisitor {
    type Value = u64;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a non-negative number")
    }
    fn visit_u64<E: de::Error>(self, v: u64) -> Result<u64, E> {
        Ok(v)
    }
    fn visit_i64<E: de::Error>(self, v: i64) -> Result<u64, E> {
        Ok(v.max(0) as u64)
    }
    fn visit_f64<E: de::Error>(self, v: f64) -> Result<u64, E> {
        if v.is_finite() {
            Ok(v.max(0.0).round() as u64)
        } else {
            Err(E::custom("number is not finite"))
        }
    }
    fn visit_str<E: de::Error>(self, v: &str) -> Result<u64, E> {
        let v = v.trim();
        v.parse::<u64>().or_else(|_| {
            v.parse::<f64>()
                .map_err(E::custom)
                .and_then(|f| self.visit_f64(f))
        })
    }
}

pub fn u64<'de, D: Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    d.deserialize_any(UintVisitor)
}

pub fn u32<'de, D: Deserializer<'de>>(d: D) -> Result<u32, D::Error> {
    let v = u64(d)?;
    Ok(v.min(u64::from(u32::MAX)) as u32)
}

pub fn opt_u32<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u32>, D::Error> {
    struct OptVisitor;
    impl<'de> Visitor<'de> for OptVisitor {
        type Value = Option<u32>;
        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("a non-negative number or null")
        }
        fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }
        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }
        fn visit_some<D2: Deserializer<'de>>(self, d: D2) -> Result<Self::Value, D2::Error> {
            u32(d).map(Some)
        }
    }
    d.deserialize_option(OptVisitor)
}
