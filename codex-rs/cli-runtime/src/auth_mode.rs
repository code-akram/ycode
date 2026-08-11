use codex_cli_protocol::AuthMode as ApiAuthMode;
use codex_protocol::auth::AuthMode;

/// Converts the domain auth mode owned by `codex-protocol` into the cli-runtime wire type owned by
/// `codex-cli-protocol`.
///
/// The types stay separate so cli-runtime protocol ownership does not leak into domain crates.
/// Because this crate owns neither type, Rust's orphan rules require an explicit conversion
/// function instead of a `From` implementation.
pub(crate) fn auth_mode_to_api(auth_mode: AuthMode) -> ApiAuthMode {
    match auth_mode {
        AuthMode::ApiKey => ApiAuthMode::ApiKey,
        AuthMode::Chatgpt => ApiAuthMode::Chatgpt,
        AuthMode::ChatgptAuthTokens => ApiAuthMode::ChatgptAuthTokens,
        AuthMode::Headers => ApiAuthMode::Headers,
        AuthMode::AgentIdentity => ApiAuthMode::AgentIdentity,
        AuthMode::PersonalAccessToken => ApiAuthMode::PersonalAccessToken,
    }
}
