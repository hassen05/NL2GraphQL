use thiserror::Error;

pub(crate) type PipelineResult<T> = Result<T, PipelineError>;

#[derive(Debug, Error)]
pub(crate) enum PipelineError {
    #[error("Query validation: {message}")]
    Validation { message: String },
    #[error("GraphQL execution: {message}")]
    Execution { message: String },
    #[error("Plan parsing: {message}")]
    Planning { message: String },
}

impl PipelineError {
    pub(crate) fn validation(message: impl Into<String>) -> Self {
        Self::Validation {
            message: message.into(),
        }
    }

    pub(crate) fn execution(message: impl Into<String>) -> Self {
        Self::Execution {
            message: message.into(),
        }
    }

    pub(crate) fn planning(message: impl Into<String>) -> Self {
        Self::Planning {
            message: message.into(),
        }
    }
}
