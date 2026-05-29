use std::fmt;

/// An error carrying a stable wire code. Wrap with `coded(...)` so the daemon
/// can return precise error codes to clients instead of `internal.error`.
#[derive(Debug, Clone)]
pub struct CodedError {
    pub code: &'static str,
    pub message: String,
}

impl fmt::Display for CodedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CodedError {}

pub fn coded(code: &'static str, message: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(CodedError {
        code,
        message: message.into(),
    })
}

/// Walk the error chain looking for a `CodedError`. Returns `internal.error` if
/// the chain is all anonymous failures.
pub fn code_of(err: &anyhow::Error) -> &'static str {
    for cause in err.chain() {
        if let Some(coded) = cause.downcast_ref::<CodedError>() {
            return coded.code;
        }
    }
    "internal.error"
}
