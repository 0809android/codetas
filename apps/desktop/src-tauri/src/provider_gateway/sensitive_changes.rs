//! Native confirmation for settings that decide what runs and where secrets go.
//!
//! The management window can rewrite the whole gateway configuration over IPC. The
//! CSP keeps injected script out of the renderer, but if script ever ran there, these
//! fields would turn it into code execution (credential commands), credential theft
//! (an existing credential sent to a new endpoint, or any environment variable sent as
//! a header), or a wider listener. Changing them needs a native dialog, which the
//! renderer cannot answer for the user.

use super::*;

const MAX_LISTED_CHANGES: usize = 12;
const MAX_COMMAND_DESCRIPTION_CHARS: usize = 300;

/// The provider's own credential and those of its pooled accounts, which are sent to
/// the same endpoint.
fn provider_credentials<'a>(
    settings: &'a GatewaySettings,
    provider: &'a ProviderDefinition,
) -> impl Iterator<Item = &'a ProviderCredential> + 'a {
    std::iter::once(&provider.credential).chain(
        settings
            .account_pool
            .accounts
            .iter()
            .filter(move |account| account.provider_id == provider.id)
            .map(|account| &account.credential),
    )
}

fn provider_commands<'a>(
    settings: &'a GatewaySettings,
    provider: &'a ProviderDefinition,
) -> impl Iterator<Item = &'a CredentialCommand> + 'a {
    provider_credentials(settings, provider).filter_map(|credential| credential.command.as_ref())
}

fn same_command(left: &CredentialCommand, right: &CredentialCommand) -> bool {
    left.program == right.program && left.args == right.args && left.cwd == right.cwd
}

fn describe_command(command: &CredentialCommand) -> String {
    let mut text = command.program.clone();
    for argument in &command.args {
        text.push(' ');
        text.push_str(argument);
    }
    if text.chars().count() > MAX_COMMAND_DESCRIPTION_CHARS {
        text = text.chars().take(MAX_COMMAND_DESCRIPTION_CHARS).collect();
        text.push('…');
    }
    text
}

/// Which secret a credential reads. How it is attached (transport, header name) does
/// not change what leaves the machine, so it is not part of the identity.
fn credential_identity(provider_id: &str, credential: &ProviderCredential) -> Option<String> {
    if credential.source == CredentialSource::None {
        return None;
    }
    // A provider's own Keychain slot only ever holds the key the user saved for that
    // provider through `store_provider_api_key`.
    if credential.source == CredentialSource::Keychain
        && credential.reference.as_deref() == Some(format!("api-key/{provider_id}").as_str())
    {
        return None;
    }
    Some(
        serde_json::json!({
            "source": credential.source,
            "reference": credential.reference,
            "command": credential.command.as_ref().map(|command| {
                serde_json::json!([command.program, command.args, command.cwd])
            }),
        })
        .to_string(),
    )
}

/// Where a provider sends requests, and every secret it can attach to them.
struct SecretRoute {
    destinations: BTreeSet<String>,
    secrets: BTreeSet<String>,
}

fn secret_route(settings: &GatewaySettings, provider: &ProviderDefinition) -> SecretRoute {
    let mut destinations = BTreeSet::from([provider.base_url.clone()]);
    destinations.extend(provider.realtime_ws_base_url.clone());
    let mut secrets = BTreeSet::new();
    for credential in provider_credentials(settings, provider) {
        secrets.extend(credential_identity(&provider.id, credential));
    }
    if let Some(variable) = provider.api_key_env.as_deref() {
        secrets.insert(format!("env:{variable}"));
    }
    for (header, variable) in &provider.env_headers {
        secrets.insert(format!("envHeader:{header}={variable}"));
    }
    SecretRoute {
        destinations,
        secrets,
    }
}

/// True when `after` can send a secret somewhere `before` did not send it: any secret
/// to a new destination, or a new secret to an existing one. Removing secrets or
/// destinations needs no confirmation.
fn sends_a_secret_somewhere_new(
    previous: &GatewaySettings,
    before: Option<&ProviderDefinition>,
    next: &GatewaySettings,
    after: &ProviderDefinition,
) -> bool {
    let after = secret_route(next, after);
    if after.secrets.is_empty() {
        return false;
    }
    let Some(before) = before else {
        return true;
    };
    let before = secret_route(previous, before);
    !after.destinations.is_subset(&before.destinations) || !after.secrets.is_subset(&before.secrets)
}

