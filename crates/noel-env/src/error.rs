//! The failures operations return (upstream `FileError` and
//! `ExecutionError`). Only the codes are contract; messages are not.

use std::fmt;
use std::io;

use serde::{Deserialize, Serialize};

/// Why a file system operation failed (upstream `FileErrorCode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileErrorCode {
    Aborted,
    NotFound,
    PermissionDenied,
    NotDirectory,
    IsDirectory,
    Invalid,
    NotSupported,
    Unknown,
}

impl FileErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Aborted => "aborted",
            Self::NotFound => "not_found",
            Self::PermissionDenied => "permission_denied",
            Self::NotDirectory => "not_directory",
            Self::IsDirectory => "is_directory",
            Self::Invalid => "invalid",
            Self::NotSupported => "not_supported",
            Self::Unknown => "unknown",
        }
    }

    /// The code `NodeExecutionEnv`'s `toFileError` gives an operating system
    /// error number; numbers it does not name are `unknown`.
    pub fn from_errno(errno: i32) -> Self {
        match errno {
            libc::ENOENT => Self::NotFound,
            libc::EACCES | libc::EPERM => Self::PermissionDenied,
            libc::ENOTDIR => Self::NotDirectory,
            libc::EISDIR => Self::IsDirectory,
            libc::EINVAL => Self::Invalid,
            _ => Self::Unknown,
        }
    }
}

impl fmt::Display for FileErrorCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A failed file system operation.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct FileError {
    pub code: FileErrorCode,
    pub message: String,
    /// The path the failing call was about, when there is one.
    pub path: Option<String>,
    /// The operating system's error number, when the failure was one.
    pub errno: Option<i32>,
}

impl FileError {
    pub fn new(code: FileErrorCode, message: impl Into<String>, path: Option<&str>) -> Self {
        Self {
            code,
            message: message.into(),
            path: path.map(str::to_string),
            errno: None,
        }
    }

    pub fn aborted(path: Option<&str>) -> Self {
        Self::new(FileErrorCode::Aborted, "aborted", path)
    }

    /// An I/O failure about `path`, coded as `NodeExecutionEnv` codes Node's.
    pub fn from_io(error: &io::Error, path: &str) -> Self {
        let code = match error.raw_os_error() {
            Some(errno) => FileErrorCode::from_errno(errno),
            None => match error.kind() {
                io::ErrorKind::NotFound => FileErrorCode::NotFound,
                io::ErrorKind::PermissionDenied => FileErrorCode::PermissionDenied,
                io::ErrorKind::NotADirectory => FileErrorCode::NotDirectory,
                io::ErrorKind::IsADirectory => FileErrorCode::IsDirectory,
                io::ErrorKind::InvalidInput => FileErrorCode::Invalid,
                _ => FileErrorCode::Unknown,
            },
        };
        Self {
            code,
            message: error.to_string(),
            path: Some(path.to_string()),
            errno: error.raw_os_error(),
        }
    }

    /// The failure as an I/O error. An operating system error is rebuilt
    /// from its number, so it reads exactly as the original did.
    pub fn to_io_error(&self) -> io::Error {
        if let Some(errno) = self.errno {
            return io::Error::from_raw_os_error(errno);
        }
        let kind = match self.code {
            FileErrorCode::NotFound => io::ErrorKind::NotFound,
            FileErrorCode::PermissionDenied => io::ErrorKind::PermissionDenied,
            FileErrorCode::NotDirectory => io::ErrorKind::NotADirectory,
            FileErrorCode::IsDirectory => io::ErrorKind::IsADirectory,
            FileErrorCode::Invalid => io::ErrorKind::InvalidInput,
            FileErrorCode::NotSupported => io::ErrorKind::Unsupported,
            FileErrorCode::Aborted => io::ErrorKind::Interrupted,
            FileErrorCode::Unknown => io::ErrorKind::Other,
        };
        io::Error::new(kind, self.message.clone())
    }
}

/// Why a command failed to run to an exit code (upstream
/// `ExecutionErrorCode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionErrorCode {
    Aborted,
    Timeout,
    ShellUnavailable,
    SpawnError,
    CallbackError,
    Unknown,
}

impl ExecutionErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Aborted => "aborted",
            Self::Timeout => "timeout",
            Self::ShellUnavailable => "shell_unavailable",
            Self::SpawnError => "spawn_error",
            Self::CallbackError => "callback_error",
            Self::Unknown => "unknown",
        }
    }
}

impl fmt::Display for ExecutionErrorCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A command that did not run to an exit code.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct ExecutionError {
    pub code: ExecutionErrorCode,
    pub message: String,
    /// Spill file of a command that timed out or was aborted after its
    /// output crossed the spill thresholds.
    pub spill_path: Option<String>,
    /// The operating system's error number, when starting the command failed
    /// with one.
    pub errno: Option<i32>,
}

impl ExecutionError {
    pub fn new(code: ExecutionErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            spill_path: None,
            errno: None,
        }
    }

    pub fn aborted() -> Self {
        Self::new(ExecutionErrorCode::Aborted, "aborted")
    }

    /// A command that could not be started.
    pub fn spawn(error: &io::Error) -> Self {
        Self {
            errno: error.raw_os_error(),
            ..Self::new(ExecutionErrorCode::SpawnError, error.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_upstreams_and_os_errors_read_as_before() {
        assert_eq!(
            FileErrorCode::PermissionDenied.to_string(),
            "permission_denied"
        );
        assert_eq!(
            ExecutionErrorCode::ShellUnavailable.as_str(),
            "shell_unavailable"
        );
        let original = io::Error::from_raw_os_error(libc::EACCES);
        let error = FileError::from_io(&original, "/x");
        assert_eq!(error.code, FileErrorCode::PermissionDenied);
        assert_eq!(error.to_io_error().to_string(), original.to_string());
        assert_eq!(
            FileError::from_io(&io::Error::from_raw_os_error(libc::ELOOP), "/x").code,
            FileErrorCode::Unknown
        );
    }
}
