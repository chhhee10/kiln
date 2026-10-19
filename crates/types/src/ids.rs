//! Strongly typed identifiers. All are UUIDv7 (time-ordered) serialized as strings.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;
use uuid::Uuid;

macro_rules! id_type {
    ($($name:ident),* $(,)?) => {$(
        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub Uuid);

        impl $name {
            pub fn new() -> Self { Self(Uuid::now_v7()) }
            pub fn nil() -> Self { Self(Uuid::nil()) }
            pub fn as_uuid(&self) -> &Uuid { &self.0 }
        }
        impl Default for $name { fn default() -> Self { Self::new() } }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { self.0.fmt(f) }
        }
        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!(stringify!($name), "({})"), self.0)
            }
        }
        impl FromStr for $name {
            type Err = uuid::Error;
            fn from_str(s: &str) -> Result<Self, Self::Err> { Uuid::parse_str(s).map(Self) }
        }
        impl From<Uuid> for $name { fn from(u: Uuid) -> Self { Self(u) } }
    )*};
}

id_type!(
    UserId, SessionId, DeviceId, ProjectId, PlanId, TaskId, RunId, ArtifactId, MemoryId,
    EpisodeId, CredentialId, NotificationId, ThreadId, MessageId, JobId,
);
