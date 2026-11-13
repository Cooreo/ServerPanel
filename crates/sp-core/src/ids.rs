//! Newtype ids.
//!
//! Keeping these as `i64` under the hood matches the SQLite `INTEGER PRIMARY KEY`
//! and keeps JSON small; `ts-rs` maps them to `number` (see `docs/architecture.md`).

use serde::{Deserialize, Serialize};
use std::fmt;

macro_rules! newtype_id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize)]
        #[serde(transparent)]
        #[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
        #[cfg_attr(feature = "codegen", ts(export_to = "web/src/lib/api/gen"))]
        pub struct $name(pub i64);

        impl $name {
            #[inline]
            pub const fn new(v: i64) -> Self {
                Self(v)
            }
            #[inline]
            pub const fn get(self) -> i64 {
                self.0
            }
        }

        impl From<i64> for $name {
            #[inline]
            fn from(v: i64) -> Self {
                Self(v)
            }
        }

        impl From<$name> for i64 {
            #[inline]
            fn from(v: $name) -> i64 {
                v.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }

        impl std::str::FromStr for $name {
            type Err = std::num::ParseIntError;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                s.parse::<i64>().map(Self)
            }
        }
    };
}

newtype_id!(
    /// Stable identifier for a managed Minecraft server.
    ServerId
);
newtype_id!(/// Identifier for a panel user.
    UserId);
newtype_id!(/// Identifier for a stored backup.
    BackupId);
newtype_id!(/// Identifier for a scheduled task.
    TaskId);
newtype_id!(/// Identifier for a Java runtime record.
    JavaId);
newtype_id!(/// One row of the rolling console ring buffer.
    LogSeq);
newtype_id!(/// Ephemeral id for a streamed upload/download.
    FileTokenId);

/// `i64` is beyond `Number.MAX_SAFE_INTEGER` in JS if we ever use random ids, so
/// these are always small, sequential rowids. The helpers below assert that.
pub const MAX_SAFE_ID: i64 = 2_i64.pow(53) - 1;

#[inline]
pub fn id_is_js_safe(id: i64) -> bool {
    id.abs() <= MAX_SAFE_ID
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_serde() {
        let id = ServerId::new(42);
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "42");
        let back: ServerId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, id);
    }

    #[test]
    fn parse_display() {
        let id: ServerId = "7".parse().unwrap();
        assert_eq!(id.get(), 7);
        assert_eq!(id.to_string(), "7");
    }
}
