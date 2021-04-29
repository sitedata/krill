mod signing;
use std::{fmt, path::PathBuf};

use openssl::error::ErrorStack;

pub use self::signing::*;

mod softsigner;
pub use self::softsigner::*;

mod pkcs11;
pub use self::pkcs11::*;

#[derive(Debug)]
pub enum SignerError {
    OpenSslError(ErrorStack),
    JsonError(serde_json::Error),
    InvalidWorkDir(PathBuf),
    IoError(std::io::Error),
    KeyNotFound,
    DecodeError,
    Pkcs11Error(String),
}

impl fmt::Display for SignerError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            SignerError::OpenSslError(e) => write!(f, "OpenSsl Error: {}", e),
            SignerError::JsonError(e) => write!(f, "Could not decode public key info: {}", e),
            SignerError::InvalidWorkDir(path) => write!(f, "Invalid base path: {}", path.to_string_lossy()),
            SignerError::IoError(e) => e.fmt(f),
            SignerError::KeyNotFound => write!(f, "Could not find key"),
            SignerError::DecodeError => write!(f, "Could not decode key"),
            SignerError::Pkcs11Error(e) => write!(f, "PKCS#11 error: {}", e),
        }
    }
}

impl From<ErrorStack> for SignerError {
    fn from(e: ErrorStack) -> Self {
        SignerError::OpenSslError(e)
    }
}

impl From<serde_json::Error> for SignerError {
    fn from(e: serde_json::Error) -> Self {
        SignerError::JsonError(e)
    }
}

impl From<std::io::Error> for SignerError {
    fn from(e: std::io::Error) -> Self {
        SignerError::IoError(e)
    }
}
