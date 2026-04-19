use thiserror::Error;

#[derive(Error, Debug)]
pub enum CoreError {
    #[error("configuration error: {0}")]
    Config(String),
    #[error("invalid state: {0}")]
    InvalidState(String),
}

pub type Result<T> = std::result::Result<T, CoreError>;
