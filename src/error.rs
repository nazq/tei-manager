//! Unified error types for TEI Manager
//!
//! This module provides a standardized error handling approach across the application.
//! All errors are represented by the `TeiError` enum which can be converted to
//! appropriate HTTP status codes for REST API and gRPC status codes.

use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Serialize;
use thiserror::Error;

/// Unified error type for TEI Manager operations
///
/// This enum represents all possible error conditions in the application.
/// Each variant maps to appropriate HTTP and gRPC status codes.
#[derive(Debug, Error)]
pub enum TeiError {
    // ========================================================================
    // Instance Errors (typically 4xx)
    // ========================================================================
    /// Instance with the given name was not found
    #[error("Instance '{name}' not found")]
    InstanceNotFound { name: String },

    /// Instance with the given name already exists
    #[error("Instance '{name}' already exists")]
    InstanceExists { name: String },

    /// Port is already in use by another instance
    #[error("Port {port} already in use by instance '{instance}'")]
    PortConflict { port: u16, instance: String },

    /// Maximum number of instances has been reached
    #[error("Maximum instance count ({max}) reached")]
    MaxInstancesReached { max: usize },

    /// Instance is not in the expected state for the operation
    #[error("Instance '{name}' is {current_state}, expected {expected_state}")]
    InvalidInstanceState {
        name: String,
        current_state: String,
        expected_state: String,
    },

    // ========================================================================
    // Configuration Errors (typically 400)
    // ========================================================================
    /// Invalid configuration value
    #[error("Invalid configuration: {message}")]
    InvalidConfig { message: String },

    /// Port is outside valid range
    #[error("Port {port} is invalid: {reason}")]
    InvalidPort { port: u16, reason: String },

    /// Invalid GPU ID specified
    #[error("Invalid GPU ID {id}: {reason}")]
    InvalidGpuId { id: u32, reason: String },

    /// Invalid instance name
    #[error("Invalid instance name '{name}': {reason}")]
    InvalidInstanceName { name: String, reason: String },

    /// Port auto-allocation failed
    #[error("Failed to allocate port: {reason}")]
    PortAllocationFailed { reason: String },

    // ========================================================================
    // Authentication/Authorization Errors (401/403)
    // ========================================================================
    /// Missing authentication credentials
    #[error("Authentication required: {reason}")]
    Unauthenticated { reason: String },

    /// Access denied
    #[error("Access denied: {reason}")]
    Forbidden { reason: String },

    // ========================================================================
    // Validation Errors (400)
    // ========================================================================
    /// Request validation failed
    #[error("Validation error: {message}")]
    ValidationError { message: String },

    /// Missing required field
    #[error("Missing required field: {field}")]
    MissingField { field: String },

    // ========================================================================
    // External Service Errors (5xx)
    // ========================================================================
    /// TEI backend is unavailable
    #[error("Backend unavailable: {message}")]
    BackendUnavailable { message: String },

    /// Request to backend timed out
    #[error("Request timeout: {message}")]
    Timeout { message: String },

    // ========================================================================
    // Internal Errors (500)
    // ========================================================================
    /// Internal server error with underlying cause
    #[error("Internal error: {message}")]
    Internal { message: String },

    /// I/O error
    #[error("I/O error: {message}")]
    IoError { message: String },
}

