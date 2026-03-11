use tracing::{error, info};

use super::client::ApnsClient;
use super::error::ApnsError;

/// Builds an [`ApnsClient`] from environment variables.
///
/// Responsible for reading, normalizing, and validating all APNS configuration
/// before constructing the client. Use [`ApnsBuilder::build`] as the single
/// entry point for APNS initialization.
pub struct ApnsBuilder;

impl ApnsBuilder {
    /// Reads APNS configuration from environment variables and returns a ready-to-use client.
    ///
    /// ### Required variables
    /// | Variable           | Description                                        |
    /// |--------------------|----------------------------------------------------|
    /// | `APNS_KEY_ID`      | Key ID from the Apple Developer portal             |
    /// | `APNS_TEAM_ID`     | Team / Organization ID                             |
    /// | `APNS_BUNDLE_ID`   | App bundle ID (e.g. com.example.myapp)             |
    /// | `APNS_PRIVATE_KEY` | PEM content of the .p8 key (inline text, not path) |
    ///
    /// ### Optional variables
    /// | Variable           | Default | Description                     |
    /// |--------------------|---------|---------------------------------|
    /// | `APNS_PRODUCTION`  | `false` | Set to `true` for production    |
    ///
    /// Returns `Ok(None)` if any required variable is missing.
    /// Returns `Err` only on HTTP client construction failure.
    pub fn build() -> Result<Option<ApnsClient>, ApnsError> {
        let (key_id, team_id, bundle_id) = match (
            std::env::var("APNS_KEY_ID").ok(),
            std::env::var("APNS_TEAM_ID").ok(),
            std::env::var("APNS_BUNDLE_ID").ok(),
        ) {
            (Some(k), Some(t), Some(b)) => (k, t, b),
            _ => return Ok(None),
        };

        let raw_pem = match std::env::var("APNS_PRIVATE_KEY") {
            Ok(v)  => v,
            Err(_) => return Ok(None),
        };

        // Normalize PEM: convert literal \n escape sequences and strip per-line whitespace.
        // This handles keys stored in .env files with indentation or \n literals.
        let pem = raw_pem
            .replace("\\n", "\n")
            .lines()
            .map(|line| line.trim())
            .collect::<Vec<_>>()
            .join("\n");

        // Log the last 5 chars of the raw key for startup diagnostics (never exposes the full PEM)
        let key_tail: String = raw_pem
            .chars()
            .rev()
            .take(5)
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        info!(apns_key_tail = %key_tail, "APNS_PRIVATE_KEY tail (last 5 chars)");

        // Validate PEM before constructing the client — fail fast on misconfiguration
        match jsonwebtoken::EncodingKey::from_ec_pem(pem.as_bytes()) {
            Ok(_) => info!(
                key_id  = %key_id,
                team_id = %team_id,
                "APNS PEM parsed successfully"
            ),
            Err(e) => error!(
                key_id  = %key_id,
                team_id = %team_id,
                error   = %e,
                "APNS PEM parse failed — check APNS_PRIVATE_KEY format"
            ),
        }

        let production = std::env::var("APNS_PRODUCTION")
            .map(|v| v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);

        let client = ApnsClient::new(key_id, team_id, bundle_id, production, pem)?;
        Ok(Some(client))
    }
}
