use super::*;

#[test]
fn classifies_personal_access_tokens_by_prefix() {
    assert!(matches!(
        classify_codex_access_token("at-example"),
        CodexAccessToken::PersonalAccessToken
    ));
    assert!(matches!(
        classify_codex_access_token("header.payload.signature"),
        CodexAccessToken::AgentIdentityJwt("header.payload.signature")
    ));
}
