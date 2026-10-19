//! Container ownership. Several daemons (or test runs) may share one Docker
//! host, so every container we create carries a `kiln.instance` label
//! derived from this daemon's data dir, and cleanup only ever touches
//! containers carrying our label.

use crate::Scheduler;
use kiln_sandbox::ManagedContainer;
use std::collections::BTreeMap;

/// Label key identifying the daemon instance (data dir) that owns a container.
pub(crate) const LABEL_INSTANCE: &str = "kiln.instance";

/// Stable, short id for a data dir: FNV-1a 64 of its canonical path.
pub(crate) fn instance_id(data_dir: &std::path::Path) -> String {
    let canon = std::fs::canonicalize(data_dir).unwrap_or_else(|_| data_dir.to_path_buf());
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in canon.to_string_lossy().as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{h:016x}")
}

impl Scheduler {
    pub(crate) fn instance(&self) -> String {
        instance_id(&self.cfg.data_dir)
    }

    pub(crate) fn tag_instance(&self, labels: &mut BTreeMap<String, String>) {
        labels.insert(LABEL_INSTANCE.into(), self.instance());
    }

    /// True only for containers this daemon instance created.
    pub(crate) fn owns(&self, c: &ManagedContainer) -> bool {
        c.labels
            .get(LABEL_INSTANCE)
            .is_some_and(|v| *v == self.instance())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_id_is_stable_and_distinct() {
        let a = instance_id(std::path::Path::new("/tmp/kiln-a"));
        assert_eq!(a, instance_id(std::path::Path::new("/tmp/kiln-a")));
        assert_ne!(a, instance_id(std::path::Path::new("/tmp/kiln-b")));
        assert_eq!(a.len(), 16);
    }
}
