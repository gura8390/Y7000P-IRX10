use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoreError {
    Invalid(String),
    Json { offset: usize, message: String },
    Unsupported(String),
    Cycle(String),
    Capacity(String),
    Closed,
    Superseded,
}

impl CoreError {
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::Invalid(message.into())
    }
}

impl fmt::Display for CoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(s) => write!(f, "Invalid input: {s}"),
            Self::Json { offset, message } => write!(f, "JSON at byte {offset}: {message}"),
            Self::Unsupported(s) => write!(f, "Unsupported: {s}"),
            Self::Cycle(s) => write!(f, "Automation reference cycle: {s}"),
            Self::Capacity(s) => write!(f, "Capacity exceeded: {s}"),
            Self::Closed => f.write_str("The execution batch has closed"),
            Self::Superseded => f.write_str("The request was superseded"),
        }
    }
}
impl std::error::Error for CoreError {}
pub type CoreResult<T> = Result<T, CoreError>;