impl TeiError {
    /// Get the HTTP status code for this error
    #[must_use]
    pub fn status_code(&self) -> StatusCode {
        match self {
            // 404 Not Found
            Self::InstanceNotFound { .. } => StatusCode::NOT_FOUND,

            // 409 Conflict
            Self::InstanceExists { .. } | Self::PortConflict { .. } => StatusCode::CONFLICT,

            // 400 Bad Request
            Self::InvalidConfig { .. }
            | Self::InvalidPort { .. }
            | Self::InvalidGpuId { .. }
            | Self::InvalidInstanceName { .. }
            | Self::ValidationError { .. }
            | Self::MissingField { .. }
            | Self::InvalidInstanceState { .. } => StatusCode::BAD_REQUEST,

            // 401 Unauthorized
            Self::Unauthenticated { .. } => StatusCode::UNAUTHORIZED,

            // 403 Forbidden
            Self::Forbidden { .. } => StatusCode::FORBIDDEN,

            // 422 Unprocessable Entity
            Self::MaxInstancesReached { .. } | Self::PortAllocationFailed { .. } => {
                StatusCode::UNPROCESSABLE_ENTITY
            }

            // 503 Service Unavailable
            Self::BackendUnavailable { .. } => StatusCode::SERVICE_UNAVAILABLE,

            // 504 Gateway Timeout
            Self::Timeout { .. } => StatusCode::GATEWAY_TIMEOUT,

            // 500 Internal Server Error
            Self::Internal { .. } | Self::IoError { .. } => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// Get a short error code for this error type
    #[must_use]
    pub fn error_code(&self) -> &'static str {
        match self {
            Self::InstanceNotFound { .. } => "INSTANCE_NOT_FOUND",
            Self::InstanceExists { .. } => "INSTANCE_EXISTS",
            Self::PortConflict { .. } => "PORT_CONFLICT",
            Self::MaxInstancesReached { .. } => "MAX_INSTANCES_REACHED",
            Self::InvalidInstanceState { .. } => "INVALID_INSTANCE_STATE",
            Self::InvalidConfig { .. } => "INVALID_CONFIG",
            Self::InvalidPort { .. } => "INVALID_PORT",
            Self::InvalidGpuId { .. } => "INVALID_GPU_ID",
            Self::InvalidInstanceName { .. } => "INVALID_INSTANCE_NAME",
            Self::PortAllocationFailed { .. } => "PORT_ALLOCATION_FAILED",
            Self::Unauthenticated { .. } => "UNAUTHENTICATED",
            Self::Forbidden { .. } => "FORBIDDEN",
            Self::ValidationError { .. } => "VALIDATION_ERROR",
            Self::MissingField { .. } => "MISSING_FIELD",
            Self::BackendUnavailable { .. } => "BACKEND_UNAVAILABLE",
            Self::Timeout { .. } => "TIMEOUT",
            Self::Internal { .. } => "INTERNAL_ERROR",
            Self::IoError { .. } => "IO_ERROR",
        }
    }

    /// Check if this is a client error (4xx)
    #[must_use]
    pub fn is_client_error(&self) -> bool {
        self.status_code().is_client_error()
    }

    /// Check if this is a server error (5xx)
    #[must_use]
    pub fn is_server_error(&self) -> bool {
        self.status_code().is_server_error()
    }
}

// ============================================================================
// Conversions from standard error types
// ============================================================================

impl From<std::io::Error> for TeiError {
    fn from(err: std::io::Error) -> Self {
        Self::IoError {
            message: err.to_string(),
        }
    }
}

impl From<anyhow::Error> for TeiError {
    fn from(err: anyhow::Error) -> Self {
        Self::Internal {
            message: err.to_string(),
        }
    }
}

// ============================================================================
// HTTP Response conversion
// ============================================================================

/// Standard JSON error response format
#[derive(Debug, Serialize)]
pub struct ErrorResponse {
    /// Human-readable error message
    pub error: String,
    /// Machine-readable error code
    pub code: &'static str,
    /// Timestamp of when the error occurred
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

impl IntoResponse for TeiError {
    fn into_response(self) -> Response {
        let status = self.status_code();
        let code = self.error_code();
        let message = self.to_string();

        // Log server errors at error level, client errors at debug level
        if self.is_server_error() {
            tracing::error!(error = %message, code = %code, "Server error");
        } else {
            tracing::debug!(error = %message, code = %code, "Client error");
        }

        let body = Json(ErrorResponse {
            error: message,
            code,
            timestamp: chrono::Utc::now(),
        });

        (status, body).into_response()
    }
}

// ============================================================================
// gRPC Status conversion
// ============================================================================

impl From<TeiError> for tonic::Status {
    fn from(err: TeiError) -> Self {
        let message = err.to_string();
        match err {
            TeiError::InstanceNotFound { .. } => tonic::Status::not_found(message),
            TeiError::InstanceExists { .. } | TeiError::PortConflict { .. } => {
                tonic::Status::already_exists(message)
            }
            TeiError::InvalidConfig { .. }
            | TeiError::InvalidPort { .. }
            | TeiError::InvalidGpuId { .. }
            | TeiError::InvalidInstanceName { .. }
            | TeiError::ValidationError { .. }
            | TeiError::MissingField { .. }
            | TeiError::InvalidInstanceState { .. } => tonic::Status::invalid_argument(message),
            TeiError::Unauthenticated { .. } => tonic::Status::unauthenticated(message),
            TeiError::Forbidden { .. } => tonic::Status::permission_denied(message),
            TeiError::MaxInstancesReached { .. } | TeiError::PortAllocationFailed { .. } => {
                tonic::Status::resource_exhausted(message)
            }
            TeiError::BackendUnavailable { .. } => tonic::Status::unavailable(message),
            TeiError::Timeout { .. } => tonic::Status::deadline_exceeded(message),
            TeiError::Internal { .. } | TeiError::IoError { .. } => {
                tonic::Status::internal(message)
            }
        }
    }
}

// ============================================================================
// Result type alias
// ============================================================================

/// Result type alias using TeiError
pub type TeiResult<T> = Result<T, TeiError>;

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;

