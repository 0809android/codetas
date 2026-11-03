use super::*;
use std::sync::Arc;

use codetas_gateway::{fetch_provider_account_usage, ProviderAccountUsage};

/// Read account-wide usage directly, independently of retained log settings.
/// Account identifiers are exactly the ones returned by the account-list UI.
/// No login, main-account switch, settings save or inference is performed.
#[tauri::command]
pub async fn gateway_account_usage(app: AppHandle) -> Result<Vec<ProviderAccountUsage>, String> {
    configure_desktop_auth_store(&app)?;
    let settings = load_settings(&app)?;
    let accounts = settings
        .providers
        .iter()
        .map(|provider| {
            (
                provider.id.clone(),
                service_cmds::list_provider_oauth_accounts(app.clone(), provider.id.clone())
                    .unwrap_or_default(),
            )
        })
        .collect();
    let targets = usage_targets(&settings, &accounts);
    let borrowed = codex_home()
        .ok()
        .and_then(|home| codex_models::read_login(&home).ok());
    let borrowed = borrowed.map(|login| Arc::new((login.access, login.account)));
    let semaphore = Arc::new(tokio::sync::Semaphore::new(4));
    let mut tasks = tokio::task::JoinSet::new();
    for (provider, account_id) in targets {
        let semaphore = semaphore.clone();
        let borrowed = borrowed.clone();
        tasks.spawn(async move {
            let _permit = semaphore.acquire_owned().await.ok();
            let login = borrowed.as_ref().map(|l| (l.0.as_str(), l.1.as_str()));
            fetch_provider_account_usage(
                &provider,
                account_id,
                if provider.credential.source == CredentialSource::Forward {
                    login
                } else {
                    None
                },
            )
            .await
        });
    }
    let mut results = Vec::new();
    while let Some(result) = tasks.join_next().await {
        results.push(result.map_err(|_| "アカウント使用量の取得処理に失敗しました".to_string())?);
    }
    // Codex owns its current login; do not display an in-flight read from the
    // previous identity under the new default account after a sign-in switch.
    if let Some(before) = borrowed.as_ref() {
        let current = codex_home()
            .ok()
            .and_then(|home| codex_models::read_login(&home).ok());
        if current.as_ref().map(|login| login.account.as_str()) != Some(before.1.as_str()) {
            for result in &mut results {
                if result.provider_id == "openai" && result.account_id.is_none() {
                    result.status = "error".into();
                    result.windows.clear();
                    result.balance = None;
                    result.observed_at_ms = None;
                    result.plan = None;
                    result.error = Some(
                        "取得中にCodexのアカウントが切り替わりました。再取得してください".into(),
                    );
                }
            }
        }
    }
    results.sort_by(|a, b| (&a.provider_id, &a.account_id).cmp(&(&b.provider_id, &b.account_id)));
    Ok(results)
}

fn usage_targets(
    settings: &GatewaySettings,
    accounts: &HashMap<String, Vec<ProviderOAuthAccountView>>,
) -> Vec<(ProviderDefinition, Option<String>)> {
    let mut targets = Vec::new();
    for provider in &settings.providers {
        let oauth = accounts.get(&provider.id).map(Vec::as_slice).unwrap_or(&[]);
        let pool: Vec<_> = settings
            .account_pool
            .accounts
            .iter()
            .filter(|a| a.provider_id == provider.id)
            .collect();
        let mut covered_default = false;
        for account in oauth {
            let Some(store_key) = account.store_key.as_deref() else {
                continue;
            };
            let mut target = provider.clone();
            target.credential = oauth_session_credential(store_key);
            target.enabled &= account.enabled;
            covered_default |= provider.credential.source == CredentialSource::OAuth
                && provider.credential.command.is_none()
                && provider.credential.reference.as_deref() == Some(store_key);
            targets.push((target, Some(account.account_id.clone())));
        }
        for account in &pool {
            if oauth.iter().any(|a| a.account_id == account.id) {
                continue;
            }
            let mut target = provider.clone();
            target.credential = account.credential.clone();
            target.enabled &= account.enabled;
            covered_default |= serde_json::to_value(&account.credential).ok()
                == serde_json::to_value(&provider.credential).ok();
            targets.push((target, Some(account.id.clone())));
        }
        if !covered_default {
            // A Forward login is the current Codex account, not an OAuth pool
            // entry or whichever account happens to be marked main.
            targets.push((provider.clone(), None));
        }
    }
    targets
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn separate_accounts_keep_their_credentials_and_disabled_state() {
        let mut settings = GatewaySettings::default();
        let mut provider = provider_presets()
            .into_iter()
            .find(|p| p.id == "anthropic")
            .unwrap()
            .instantiate(None)
            .unwrap();
        provider.enabled = true;
        provider.credential = oauth_session_credential("anthropic#one");
        settings.providers = vec![provider];
        let view = |id: &str, key: &str, enabled| ProviderOAuthAccountView {
            account_id: id.into(),
            label: id.into(),
            email: None,
            store_key: Some(key.into()),
            main: id == "one",
            enabled,
            expires_at_ms: 0,
            source: "oauth".into(),
        };
        let accounts = HashMap::from([(
            "anthropic".into(),
            vec![
                view("one", "anthropic#one", true),
                view("two", "anthropic#two", false),
            ],
        )]);
        let targets = usage_targets(&settings, &accounts);
        assert_eq!(targets.len(), 2);
        assert_eq!(
            targets[0].0.credential.reference.as_deref(),
            Some("anthropic#one")
        );
        assert_eq!(
            targets[1].0.credential.reference.as_deref(),
            Some("anthropic#two")
        );
        assert_eq!(targets[0].1.as_deref(), Some("one"));
        assert!(!targets[1].0.enabled);
    }
    #[test]
    fn forward_login_is_never_assigned_to_an_oauth_pool_identity() {
        let mut settings = GatewaySettings::default();
        let mut provider = provider_presets()
            .into_iter()
            .find(|p| p.id == "openai")
            .unwrap()
            .instantiate(None)
            .unwrap();
        provider.credential.source = CredentialSource::Forward;
        provider.credential.reference = None;
        settings.providers = vec![provider];
        let accounts = HashMap::from([(
            "openai".into(),
            vec![ProviderOAuthAccountView {
                account_id: "pool".into(),
                label: "pool".into(),
                email: None,
                store_key: Some("openai#pool".into()),
                main: true,
                enabled: true,
                expires_at_ms: 0,
                source: "oauth".into(),
            }],
        )]);
        let targets = usage_targets(&settings, &accounts);
        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].1.as_deref(), Some("pool"));
        assert!(targets[1].1.is_none());
        assert_eq!(targets[1].0.credential.source, CredentialSource::Forward);
    }
}
