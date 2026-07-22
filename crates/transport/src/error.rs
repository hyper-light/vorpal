//! Transport errors — redacted by construction (they never carry credentials).

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
  #[error("connect failed: {0}")]
  Connect(String),
  #[error("authentication failed: {0}")]
  Auth(String),
  #[error("exec failed: {0}")]
  Exec(String),
  #[error("i/o: {0}")]
  Io(String),
  #[error("wire: {0}")]
  Wire(#[from] vorpal_wire::WireError),
  #[error("policy refused: {0}")]
  Policy(String),
  #[error("unsupported: {0}")]
  Unsupported(String),
  #[error("{0}")]
  Other(String),
}

impl TransportError {
  pub fn connect(m: impl Into<String>) -> Self {
    Self::Connect(m.into())
  }
  pub fn auth(m: impl Into<String>) -> Self {
    Self::Auth(m.into())
  }
  pub fn exec(m: impl Into<String>) -> Self {
    Self::Exec(m.into())
  }
  pub fn io(e: impl std::fmt::Display) -> Self {
    Self::Io(e.to_string())
  }
  pub fn policy(m: impl Into<String>) -> Self {
    Self::Policy(m.into())
  }
  pub fn unsupported(m: impl Into<String>) -> Self {
    Self::Unsupported(m.into())
  }
  pub fn other(m: impl Into<String>) -> Self {
    Self::Other(m.into())
  }
}
