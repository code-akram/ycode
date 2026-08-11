use super::*;
use codex_protocol::auth::AuthMode;
use pretty_assertions::assert_eq;

#[test]
fn built_in_catalog_contains_only_official_openai() {
    let providers = built_in_model_providers();

    assert_eq!(
        providers.keys().collect::<Vec<_>>(),
        vec![OPENAI_PROVIDER_ID]
    );
    assert_eq!(providers[OPENAI_PROVIDER_ID].name, "OpenAI");
}

#[test]
fn official_auth_modes_select_their_first_party_endpoints() {
    let provider = ModelProviderInfo::create_openai_provider(/*base_url*/ None);

    assert_eq!(
        provider
            .to_api_provider(Some(AuthMode::ApiKey))
            .expect("API-key provider")
            .base_url,
        "https://api.openai.com/v1"
    );
    assert_eq!(
        provider
            .to_api_provider(Some(AuthMode::Chatgpt))
            .expect("ChatGPT provider")
            .base_url,
        CHATGPT_CODEX_BASE_URL
    );
}
