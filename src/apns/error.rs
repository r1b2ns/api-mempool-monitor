use thiserror::Error;

#[derive(Debug, Error)]
pub enum ApnsError {
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("JWT error: {0}")]
    Jwt(#[from] jsonwebtoken::errors::Error),
    #[error("APNS rejected the notification (HTTP {code}): {reason}")]
    Apns { code: u16, reason: String },
}