fn is_loopback_host(host: &str) -> bool {
    host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

/// The changes from `previous` to `next` that need the user's native confirmation,
/// described for the dialog. Empty when nothing sensitive changed.
pub(crate) fn sensitive_setting_changes(
    previous: &GatewaySettings,
    next: &GatewaySettings,
) -> Vec<String> {
    let mut changes = Vec::new();
    for provider in &next.providers {
        let before = previous
            .providers
            .iter()
            .find(|candidate| candidate.id == provider.id);
        for command in provider_commands(next, provider) {
            let known = before.is_some_and(|before| {
                provider_commands(previous, before).any(|existing| same_command(existing, command))
            });
            if !known {
                changes.push(format!(
                    "{}: 資格情報の取得に次のコマンドを実行します: {}",
                    provider.id,
                    describe_command(command)
                ));
            }
        }
        if sends_a_secret_somewhere_new(previous, before, next, provider) {
            changes.push(format!(
                "{}: 資格情報の送信先または送る資格情報が変わります（送信先: {}）",
                provider.id, provider.base_url
            ));
        }
        if provider.allow_private_network
            && !before.is_some_and(|before| before.allow_private_network)
        {
            changes.push(format!(
                "{}: ローカル・プライベートネットワークへの接続を許可します",
                provider.id
            ));
        }
    }

    let (old, new) = (&previous.security, &next.security);
    if new.allow_remote && !old.allow_remote {
        changes.push("ゲートウェイを他のホストからも使えるようにします（allowRemote）".into());
    }
    if next.runtime.host != previous.runtime.host && !is_loopback_host(&next.runtime.host) {
        changes.push(format!(
            "ゲートウェイの待ち受けアドレスを {} に変更します",
            next.runtime.host
        ));
    }
    if old.require_local_token && !new.require_local_token {
        changes.push("ゲートウェイのローカルトークン認証を無効にします".into());
    }
    if old.dns_pinning && !new.dns_pinning {
        changes.push("送信先の DNS ピン留め（SSRF 対策）を無効にします".into());
    }
    for origin in &new.cors_allow_origins {
        if !old.cors_allow_origins.contains(origin) {
            changes.push(format!(
                "ブラウザのオリジン {origin} からゲートウェイを使えるようにします"
            ));
        }
    }
    for key in new.external_access_keys.iter().filter(|key| key.enabled) {
        let covered = old.external_access_keys.iter().any(|existing| {
            existing.enabled
                && existing.env_var == key.env_var
                && key
                    .scopes
                    .iter()
                    .all(|scope| existing.scopes.contains(scope))
        });
        if !covered {
            changes.push(format!(
                "外部アクセスキー {}（環境変数 {}）でゲートウェイを使えるようにします",
                key.label, key.env_var
            ));
        }
    }
    let (old_updates, new_updates) = (&previous.updates, &next.updates);
    if old_updates.manifest_url != new_updates.manifest_url
        || old_updates.public_key_base64 != new_updates.public_key_base64
        || old_updates.installer_endpoint != new_updates.installer_endpoint
        || old_updates.installer_public_key != new_updates.installer_public_key
    {
        changes.push("CODETAS 更新の取得先または署名検証の公開鍵を変更します".into());
    }
    changes
}

/// Shows the sensitive changes in a native dialog and returns an error unless the
/// user approves them. Returns at once when nothing sensitive changed.
pub(crate) async fn confirm_sensitive_setting_changes(
    previous: &GatewaySettings,
    next: &GatewaySettings,
) -> Result<(), String> {
    let changes = sensitive_setting_changes(previous, next);
    if changes.is_empty() {
        return Ok(());
    }
    let mut description = String::from(
        "セキュリティに関わる次の設定を変更しようとしています。自分で行った操作でなければキャンセルしてください。\n\n",
    );
    for change in changes.iter().take(MAX_LISTED_CHANGES) {
        description.push_str("• ");
        description.push_str(change);
        description.push('\n');
    }
    if changes.len() > MAX_LISTED_CHANGES {
        description.push_str(&format!(
            "…ほか {} 件\n",
            changes.len() - MAX_LISTED_CHANGES
        ));
    }
    let result = rfd::AsyncMessageDialog::new()
        .set_level(rfd::MessageLevel::Warning)
        .set_title("CODETAS: セキュリティ設定の確認")
        .set_description(description)
        .set_buttons(rfd::MessageButtons::OkCancel)
        .show()
        .await;
    if matches!(
        result,
        rfd::MessageDialogResult::Ok | rfd::MessageDialogResult::Yes
    ) {
        Ok(())
    } else {
        Err("セキュリティに関わる設定の変更はキャンセルされました".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings_with(provider: ProviderDefinition) -> GatewaySettings {
        let mut settings = GatewaySettings::default();
        settings.providers = vec![provider];
        settings
    }

    fn provider(id: &str, base_url: &str) -> ProviderDefinition {
        ProviderDefinition {
            id: id.into(),
            base_url: base_url.into(),
            ..ProviderDefinition::default()
        }
    }

    fn command_provider(program: &str) -> ProviderDefinition {
        let mut provider = provider("custom", "https://api.example/v1");
        provider.credential = ProviderCredential {
            source: CredentialSource::Command,
            command: Some(CredentialCommand {
                program: program.into(),
                ..CredentialCommand::default()
            }),
            ..ProviderCredential::default()
        };
        provider
    }

    #[test]
    fn unchanged_settings_need_no_confirmation() {
        let settings = settings_with(command_provider("/usr/bin/security"));
        assert!(sensitive_setting_changes(&settings, &settings.clone()).is_empty());
    }

    #[test]
    fn a_new_or_changed_credential_command_needs_confirmation() {
        let empty = GatewaySettings::default();
        let first = settings_with(command_provider("/usr/bin/security"));
        let changed = settings_with(command_provider("/tmp/payload"));
        let added = sensitive_setting_changes(&empty, &first);
        assert!(added
            .iter()
            .any(|change| change.contains("/usr/bin/security")));
        let replaced = sensitive_setting_changes(&first, &changed);
        assert!(replaced
            .iter()
            .any(|change| change.contains("/tmp/payload")));
    }

    #[test]
    fn a_command_on_a_pooled_account_needs_confirmation() {
        let before = settings_with(provider("custom", "https://api.example/v1"));
        let mut after = before.clone();
        after.account_pool.accounts.push(AccountReference {
            id: "second".into(),
            provider_id: "custom".into(),
            label: "second".into(),
            credential: ProviderCredential {
                source: CredentialSource::Command,
                command: Some(CredentialCommand {
                    program: "/tmp/payload".into(),
                    ..CredentialCommand::default()
                }),
                ..ProviderCredential::default()
            },
            ..AccountReference::default()
        });
        let changes = sensitive_setting_changes(&before, &after);
        assert!(changes.iter().any(|change| change.contains("/tmp/payload")));
        // The same account stays approved once saved.
        assert!(sensitive_setting_changes(&after, &after.clone()).is_empty());
    }

    #[test]
    fn sending_an_existing_credential_to_a_new_endpoint_needs_confirmation() {
        let mut official = provider("anthropic", "https://api.anthropic.com/v1");
        official.credential.source = CredentialSource::OAuth;
        let mut moved = official.clone();
        moved.base_url = "https://collector.example/v1".into();
        let changes = sensitive_setting_changes(&settings_with(official), &settings_with(moved));
        assert!(changes
            .iter()
            .any(|change| change.contains("https://collector.example/v1")));
    }

    #[test]
    fn saving_an_api_key_into_the_providers_own_keychain_slot_needs_no_confirmation() {
        let mut before = provider("openai-api", "https://api.openai.com/v1");
        before.api_key_env = Some("OPENAI_API_KEY".into());
        let mut after = before.clone();
        after.api_key_env = None;
        after.credential = ProviderCredential {
            source: CredentialSource::Keychain,
            reference: Some("api-key/openai-api".into()),
            ..ProviderCredential::default()
        };
        assert!(
            sensitive_setting_changes(&settings_with(before), &settings_with(after)).is_empty()
        );
    }

    #[test]
    fn borrowing_another_secret_for_the_same_endpoint_needs_confirmation() {
        let before = provider("custom", "https://api.example/v1");
        let mut other_slot = before.clone();
        other_slot.credential = ProviderCredential {
            source: CredentialSource::Keychain,
            reference: Some("api-key/openai-api".into()),
            ..ProviderCredential::default()
        };
        let mut env_header = before.clone();
        env_header
            .env_headers
            .insert("x-extra".into(), "AWS_SECRET_ACCESS_KEY".into());
        for after in [other_slot, env_header] {
            let changes =
                sensitive_setting_changes(&settings_with(before.clone()), &settings_with(after));
            assert!(!changes.is_empty());
        }
    }

    #[test]
    fn a_provider_without_secrets_can_change_endpoint_freely() {
        let before = settings_with(provider("local", "http://127.0.0.1:11434/v1"));
        let after = settings_with(provider("local", "http://127.0.0.1:11435/v1"));
        assert!(sensitive_setting_changes(&before, &after).is_empty());
    }

    #[test]
    fn widening_the_listener_or_weakening_checks_needs_confirmation() {
        let before = GatewaySettings::default();
        let mut after = before.clone();
        after.security.allow_remote = true;
        after.security.require_local_token = true;
        after.runtime.host = "0.0.0.0".into();
        after.security.dns_pinning = false;
        after.security.cors_allow_origins = vec!["https://app.example".into()];
        after.updates.installer_public_key = Some("key".into());
        let changes = sensitive_setting_changes(&before, &after);
        for expected in [
            "allowRemote",
            "0.0.0.0",
            "DNS",
            "https://app.example",
            "公開鍵",
        ] {
            assert!(
                changes.iter().any(|change| change.contains(expected)),
                "{expected}: {changes:?}"
            );
        }
    }
}
