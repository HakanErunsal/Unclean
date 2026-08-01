//! Defines stable failure categories, messages, and console exit codes shared by both frontends.

use thiserror::Error;

/// Identifies a stable failure category for frontends and automation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ErrorCode {
    /// Reports invalid command input or file content.
    InvalidInput,
    /// Reports a missing engine, plugin, preset, or snapshot.
    NotFound,
    /// Reports input that changed or conflicts with current state.
    Conflict,
    /// Reports drift detected by a status operation.
    Drift,
    /// Reports an operation blocked by operating-system permissions.
    PermissionDenied,
    /// Reports a failed write after recovery handling.
    WriteFailed,
    /// Reports a rollback that needs manual recovery.
    RollbackIncomplete,
    /// Reports an unexpected internal failure.
    Internal,
    /// Reports a command that the current build does not provide.
    Unavailable,
}

impl ErrorCode {
    /// Returns the stable lowercase identifier used in machine output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidInput => "invalid_input",
            Self::NotFound => "not_found",
            Self::Conflict => "conflict",
            Self::Drift => "drift",
            Self::PermissionDenied => "permission_denied",
            Self::WriteFailed => "write_failed",
            Self::RollbackIncomplete => "rollback_incomplete",
            Self::Internal => "internal",
            Self::Unavailable => "unavailable",
        }
    }

    /// Returns the stable process exit code used by the console frontend.
    #[must_use]
    pub const fn exit_code(self) -> u8 {
        match self {
            Self::InvalidInput => 3,
            Self::NotFound => 4,
            Self::Conflict => 5,
            Self::Drift => 6,
            Self::PermissionDenied => 7,
            Self::WriteFailed => 10,
            Self::RollbackIncomplete => 11,
            Self::Internal => 70,
            Self::Unavailable => 78,
        }
    }
}

/// Carries a typed product failure from the shared core to a frontend boundary.
#[derive(Debug, Error)]
pub enum Error {
    /// Reports invalid input and the action needed before retrying.
    #[error("Invalid input: {message}. Correct the value and retry.")]
    InvalidInput {
        /// Describes the rejected value or content.
        message: String,
    },
    /// Reports a missing item and the action needed before retrying.
    #[error("Requested item not found: {item}. Check the name or path and retry.")]
    NotFound {
        /// Names the missing engine, plugin, preset, or snapshot.
        item: String,
    },
    /// Reports stale or conflicting state and the action needed before retrying.
    #[error("Current state conflicts with the request: {message}. Rescan and review a new plan.")]
    Conflict {
        /// Describes the conflicting state.
        message: String,
    },
    /// Reports an engine selector that matches more than one canonical path.
    #[error(
        "Engine selection is ambiguous: {selector} matches {candidates}. Pass --engine-path with one reported path."
    )]
    AmbiguousEngine {
        /// Records the version selector that produced multiple matches.
        selector: String,
        /// Lists the canonical paths that match the selector.
        candidates: String,
    },
    /// Reports detected drift and the action needed to inspect it.
    #[error("Recorded state has drifted: {message}. Run plan to review the current differences.")]
    Drift {
        /// Describes the detected change.
        message: String,
    },
    /// Reports missing authority and the action needed before retrying.
    #[error("Permission denied: {message}. Review the target permissions and retry.")]
    PermissionDenied {
        /// Describes the denied operation.
        message: String,
    },
    /// Reports a failed write and the resulting recovery state.
    #[error("Write failed: {message}. Review the rollback result before retrying.")]
    WriteFailed {
        /// Describes the failed write.
        message: String,
    },
    /// Reports a rollback that needs manual recovery.
    #[error(
        "Rollback is incomplete: {message}. Preserve the backup and follow the reported recovery steps."
    )]
    RollbackIncomplete {
        /// Describes the files or recovery step that remain.
        message: String,
    },
    /// Preserves a structured failure returned by the elevated worker.
    #[error("{message}")]
    WorkerFailure {
        /// Identifies the stable worker failure category.
        code: ErrorCode,
        /// Retains the worker-rendered failure and recovery action.
        message: String,
    },
    /// Reports an unexpected product failure.
    #[error("Internal failure: {message}. Report this result with the application version.")]
    Internal {
        /// Describes the unexpected failure without private input content.
        message: String,
    },
    /// Reports a command that the current build does not provide.
    #[error(
        "Command is unavailable in this build: {command}. Install a build that provides this command."
    )]
    Unavailable {
        /// Names the unavailable command.
        command: &'static str,
    },
}

impl Error {
    /// Returns the stable failure category for interface formatting and exit status.
    #[must_use]
    pub const fn code(&self) -> ErrorCode {
        match self {
            Self::InvalidInput { .. } => ErrorCode::InvalidInput,
            Self::NotFound { .. } => ErrorCode::NotFound,
            Self::Conflict { .. } | Self::AmbiguousEngine { .. } => ErrorCode::Conflict,
            Self::Drift { .. } => ErrorCode::Drift,
            Self::PermissionDenied { .. } => ErrorCode::PermissionDenied,
            Self::WriteFailed { .. } => ErrorCode::WriteFailed,
            Self::RollbackIncomplete { .. } => ErrorCode::RollbackIncomplete,
            Self::WorkerFailure { code, .. } => *code,
            Self::Internal { .. } => ErrorCode::Internal,
            Self::Unavailable { .. } => ErrorCode::Unavailable,
        }
    }
}

/// Returns a shared-core result with a typed Unclean failure.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::{Error, ErrorCode};

    #[test]
    fn unavailable_errors_keep_the_documented_exit_code() {
        let error = Error::Unavailable { command: "engines" };

        assert_eq!(error.code(), ErrorCode::Unavailable);
        assert_eq!(error.code().as_str(), "unavailable");
        assert_eq!(error.code().exit_code(), 78);
        assert_eq!(
            error.to_string(),
            "Command is unavailable in this build: engines. Install a build that provides this command."
        );
    }
}