    #[test]
    fn test_status_codes() {
        assert_eq!(
            TeiError::InstanceNotFound {
                name: "test".into()
            }
            .status_code(),
            StatusCode::NOT_FOUND
        );

        assert_eq!(
            TeiError::InstanceExists {
                name: "test".into()
            }
            .status_code(),
            StatusCode::CONFLICT
        );

        assert_eq!(
            TeiError::PortConflict {
                port: 8080,
                instance: "test".into()
            }
            .status_code(),
            StatusCode::CONFLICT
        );

        assert_eq!(
            TeiError::InvalidConfig {
                message: "test".into()
            }
            .status_code(),
            StatusCode::BAD_REQUEST
        );

        assert_eq!(
            TeiError::Unauthenticated {
                reason: "test".into()
            }
            .status_code(),
            StatusCode::UNAUTHORIZED
        );

        assert_eq!(
            TeiError::Forbidden {
                reason: "test".into()
            }
            .status_code(),
            StatusCode::FORBIDDEN
        );

        assert_eq!(
            TeiError::BackendUnavailable {
                message: "test".into()
            }
            .status_code(),
            StatusCode::SERVICE_UNAVAILABLE
        );

        assert_eq!(
            TeiError::Timeout {
                message: "test".into()
            }
            .status_code(),
            StatusCode::GATEWAY_TIMEOUT
        );

        assert_eq!(
            TeiError::Internal {
                message: "test".into()
            }
            .status_code(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[test]
    fn test_error_codes() {
        assert_eq!(
            TeiError::InstanceNotFound {
                name: "test".into()
            }
            .error_code(),
            "INSTANCE_NOT_FOUND"
        );

        assert_eq!(
            TeiError::PortConflict {
                port: 8080,
                instance: "test".into()
            }
            .error_code(),
            "PORT_CONFLICT"
        );
    }

    #[test]
    fn test_is_client_error() {
        assert!(
            TeiError::InstanceNotFound {
                name: "test".into()
            }
            .is_client_error()
        );
        assert!(
            !TeiError::Internal {
                message: "test".into()
            }
            .is_client_error()
        );
    }

    #[test]
    fn test_is_server_error() {
        assert!(
            TeiError::Internal {
                message: "test".into()
            }
            .is_server_error()
        );
        assert!(
            !TeiError::InstanceNotFound {
                name: "test".into()
            }
            .is_server_error()
        );
    }

    #[test]
    fn test_error_display() {
        let err = TeiError::InstanceNotFound {
            name: "my-instance".into(),
        };
        assert_eq!(err.to_string(), "Instance 'my-instance' not found");

        let err = TeiError::PortConflict {
            port: 8080,
            instance: "other".into(),
        };
        assert_eq!(
            err.to_string(),
            "Port 8080 already in use by instance 'other'"
        );
    }

    #[test]
    fn test_from_io_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file not found");
        let tei_err: TeiError = io_err.into();
        assert!(matches!(tei_err, TeiError::IoError { .. }));
    }

    #[test]
    fn test_grpc_status_conversion() {
        let err = TeiError::InstanceNotFound {
            name: "test".into(),
        };
        let status: tonic::Status = err.into();
        assert_eq!(status.code(), tonic::Code::NotFound);

        let err = TeiError::Unauthenticated {
            reason: "no token".into(),
        };
        let status: tonic::Status = err.into();
        assert_eq!(status.code(), tonic::Code::Unauthenticated);

        let err = TeiError::Timeout {
            message: "took too long".into(),
        };
        let status: tonic::Status = err.into();
        assert_eq!(status.code(), tonic::Code::DeadlineExceeded);
    }
}
