use super::*;
use crate::token_data::IdTokenInfo;
use crate::token_data::TokenData;
use base64::Engine;
use pretty_assertions::assert_eq;
use tempfile::tempdir;

fn chatgpt_auth() -> AuthDotJson {
    let encode = |bytes: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    let header = encode(br#"{"alg":"none","typ":"JWT"}"#);
    let payload = encode(
        br#"{"email":"user@example.com","https://api.openai.com/auth":{"chatgpt_account_id":"account-id"}}"#,
    );
    let raw_jwt = format!("{header}.{payload}.{}", encode(b"sig"));
    AuthDotJson {
        auth_mode: Some(AuthMode::Chatgpt),
        openai_api_key: None,
        tokens: Some(TokenData {
            id_token: IdTokenInfo {
                email: Some("user@example.com".to_string()),
                chatgpt_account_id: Some("account-id".to_string()),
                raw_jwt,
                ..Default::default()
            },
            access_token: "access-token".to_string(),
            refresh_token: "refresh-token".to_string(),
            account_id: Some("account-id".to_string()),
        }),
        last_refresh: Some(Utc::now()),
        agent_identity: None,
        personal_access_token: None,
    }
}

#[test]
fn file_storage_round_trips_chatgpt_auth_json() -> anyhow::Result<()> {
    let codex_home = tempdir()?;
    let storage = FileAuthStorage::new(codex_home.path().to_path_buf());
    let auth = chatgpt_auth();

    storage.save(&auth)?;

    assert_eq!(storage.load()?, Some(auth));
    let serialized = std::fs::read_to_string(get_auth_file(codex_home.path()))?;
    assert!(serialized.contains("\"OPENAI_API_KEY\": null"));
    assert!(serialized.contains("\"tokens\""));
    Ok(())
}

#[cfg(unix)]
#[test]
fn file_storage_enforces_owner_only_permissions() -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let codex_home = tempdir()?;
    let auth_file = get_auth_file(codex_home.path());
    std::fs::write(&auth_file, "stale")?;
    std::fs::set_permissions(&auth_file, std::fs::Permissions::from_mode(0o644))?;

    FileAuthStorage::new(codex_home.path().to_path_buf()).save(&chatgpt_auth())?;

    assert_eq!(
        std::fs::metadata(auth_file)?.permissions().mode() & 0o777,
        0o600
    );
    Ok(())
}

#[test]
fn file_storage_reports_corrupt_auth_json() -> anyhow::Result<()> {
    let codex_home = tempdir()?;
    std::fs::write(get_auth_file(codex_home.path()), "{not-json")?;

    let error = FileAuthStorage::new(codex_home.path().to_path_buf())
        .load()
        .expect_err("corrupt auth JSON must be reported");

    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    Ok(())
}

#[test]
fn file_storage_delete_removes_auth_file() -> anyhow::Result<()> {
    let codex_home = tempdir()?;
    let storage = create_auth_storage(codex_home.path().to_path_buf());
    storage.save(&chatgpt_auth())?;

    assert!(storage.delete()?);
    assert!(!get_auth_file(codex_home.path()).exists());
    assert!(!storage.delete()?);
    Ok(())
}

#[test]
fn ephemeral_storage_never_creates_auth_json() -> anyhow::Result<()> {
    let codex_home = tempdir()?;
    let storage = create_ephemeral_auth_storage(codex_home.path().to_path_buf());
    let auth = chatgpt_auth();

    storage.save(&auth)?;
    assert_eq!(storage.load()?, Some(auth));
    assert!(!get_auth_file(codex_home.path()).exists());
    assert!(storage.delete()?);
    assert_eq!(storage.load()?, None);
    Ok(())
}

#[test]
fn file_storage_round_trips_registered_agent_identity() -> anyhow::Result<()> {
    let codex_home = tempdir()?;
    let storage = FileAuthStorage::new(codex_home.path().to_path_buf());
    let record = AgentIdentityAuthRecord {
        agent_runtime_id: "agent-runtime-id".to_string(),
        agent_private_key: "private-key".to_string(),
        account_id: "account-id".to_string(),
        chatgpt_user_id: "user-id".to_string(),
        email: None,
        plan_type: AccountPlanType::Pro,
        chatgpt_account_is_fedramp: false,
        task_id: Some("task-id".to_string()),
    };
    let auth = AuthDotJson {
        auth_mode: Some(AuthMode::AgentIdentity),
        openai_api_key: None,
        tokens: None,
        last_refresh: None,
        agent_identity: Some(AgentIdentityStorage::Record(record)),
        personal_access_token: None,
    };

    storage.save(&auth)?;

    assert_eq!(storage.load()?, Some(auth));
    Ok(())
}

#[test]
fn file_storage_round_trips_personal_access_token() -> anyhow::Result<()> {
    let codex_home = tempdir()?;
    let storage = FileAuthStorage::new(codex_home.path().to_path_buf());
    let auth = AuthDotJson {
        auth_mode: Some(AuthMode::PersonalAccessToken),
        openai_api_key: None,
        tokens: None,
        last_refresh: None,
        agent_identity: None,
        personal_access_token: Some("pat-example".to_string()),
    };

    storage.save(&auth)?;

    assert_eq!(storage.load()?, Some(auth));
    Ok(())
}
