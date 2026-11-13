//! Crate-wide error type.
//!
//! `CoreError` covers everything that can go wrong *below* the API layer. The API
//! layer (`sp-server::error::ApiError`) maps these onto HTTP status codes and
//! machine-readable `code` strings that the frontend switches on.

use thiserror::Error;

pub type Result<T> = std::result::Result<T, CoreError>;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("input invalid: {field}: {reason}")]
    Validation { field: String, reason: String },

    #[error("not found: {what}")]
    NotFound { what: String },

    #[error("already exists: {what}")]
    Conflict { what: String },

    #[error("operation not allowed in this state: {0}")]
    InvalidState(String),

    #[error("permission denied: {0}")]
    Forbidden(String),

    #[error("unauthenticated")]
    Unauthenticated,

    #[error("path escapes the sandbox: {0}")]
    PathEscape(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("database error: {0}")]
    Db(String),

    #[error("serialization error: {0}")]
    Serde(String),

    #[error("checksum mismatch for {file}: expected {expected}, got {actual}")]
    ChecksumMismatch {
        file: String,
        expected: String,
        actual: String,
    },

    #[error("upstream service {service} returned {status}: {body}")]
    Upstream {
        service: &'static str,
        status: u16,
        body: String,
    },

    #[error("upstream service {service} is rate limiting us, retry after {retry_after_secs}s")]
    RateLimited {
        service: &'static str,
        retry_after_secs: u64,
    },

    #[error("upstream service {service} unavailable: {source}")]
    UpstreamUnavailable {
        service: &'static str,
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[error("no Java runtime available for Minecraft {0} (major version {1})")]
    JavaMissing(String, u16),

    #[error("server process error: {0}")]
    Process(String),

    #[error("operation cancelled")]
    Cancelled,

    /// A wall-clock deadline was exceeded (process boot, graceful stop, download).
    /// Maps to 504 so the UI can distinguish "still working, too slow" from a
    /// genuine failure.
    #[error("timed out: {0}")]
    Timeout(String),

    #[error("{0}")]
    Other(String),
}

impl CoreError {
    pub fn validation(field: impl Into<String>, reason: impl Into<String>) -> Self {
        Self::Validation {
            field: field.into(),
            reason: reason.into(),
        }
    }

    pub fn not_found(what: impl Into<String>) -> Self {
        Self::NotFound { what: what.into() }
    }

    pub fn conflict(what: impl Into<String>) -> Self {
        Self::Conflict { what: what.into() }
    }

    pub fn other(msg: impl Into<String>) -> Self {
        Self::Other(msg.into())
    }

    /// Stable machine-readable code for the API envelope and the UI.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Validation { .. } => "validation",
            Self::NotFound { .. } => "not_found",
            Self::Conflict { .. } => "conflict",
            Self::InvalidState(_) => "invalid_state",
            Self::Forbidden(_) => "forbidden",
            Self::Unauthenticated => "unauthenticated",
            Self::PathEscape(_) => "path_escape",
            Self::Io(_) => "io",
            Self::Db(_) => "database",
            Self::Serde(_) => "serialization",
            Self::ChecksumMismatch { .. } => "checksum_mismatch",
            Self::Upstream { .. } => "upstream",
            Self::RateLimited { .. } => "rate_limited",
            Self::UpstreamUnavailable { .. } => "upstream_unavailable",
            Self::JavaMissing(..) => "java_missing",
            Self::Process(_) => "process",
            Self::Cancelled => "cancelled",
            Self::Timeout(_) => "timeout",
            Self::Other(_) => "internal",
        }
    }

    /// Default HTTP status for this error class.
    pub fn status(&self) -> u16 {
        match self {
            Self::Validation { .. } | Self::ChecksumMismatch { .. } | Self::PathEscape(_) => 400,
            Self::Unauthenticated => 401,
            Self::Forbidden(_) => 403,
            Self::NotFound { .. } => 404,
            Self::Conflict { .. } | Self::InvalidState(_) => 409,
            Self::JavaMissing(..) => 422,
            Self::RateLimited { .. } => 429,
            Self::UpstreamUnavailable { .. } => 502,
            Self::Upstream { status, .. } => *status,
            _ => 500,
        }
    }

    /// True when the UI should offer "retry" rather than surfacing a hard error.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::RateLimited { .. } | Self::UpstreamUnavailable { .. }
        )
    }
}

impl From<serde_json::Error> for CoreError {
    fn from(value: serde_json::Error) -> Self {
        Self::Serde(value.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_stable() {
        assert_eq!(CoreError::Unauthenticated.code(), "unauthenticated");
        assert_eq!(CoreError::Unauthenticated.status(), 401);
        let e = CoreError::validation("name", "must be 1..=64 chars");
        assert_eq!(e.status(), 400);
        assert!(e.to_string().contains("name"));
    }

    #[test]
    fn retryable_marks_upstream_outages() {
        let e = CoreError::UpstreamUnavailable {
            service: "modrinth",
            source: "connection reset".into(),
        };
        assert!(e.is_retryable());
        assert_eq!(e.status(), 502);
    }
}
