use super::*;
pub(crate) fn session_needs_refresh(session: &OAuthSession) -> bool {
    session.access.trim().is_empty()
        || session.expires_at_ms <= 0
        || session.expires_at_ms <= now_ms() + REFRESH_SKEW_MS
}

pub(crate) async fn refresh_session(
    provider_id: &str,
    session: &OAuthSession,
) -> Result<OAuthSession, String> {
    match provider_id {
        "kimi" => refresh_kimi_token(&session.refresh).await,
        "anthropic" => refresh_anthropic_token(&session.refresh).await,
        "xai" => refresh_xai_token(&session.refresh).await,
        _ => Err(format!("{provider_id} の自動更新に未対応です")),
    }
}

