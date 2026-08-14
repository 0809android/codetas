use super::*;
use serde_json::Value;
use std::fs;
pub(crate) fn detect_kimi_cli_session(home: &Path) -> Option<OAuthSession> {
    parse_kimi_credentials(
        &fs::read_to_string(home.join(".kimi-code/credentials/kimi-code.json")).ok()?,
    )
}

pub(crate) fn detect_claude_cli_session(home: &Path) -> Option<OAuthSession> {
    if let Some(raw) = read_claude_secure_storage(home) {
        if let Some(session) = parse_claude_credentials(&raw) {
            return Some(session);
        }
    }
    None
}

pub(crate) fn detect_xai_cli_session(home: &Path) -> Option<OAuthSession> {
    parse_xai_credentials(&fs::read_to_string(home.join(".grok/auth.json")).ok()?)
}

pub(crate) fn read_claude_secure_storage(home: &Path) -> Option<String> {
    let file = claude_credentials_path(home);
    let from_file = fs::read_to_string(&file).ok();
    if env::var_os("CLAUDE_CONFIG_DIR").is_some() {
        return from_file.or_else(read_claude_keychain);
    }
    read_claude_keychain().or(from_file)
}

pub(crate) fn claude_credentials_path(home: &Path) -> PathBuf {
    env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| home.join(".claude"))
        .join(".credentials.json")
}

pub(crate) fn read_claude_keychain() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let output = std::process::Command::new("security")
            .args(["find-generic-password", "-s", CLAUDE_KEYCHAIN_SERVICE, "-w"])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let value = String::from_utf8(output.stdout).ok()?.trim().to_string();
        if value.is_empty() {
            return None;
        }
        return Some(value);
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

pub(crate) fn parse_kimi_credentials(raw: &str) -> Option<OAuthSession> {
    let value: Value = serde_json::from_str(raw).ok()?;
    let access = json_string(&value, &["access_token", "accessToken", "access"])?;
    let refresh =
        json_string(&value, &["refresh_token", "refreshToken", "refresh"]).unwrap_or_default();
    if access.is_empty() {
        return None;
    }
    Some(OAuthSession {
        access,
        refresh,
        expires_at_ms: json_expiry_ms(&value, &["expires_at", "expiresAt", "expires"]),
        source: "local-cli".into(),
        account_id: json_string(&value, &["user_id", "userId", "accountId"]),
        email: json_string(&value, &["email"]),
    })
}

pub(crate) fn parse_claude_credentials(raw: &str) -> Option<OAuthSession> {
    let value: Value = serde_json::from_str(raw).ok()?;
    let oauth = value.get("claudeAiOauth").unwrap_or(&value);
    let access = json_string(oauth, &["accessToken", "access_token", "access"])?;
    let refresh =
        json_string(oauth, &["refreshToken", "refresh_token", "refresh"]).unwrap_or_default();
    if access.is_empty() {
        return None;
    }
    Some(OAuthSession {
        access,
        refresh,
        expires_at_ms: json_expiry_ms(oauth, &["expiresAt", "expires_at", "expires"]),
        source: "local-cli".into(),
        account_id: json_string(oauth, &["accountId", "account_id"]),
        email: json_string(oauth, &["email"]),
    })
}

pub(crate) fn parse_xai_credentials(raw: &str) -> Option<OAuthSession> {
    let value: Value = serde_json::from_str(raw).ok()?;
    let entry = value.as_object()?.iter().find_map(|(key, entry)| {
        key.starts_with("https://auth.x.ai::")
            .then_some(entry)
            .or_else(|| key.contains("auth.x.ai").then_some(entry))
    })?;
    let access = json_string(entry, &["key", "access_token", "access"])?;
    let refresh = json_string(entry, &["refresh_token", "refreshToken", "refresh"])?;
    if access.is_empty() || refresh.is_empty() {
        return None;
    }
    Some(OAuthSession {
        access,
        refresh,
        expires_at_ms: json_expiry_ms(entry, &["expires_at", "expiresAt", "expires"]),
        source: "local-cli".into(),
        account_id: json_string(entry, &["user_id", "userId", "accountId"]),
        email: json_string(entry, &["email"]),
    })
}

