use crate::clients::{self, ClientIntegrationReport};
use crate::service::{self, ServiceInstallReport, ServiceStatus, ServiceUninstallReport};
use codetas_gateway::{
    adopt_local_cli_sessions, auth_store_is_configured, build_codex_catalog, check_signed_update,
    configure_auth_store_path,
    detect_local_cli_session, discover_provider_models, has_stored_session,
    list_observability_trash, login_provider_oauth, oauth_session_credential,
    parse_gateway_settings_json, preview_observability_cleanup, provider_presets,
    provider_supports_native_oauth, read_observability_breakdown, read_observability_summary,
    read_recent_observability_events, restore_observability_trash, start_gateway_with_options,
    test_provider_connection, trash_observability_cleanup, ClientIntegrationSettings,
    CredentialCommand, CredentialSource, CredentialTransport, GatewayHandle,
    GatewayRuntimeOptions, GatewaySettings, ObservabilityBreakdown, ObservabilityCleanupPreview,
    ObservabilitySummary, ObservabilityTrashEntry, ObservabilityTrashReport, ObservationEvent,
    ProviderConnectionReport, ProviderCredential, ProviderDefinition, ProviderPreset, UpdateCheck,
};
use command_group::CommandGroup;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    env,
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc, Mutex as StdMutex,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tauri::{AppHandle, Manager, State};
use tauri_plugin_updater::UpdaterExt;
use tokio::sync::Mutex;
use toml_edit::{value, DocumentMut, Item, Table};

const GATEWAY_PROVIDER_ID: &str = "codetas_gateway";
const CODEX_JOURNAL_VERSION: u8 = 1;

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
enum CodexRoutingMode {
    #[default]
    ProviderTable,
    OpenAiBaseUrl,
}

#[derive(Default)]
pub struct GatewayManager {
    handle: Mutex<Option<GatewayHandle>>,
}

#[derive(Default)]
pub struct DebugScopeManager {
    scopes: StdMutex<HashMap<String, DebugScopeState>>,
}

struct DebugScopeState {
    started_at_ms: u64,
    expires_at_ms: u64,
}

static DEBUG_SCOPE_SEQUENCE: AtomicU64 = AtomicU64::new(1);
static OAUTH_TRANSACTION_SEQUENCE: AtomicU64 = AtomicU64::new(1);
static ATOMIC_WRITE_SEQUENCE: AtomicU64 = AtomicU64::new(1);
#[cfg(windows)]
static WINDOWS_REPLACE_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DebugScope {
    id: String,
    started_at_ms: u64,
    expires_at_ms: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewayStatus {
    running: bool,
    url: String,
    providers: Vec<ProviderDefinition>,
    default_provider: Option<String>,
    codex_configured: bool,
    settings_path: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderUpsertInput {
    provider: ProviderDefinition,
    make_default: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexGatewayInstallInput {
    model: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PresetInstallInput {
    preset_id: String,
    base_url: Option<String>,
    make_default: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderOAuthInput {
    provider_id: String,
    broker: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderOAuthLaunchReport {
    provider_id: String,
    broker: String,
    launched: bool,
    token_command: String,
    instructions: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceInstallInput {
    install_shim: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExternalClientIntegrationInput {
    claude_code: bool,
    claude_desktop: bool,
    opencode: bool,
    grok: bool,
    pi: bool,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct CodexInstallJournal {
    version: u8,
    config_path: String,
    backup_path: Option<String>,
    catalog_path: String,
    #[serde(default)]
    catalog_backup_path: Option<String>,
    #[serde(default)]
    catalog_existed: Option<bool>,
    installed_model: String,
    installed_base_url: String,
    #[serde(default)]
    routing_mode: CodexRoutingMode,
    #[serde(default)]
    installed_local_token: bool,
    #[serde(default)]
    installed_agents: Option<CodexAgentInstall>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct CodexAgentInstall {
    enabled: bool,
    max_concurrent_threads_per_session: i64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexRestoreReport {
    restored: bool,
    config_path: String,
    conflicts: Vec<String>,
    removed_catalog: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodetasUninstallReport {
    restored_codex: bool,
    removed_settings: bool,
    removed_catalog: bool,
    removed_observability: bool,
    removed_service: bool,
    stopped_gateway: bool,
    conflicts: Vec<String>,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum DiagnosticLevel {
    Pass,
    Warning,
    Error,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticCheck {
    id: String,
    level: DiagnosticLevel,
    summary: String,
    remediation: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewayDiagnosticReport {
    checks: Vec<DiagnosticCheck>,
    passed: usize,
    warnings: usize,
    errors: usize,
}

#[derive(Clone, Copy)]
struct LocalCliCandidate {
    id: &'static str,
    name: &'static str,
    executable: &'static str,
    supports_provider: bool,
    codetas_provider_id: Option<&'static str>,
}

const LOCAL_CLI_CANDIDATES: &[LocalCliCandidate] = &[
    LocalCliCandidate {
        id: "codex",
        name: "Codex CLI",
        executable: "codex",
        supports_provider: true,
        codetas_provider_id: Some("openai"),
    },
    LocalCliCandidate {
        id: "claude",
        name: "Claude Code",
        executable: "claude",
        supports_provider: true,
        codetas_provider_id: Some("anthropic"),
    },
    LocalCliCandidate {
        id: "grok",
        name: "Grok CLI",
        executable: "grok",
        supports_provider: true,
        codetas_provider_id: Some("xai"),
    },
    LocalCliCandidate {
        id: "agy",
        name: "Antigravity CLI",
        executable: "agy",
        supports_provider: false,
        codetas_provider_id: Some("google-antigravity"),
    },
    LocalCliCandidate {
        id: "qwen",
        name: "Qwen Code",
        executable: "qwen",
        supports_provider: false,
        codetas_provider_id: Some("alibaba-token-plan-intl"),
    },
    LocalCliCandidate {
        id: "kimi",
        name: "Kimi CLI",
        executable: "kimi",
        supports_provider: true,
        codetas_provider_id: Some("kimi"),
    },
    LocalCliCandidate {
        id: "opencode",
        name: "OpenCode",
        executable: "opencode",
        supports_provider: false,
        codetas_provider_id: Some("opencode-go"),
    },
    LocalCliCandidate {
        id: "gemini",
        name: "Gemini CLI",
        executable: "gemini",
        supports_provider: false,
        codetas_provider_id: Some("google"),
    },
    LocalCliCandidate {
        id: "kiro",
        name: "Kiro CLI",
        executable: "kiro",
        supports_provider: false,
        codetas_provider_id: Some("kiro"),
    },
];

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalCliStatus {
    id: String,
    name: String,
    installed: bool,
    executable: Option<String>,
    version: Option<String>,
    probe_state: String,
    message: String,
    can_register: bool,
    needs_codetas_registration: bool,
    codetas_provider_id: Option<String>,
    registration_hint: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalCliScanReport {
    checked_at_ms: u64,
    deep: bool,
    clients: Vec<LocalCliStatus>,
}

#[tauri::command]
pub fn scan_local_cli_clients(app: AppHandle, deep: Option<bool>) -> LocalCliScanReport {
    let registered = load_settings(&app)
        .map(|settings| {
            settings
                .providers
                .iter()
                .filter(|provider| provider.enabled && provider_has_usable_credential(provider))
                .map(|provider| provider.id.clone())
                .collect::<BTreeSet<_>>()
        })
        .unwrap_or_default();
    scan_local_cli_candidates(deep.unwrap_or(false), &registered)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalCliRegistrationReport {
    provider_id: String,
    enabled: bool,
    credential_needed: bool,
    message: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DirectApiTarget {
    provider_id: String,
    name: String,
    hint: String,
}

#[tauri::command]
pub fn list_direct_api_targets() -> Vec<DirectApiTarget> {
    vec![
        DirectApiTarget {
            provider_id: "deepseek".into(),
            name: "DeepSeek API".into(),
            hint: "DeepSeek 公式APIキーを DEEPSEEK_API_KEY または Keychain 参照で保存します。".into(),
        },
        DirectApiTarget {
            provider_id: "zai".into(),
            name: "GLM (Z.AI)".into(),
            hint: "Z.AI Coding Plan の ZAI_API_KEY を Keychain または環境変数で保存します。".into(),
        },
        DirectApiTarget {
            provider_id: "minimax".into(),
            name: "MiniMax".into(),
            hint: "MiniMax Coding Plan の MINIMAX_API_KEY を Keychain または環境変数で保存します。".into(),
        },
    ]
}

#[tauri::command]
pub async fn register_codetas_provider(
    app: AppHandle,
    manager: State<'_, GatewayManager>,
    provider_id: String,
) -> Result<LocalCliRegistrationReport, String> {
    register_provider_in_codetas(app, manager, &provider_id).await
}

#[tauri::command]
pub async fn register_local_cli_in_codetas(
    app: AppHandle,
    manager: State<'_, GatewayManager>,
    client_id: String,
) -> Result<LocalCliRegistrationReport, String> {
    let candidate = LOCAL_CLI_CANDIDATES
        .iter()
        .find(|item| item.id == client_id)
        .ok_or_else(|| format!("未対応のローカルCLIです: {client_id}"))?;
    let provider_id = candidate
        .codetas_provider_id
        .ok_or("このCLIにはCODETAS再登録先がありません")?;
    register_provider_in_codetas(app, manager, provider_id).await
}

async fn register_provider_in_codetas(
    app: AppHandle,
    manager: State<'_, GatewayManager>,
    provider_id: &str,
) -> Result<LocalCliRegistrationReport, String> {
    let previous = load_settings(&app)?;
    let mut settings = previous.clone();
    if settings
        .providers
        .iter()
        .all(|provider| provider.id != provider_id)
    {
        let provider = provider_presets()
            .into_iter()
            .find(|preset| preset.id == provider_id)
            .ok_or_else(|| format!("CODETAS再登録先のpresetがありません: {provider_id}"))?
            .instantiate(None)?;
        settings.providers.push(provider);
    }
    configure_desktop_auth_store(&app)?;
    let needs_native_login = {
        let provider = settings
            .providers
            .iter_mut()
            .find(|provider| provider.id == provider_id)
            .ok_or_else(|| format!("CODETAS再登録先がありません: {provider_id}"))?;
        provider.enabled = true;
        if !provider_has_usable_credential(provider)
            && (detect_local_cli_session_for(provider_id) || has_stored_session(provider_id))
        {
            provider.api_key_env = None;
            provider.credential = oauth_session_credential(provider_id);
        }
        !provider_has_usable_credential(provider) && provider_supports_native_oauth(provider_id)
    };
    if needs_native_login {
        let login = login_provider_oauth(provider_id).await?;
        let provider = settings
            .providers
            .iter_mut()
            .find(|provider| provider.id == provider_id)
            .ok_or_else(|| format!("CODETAS再登録先がありません: {provider_id}"))?;
        provider.api_key_env = None;
        provider.credential = oauth_session_credential(provider_id);
        provider.enabled = true;
        settings.validate()?;
        persist_and_apply_settings(&app, &manager, &previous, &settings).await?;
        return Ok(LocalCliRegistrationReport {
            provider_id: provider_id.to_string(),
            enabled: true,
            credential_needed: false,
            message: login.message,
        });
    }
    let credential_needed = settings
        .providers
        .iter()
        .find(|provider| provider.id == provider_id)
        .is_none_or(|provider| !provider_has_usable_credential(provider));
    settings.validate()?;
    persist_and_apply_settings(&app, &manager, &previous, &settings).await?;
    Ok(LocalCliRegistrationReport {
        provider_id: provider_id.to_string(),
        enabled: true,
        credential_needed,
        message: if credential_needed {
            format!(
                "{provider_id} をCODETASに再登録しました。{}",
                provider_registration_hint(provider_id)
            )
        } else {
            format!("{provider_id} を有効にしました。既存のログインを使います。")
        },
    })
}

fn detect_local_cli_session_for(provider_id: &str) -> bool {
    dirs::home_dir().is_some_and(|home| detect_local_cli_session(provider_id, &home).is_some())
}

fn provider_has_usable_credential(provider: &ProviderDefinition) -> bool {
    match provider.credential.source {
        CredentialSource::None => false,
        CredentialSource::Environment => provider
            .credential
            .reference
            .as_deref()
            .or(provider.api_key_env.as_deref())
            .is_some_and(|name| env::var_os(name).is_some_and(|value| !value.is_empty())),
        CredentialSource::OAuth if provider.credential.command.is_none() => provider
            .credential
            .reference
            .as_deref()
            .is_some_and(|reference| {
                has_stored_session(reference) || detect_local_cli_session_for(reference)
            }),
        CredentialSource::Keychain | CredentialSource::OAuth | CredentialSource::Command => provider
            .credential
            .reference
            .as_deref()
            .or_else(|| {
                provider
                    .credential
                    .command
                    .as_ref()
                    .map(|command| command.program.as_str())
            })
            .is_some_and(|value| !value.trim().is_empty()),
        CredentialSource::Forward => true,
    }
}

#[tauri::command]
pub fn list_provider_presets() -> Vec<ProviderPreset> {
    provider_presets()
}

#[tauri::command]
pub async fn test_gateway_provider(
    app: AppHandle,
    provider_id: String,
) -> Result<ProviderConnectionReport, String> {
    let settings = load_settings(&app)?;
    let provider = settings
        .providers
        .iter()
        .find(|provider| provider.enabled && provider.id == provider_id)
        .ok_or_else(|| "enabled provider was not found".to_string())?;
    Ok(test_provider_connection(provider).await)
}

#[tauri::command]
pub async fn gateway_diagnostics(
    app: AppHandle,
    manager: State<'_, GatewayManager>,
) -> Result<GatewayDiagnosticReport, String> {
    let settings = load_settings(&app)?;
    let mut checks = Vec::new();
    let running = manager.handle.lock().await.is_some();
    checks.push(diagnostic(
        "gateway-running",
        if running {
            DiagnosticLevel::Pass
        } else {
            DiagnosticLevel::Warning
        },
        if running {
            "ローカルゲートウェイは起動中です"
        } else {
            "ローカルゲートウェイは停止中です"
        },
        (!running).then(|| "Connections画面でゲートウェイを起動してください".into()),
    ));
    let enabled = settings
        .providers
        .iter()
        .filter(|provider| provider.enabled)
        .count();
    checks.push(diagnostic(
        "enabled-providers",
        if enabled > 0 {
            DiagnosticLevel::Pass
        } else {
            DiagnosticLevel::Error
        },
        &format!("有効なプロバイダは{enabled}件です"),
        (enabled == 0).then(|| "少なくとも1件のプロバイダを有効にしてください".into()),
    ));
    checks.push(diagnostic(
        "default-provider",
        if settings.default_provider.is_some() {
            DiagnosticLevel::Pass
        } else {
            DiagnosticLevel::Warning
        },
        if settings.default_provider.is_some() {
            "既定プロバイダが設定されています"
        } else {
            "既定プロバイダが未設定です"
        },
        settings
            .default_provider
            .is_none()
            .then(|| "既定プロバイダを選択してください".into()),
    ));

    for provider in settings
        .providers
        .iter()
        .filter(|provider| provider.enabled)
    {
        if let Some(env_key) = provider
            .credential
            .reference
            .as_deref()
            .filter(|_| {
                matches!(
                    provider.credential.source,
                    codetas_gateway::CredentialSource::Environment
                )
            })
            .or(provider.api_key_env.as_deref())
        {
            let available = env::var_os(env_key).is_some_and(|value| !value.is_empty());
            let summary = if available {
                format!("{}の環境変数参照を確認しました", provider.name)
            } else {
                format!("{}の環境変数{env_key}が見つかりません", provider.name)
            };
            checks.push(diagnostic(
                &format!("provider-env-{}", provider.id),
                if available {
                    DiagnosticLevel::Pass
                } else {
                    DiagnosticLevel::Error
                },
                &summary,
                (!available).then(|| format!("CODETASを起動する環境へ{env_key}を設定してください")),
            ));
        }
    }
    if settings.security.require_local_token {
        let available = env::var_os("CODETAS_GATEWAY_TOKEN").is_some_and(|value| !value.is_empty());
        checks.push(diagnostic(
            "local-admission-token",
            if available {
                DiagnosticLevel::Pass
            } else {
                DiagnosticLevel::Error
            },
            if available {
                "ローカル入場トークンを確認しました"
            } else {
                "ローカル入場トークンがありません"
            },
            (!available).then(|| {
                "CODETAS_GATEWAY_TOKENをCodexとCODETASの起動環境へ設定してください".into()
            }),
        ));
    }
    for key in settings
        .security
        .external_access_keys
        .iter()
        .filter(|key| key.enabled)
    {
        let available = env::var_os(&key.env_var).is_some_and(|value| !value.is_empty());
        let expired = key.expires_at_unix.is_some_and(|expires| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_secs() >= expires)
                .unwrap_or(true)
        });
        let summary = if expired {
            format!("外部アクセスキー{}は期限切れです", key.label)
        } else if available {
            format!("外部アクセスキー{}の環境変数参照を確認しました", key.label)
        } else {
            format!(
                "外部アクセスキー{}の環境変数{}が見つかりません",
                key.label, key.env_var
            )
        };
        checks.push(diagnostic(
            &format!("external-access-key-{}", key.id),
            if available && !expired {
                DiagnosticLevel::Pass
            } else {
                DiagnosticLevel::Error
            },
            &summary,
            (!available || expired).then(|| {
                format!(
                    "{}を更新するか、このアクセスキーを無効化してください",
                    key.env_var
                )
            }),
        ));
    }
    if settings.agents.multi_agent_v2 && !settings.security.require_local_token {
        checks.push(diagnostic(
            "subagent-metadata-auth",
            DiagnosticLevel::Warning,
            "サブエージェント用ルーティングは入場認証がないため無効です",
            Some("requireLocalTokenとCODETAS_GATEWAY_TOKENを有効にしてCodex接続設定を更新してください".into()),
        ));
    }
    let codex_configured = codex_gateway_is_configured(&app, &settings)?;
    checks.push(diagnostic(
        "codex-config",
        if codex_configured {
            DiagnosticLevel::Pass
        } else {
            DiagnosticLevel::Warning
        },
        if codex_configured {
            "CodexはCODETAS Gatewayへ接続されています"
        } else {
            "Codex接続は未設定です"
        },
        (!codex_configured).then(|| "既定モデルを選び、Codexへ接続してください".into()),
    ));
    if codex_configured {
        let journal = codex_journal_path(&app)?;
        checks.push(diagnostic(
            "restore-journal",
            if journal.is_file() {
                DiagnosticLevel::Pass
            } else {
                DiagnosticLevel::Error
            },
            if journal.is_file() {
                "安全な復元情報があります"
            } else {
                "Codex設定の復元情報がありません"
            },
            (!journal.is_file())
                .then(|| "Codex接続設定を更新して復元情報を再作成してください".into()),
        ));
    }
    if settings.observability.request_log || settings.observability.usage_log {
        let summary = current_observability_summary(&app, &manager).await?;
        if let Some(error) = summary.persistence_error.as_deref() {
            checks.push(diagnostic(
                "observability-persistence",
                DiagnosticLevel::Error,
                "観測台帳の永続化に失敗しました",
                Some(error.to_string()),
            ));
        }
        checks.push(diagnostic(
            "observability-storage",
            if summary.storage_bytes <= settings.observability.max_storage_bytes {
                DiagnosticLevel::Pass
            } else {
                DiagnosticLevel::Error
            },
            &format!(
                "観測台帳は{}件・{} bytesです",
                summary.total_requests, summary.storage_bytes
            ),
            (summary.storage_bytes > settings.observability.max_storage_bytes)
                .then(|| "観測台帳の容量設定と保存先を確認してください".into()),
        ));
    }
    if settings.runtime.standalone_service {
        let service_status = service::status()?;
        checks.push(diagnostic(
            "gateway-service",
            if service_status.running {
                DiagnosticLevel::Pass
            } else {
                DiagnosticLevel::Error
            },
            &service_status.message,
            (!service_status.running)
                .then(|| "常駐サービスを再起動するか、登録をやり直してください".into()),
        ));
    }

    let passed = checks
        .iter()
        .filter(|check| matches!(check.level, DiagnosticLevel::Pass))
        .count();
    let warnings = checks
        .iter()
        .filter(|check| matches!(check.level, DiagnosticLevel::Warning))
        .count();
    let errors = checks
        .iter()
        .filter(|check| matches!(check.level, DiagnosticLevel::Error))
        .count();
    Ok(GatewayDiagnosticReport {
        checks,
        passed,
        warnings,
        errors,
    })
}

#[tauri::command]
pub async fn gateway_observability_summary(
    app: AppHandle,
    manager: State<'_, GatewayManager>,
) -> Result<ObservabilitySummary, String> {
    current_observability_summary(&app, &manager).await
}

#[tauri::command]
pub fn gateway_observability_breakdown(
    app: AppHandle,
    since_ms: Option<u64>,
    max_events: Option<usize>,
) -> Result<ObservabilityBreakdown, String> {
    Ok(read_observability_breakdown(
        observability_path(&app)?,
        since_ms.unwrap_or(0),
        max_events.unwrap_or(50_000),
    ))
}

#[tauri::command]
pub async fn preview_gateway_observability_cleanup(
    app: AppHandle,
    manager: State<'_, GatewayManager>,
) -> Result<ObservabilityCleanupPreview, String> {
    ensure_observability_offline(&manager).await?;
    let settings = load_settings(&app)?;
    Ok(preview_observability_cleanup(
        observability_path(&app)?,
        now_ms(),
        settings.observability.retention_days,
        settings.observability.max_storage_bytes,
    ))
}

#[tauri::command]
pub async fn trash_gateway_observability_cleanup(
    app: AppHandle,
    manager: State<'_, GatewayManager>,
) -> Result<Option<ObservabilityTrashReport>, String> {
    ensure_observability_offline(&manager).await?;
    let settings = load_settings(&app)?;
    let directory = observability_path(&app)?;
    let preview = preview_observability_cleanup(
        &directory,
        now_ms(),
        settings.observability.retention_days,
        settings.observability.max_storage_bytes,
    );
    trash_observability_cleanup(directory, &preview)
}

#[tauri::command]
pub fn list_gateway_observability_trash(
    app: AppHandle,
) -> Result<Vec<ObservabilityTrashEntry>, String> {
    Ok(list_observability_trash(observability_path(&app)?))
}

#[tauri::command]
pub async fn restore_gateway_observability_trash(
    app: AppHandle,
    manager: State<'_, GatewayManager>,
    transaction_id: String,
) -> Result<ObservabilityTrashReport, String> {
    ensure_observability_offline(&manager).await?;
    restore_observability_trash(observability_path(&app)?, &transaction_id)
}

async fn ensure_observability_offline(manager: &State<'_, GatewayManager>) -> Result<(), String> {
    if manager.handle.lock().await.is_some() || service::status()?.running {
        return Err("観測ストレージを変更する前にGatewayと常駐サービスを停止してください".into());
    }
    Ok(())
}

#[tauri::command]
pub fn start_gateway_debug_scope(
    manager: State<'_, DebugScopeManager>,
    duration_seconds: u64,
) -> Result<DebugScope, String> {
    let duration_seconds = duration_seconds.clamp(10, 900);
    let started_at_ms = now_ms();
    let expires_at_ms = started_at_ms.saturating_add(duration_seconds.saturating_mul(1_000));
    let sequence = DEBUG_SCOPE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let id = format!("debug-{started_at_ms:016x}-{sequence:016x}");
    let mut scopes = manager
        .scopes
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    scopes.retain(|_, scope| scope.expires_at_ms > started_at_ms);
    scopes.insert(
        id.clone(),
        DebugScopeState {
            started_at_ms,
            expires_at_ms,
        },
    );
    Ok(DebugScope {
        id,
        started_at_ms,
        expires_at_ms,
    })
}

#[tauri::command]
pub fn gateway_debug_events(
    app: AppHandle,
    manager: State<'_, DebugScopeManager>,
    scope_id: String,
    limit: usize,
) -> Result<Vec<ObservationEvent>, String> {
    let current = now_ms();
    let started_at_ms = {
        let mut scopes = manager
            .scopes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        scopes.retain(|_, scope| scope.expires_at_ms > current);
        scopes
            .get(&scope_id)
            .map(|scope| scope.started_at_ms)
            .ok_or("debug scope is missing or expired")?
    };
    Ok(read_recent_observability_events(
        observability_path(&app)?,
        started_at_ms,
        limit.clamp(1, 500),
    ))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

#[tauri::command]
pub fn gateway_service_status() -> Result<ServiceStatus, String> {
    service::status()
}

#[tauri::command]
pub async fn install_gateway_service(
    app: AppHandle,
    manager: State<'_, GatewayManager>,
    input: ServiceInstallInput,
) -> Result<ServiceInstallReport, String> {
    if service::status()?.installed {
        return Err("常駐サービスは登録済みです。設定保存時に安全に再起動されます".into());
    }
    let previous = load_settings(&app)?;
    let executable = env::current_exe()
        .map_err(|error| format!("CODETAS実行ファイルを特定できません: {error}"))?;
    let settings_file = settings_path(&app)?;
    let observability_directory = observability_path(&app)?;
    let warnings = service::validate_service_settings(&previous)?;
    let mut settings = previous.clone();
    settings.runtime.standalone_service = true;
    settings.validate()?;
    let catalog_snapshot = snapshot_codex_catalog_transition(&app, &previous, &settings)?;
    save_settings(&app, &settings)?;

    let prior_handle = manager.handle.lock().await.take();
    let was_running = prior_handle.is_some();
    if let Some(handle) = prior_handle {
        handle.shutdown().await;
    }
    let result = service::install(
        &executable,
        &settings_file,
        &observability_directory,
        input.install_shim,
        warnings,
    );
    match result {
        Ok(report) => Ok(report),
        Err(error) => {
            let mut restore_errors =
                restore_settings_and_catalog(&app, &previous, catalog_snapshot.as_ref());
            if was_running {
                match start_managed_gateway(&app, previous).await {
                    Ok(handle) => *manager.handle.lock().await = Some(handle),
                    Err(restore) => {
                        restore_errors.push(format!("埋め込みGateway: {restore}"));
                    }
                }
            }
            if restore_errors.is_empty() {
                Err(format!(
                    "常駐サービスを登録できないため旧状態へ復元しました: {error}"
                ))
            } else {
                Err(format!(
                    "常駐サービスを登録できず、旧状態の復元にも問題がありました: {error}; {}",
                    restore_errors.join("; ")
                ))
            }
        }
    }
}

#[tauri::command]
pub async fn launch_provider_oauth_broker(
    app: AppHandle,
    manager: State<'_, GatewayManager>,
    input: ProviderOAuthInput,
) -> Result<ProviderOAuthLaunchReport, String> {
    let provider_id = input.provider_id.clone();
    let previous = load_settings(&app)?;
    let mut settings = previous.clone();
    let provider_index = settings
        .providers
        .iter()
        .position(|provider| provider.id == provider_id)
        .ok_or_else(|| format!("プロバイダーが見つかりません: {provider_id}"))?;
    let broker = input.broker.as_deref().unwrap_or_else(|| {
        match settings.providers[provider_index].id.as_str() {
            "github-copilot" => "github-cli",
            "google" | "google-vertex" | "google-antigravity" => "gcloud",
            "anthropic" => "claude-cli",
            _ => "",
        }
    });
    configure_desktop_auth_store(&app)?;
    if provider_supports_native_oauth(&provider_id) || broker == "claude-cli" {
        let login = login_provider_oauth(&provider_id).await?;
        settings.providers[provider_index].enabled = true;
        settings.providers[provider_index].api_key_env = None;
        settings.providers[provider_index].credential = oauth_session_credential(&provider_id);
        settings.validate()?;
        persist_and_apply_settings(&app, &manager, &previous, &settings).await?;
        return Ok(ProviderOAuthLaunchReport {
            provider_id,
            broker: if login.imported {
                "local-cli".into()
            } else {
                "codetas-oauth".into()
            },
            launched: true,
            token_command: String::new(),
            instructions: login.message,
        });
    }
    let (program_name, login_args, token_args, refresh_interval_ms) = oauth_broker_spec(broker)?;
    let program = find_program_on_path(program_name)?;
    let credential = ProviderCredential {
        source: CredentialSource::OAuth,
        reference: Some(broker.to_string()),
        transport: CredentialTransport::Bearer,
        header_name: None,
        command: Some(CredentialCommand {
            program: program.to_string_lossy().into_owned(),
            args: token_args.clone(),
            cwd: None,
            timeout_ms: 15_000,
            refresh_interval_ms,
        }),
    };
    // Do not alter the active provider until the detached terminal reports a successful
    // login and a second, output-discarding token command confirms the broker session.
    // Cancelling the browser or closing the terminal therefore leaves the old credential intact.
    run_oauth_terminal_transaction(&app, &program, &login_args, &token_args, broker).await?;
    settings.providers[provider_index].api_key_env = None;
    settings.providers[provider_index].credential = credential;
    settings.validate()?;
    persist_and_apply_settings(&app, &manager, &previous, &settings).await?;
    Ok(ProviderOAuthLaunchReport {
        provider_id,
        broker: broker.to_string(),
        launched: true,
        token_command: format!(
            "{} {}",
            program.display(),
            token_args.join(" ")
        ),
        instructions: format!(
            "{broker}のログインとtoken取得確認が完了しました。秘密は外部CLIが所有し、CODETASには保存していません。"
        ),
    })
}

fn oauth_broker_spec(
    broker: &str,
) -> Result<(&'static str, Vec<String>, Vec<String>, u64), String> {
    match broker {
        "github-cli" => Ok((
            "gh",
            vec![
                "auth".into(),
                "login".into(),
                "--hostname".into(),
                "github.com".into(),
                "--web".into(),
                "--git-protocol".into(),
                "https".into(),
            ],
            vec![
                "auth".into(),
                "token".into(),
                "--hostname".into(),
                "github.com".into(),
            ],
            15 * 60 * 1_000,
        )),
        "gcloud" => Ok((
            "gcloud",
            vec!["auth".into(), "login".into(), "--brief".into()],
            vec!["auth".into(), "print-access-token".into()],
            45 * 60 * 1_000,
        )),
        "" => Err("このプロバイダーには既定のOAuth brokerがありません".into()),
        _ => Err("OAuth brokerはgithub-cli、gcloud、またはclaude-cliを指定してください".into()),
    }
}

fn launch_claude_setup_token_terminal(app: &AppHandle, program: &Path) -> Result<(), String> {
    let directory = app
        .path()
        .app_data_dir()
        .map_err(|error| format!("CODETASデータフォルダを特定できません: {error}"))?
        .join("oauth-broker");
    fs::create_dir_all(&directory)
        .map_err(|error| format!("OAuth起動フォルダを作れません: {error}"))?;
    let script = directory.join("claude-setup-token.sh");
    let body = format!(
        "#!/bin/sh\nset -eu\necho 'Claude Pro/Max の setup-token を起動します。'\necho '表示されたトークンは CLAUDE_CODE_OAUTH_TOKEN に設定するか、CODETAS の Keychain 参照へ保存してください。'\necho\n{} setup-token\necho\nprintf '終わったらこの窓を閉じてください。'\nread -r _\n",
        shell_quote(&program.to_string_lossy())
    );
    fs::write(&script, body).map_err(|error| format!("OAuth起動スクリプトを書けません: {error}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("OAuth起動スクリプトを保護できません: {error}"))?;
    }
    launch_oauth_terminal(&script)
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn find_program_on_path(name: &str) -> Result<PathBuf, String> {
    let path = env::var_os("PATH").ok_or("PATHを取得できません")?;
    for directory in env::split_paths(&path) {
        let candidate = directory.join(name);
        if candidate.is_file() {
            return fs::canonicalize(&candidate)
                .map_err(|error| format!("{}を解決できません: {error}", candidate.display()));
        }
        #[cfg(windows)]
        for extension in ["exe", "cmd", "bat"] {
            let candidate = directory.join(format!("{name}.{extension}"));
            if candidate.is_file() {
                return fs::canonicalize(&candidate)
                    .map_err(|error| format!("{}を解決できません: {error}", candidate.display()));
            }
        }
    }
    Err(format!("{name}がPATH上に見つかりません"))
}

async fn run_oauth_terminal_transaction(
    app: &AppHandle,
    program: &Path,
    login_args: &[String],
    token_args: &[String],
    broker: &str,
) -> Result<(), String> {
    let (script, result) = create_oauth_transaction_script(app, program, login_args, token_args)?;
    if let Err(error) = launch_oauth_terminal(&script) {
        cleanup_oauth_transaction(&script);
        return Err(error);
    }
    let deadline = Instant::now() + Duration::from_secs(20 * 60);
    loop {
        match fs::read_to_string(&result) {
            Ok(value) => {
                cleanup_oauth_transaction(&script);
                return if value.trim() == "ok" {
                    Ok(())
                } else {
                    Err(format!(
                        "{broker}のログインが完了しなかったため設定を変更しませんでした"
                    ))
                };
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                cleanup_oauth_transaction(&script);
                return Err(format!("OAuth結果を確認できません: {error}"));
            }
        }
        if Instant::now() >= deadline {
            cleanup_oauth_transaction(&script);
            return Err(
                "OAuthログインが20分以内に完了しなかったため設定を変更しませんでした".into(),
            );
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

fn cleanup_oauth_transaction(script: &Path) {
    if let Some(directory) = script.parent() {
        let _ = fs::remove_dir_all(directory);
    }
}

#[cfg(unix)]
fn create_oauth_transaction_script(
    app: &AppHandle,
    program: &Path,
    login_args: &[String],
    token_args: &[String],
) -> Result<(PathBuf, PathBuf), String> {
    use std::os::unix::fs::PermissionsExt;
    let directory = app
        .path()
        .app_data_dir()
        .map_err(|error| format!("CODETASデータフォルダを特定できません: {error}"))?
        .join("oauth-broker");
    fs::create_dir_all(&directory)
        .map_err(|error| format!("OAuth起動フォルダを作れません: {error}"))?;
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("OAuth起動フォルダを保護できません: {error}"))?;
    let nonce = format!(
        "{}-{}-{}",
        now_ms(),
        std::process::id(),
        OAUTH_TRANSACTION_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    );
    let transaction = directory.join(nonce);
    fs::create_dir(&transaction)
        .map_err(|error| format!("OAuth取引フォルダを作れません: {error}"))?;
    fs::set_permissions(&transaction, fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("OAuth取引フォルダを保護できません: {error}"))?;
    let extension = if cfg!(target_os = "macos") {
        "command"
    } else {
        "sh"
    };
    let script = transaction.join(format!("login.{extension}"));
    let result = transaction.join("result.status");
    let mut login = shell_quote_value(&program.to_string_lossy());
    for argument in login_args {
        login.push(' ');
        login.push_str(&shell_quote_value(argument));
    }
    let mut verify = shell_quote_value(&program.to_string_lossy());
    for argument in token_args {
        verify.push(' ');
        verify.push_str(&shell_quote_value(argument));
    }
    let content = format!(
        "#!/bin/sh\n# CODETAS-OAUTH-BROKER-V2\numask 077\n{login}\nstatus=$?\nif [ \"$status\" -eq 0 ]; then\n  {verify} >/dev/null 2>&1\n  status=$?\nfi\nif [ \"$status\" -eq 0 ]; then\n  printf 'ok\\n' > {}\n  printf '\\nCODETAS: login verified. You can close this terminal.\\n'\nelse\n  printf 'failed\\n' > {}\n  printf '\\nCODETAS: login was not applied. You can close this terminal.\\n'\nfi\nexit \"$status\"\n",
        shell_quote_value(&result.to_string_lossy()),
        shell_quote_value(&result.to_string_lossy()),
    );
    atomic_write(&script, content.as_bytes())?;
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("OAuth起動スクリプトを保護できません: {error}"))?;
    Ok((script, result))
}

#[cfg(target_os = "windows")]
fn create_oauth_transaction_script(
    app: &AppHandle,
    program: &Path,
    login_args: &[String],
    token_args: &[String],
) -> Result<(PathBuf, PathBuf), String> {
    let directory = app
        .path()
        .app_data_dir()
        .map_err(|error| format!("CODETASデータフォルダを特定できません: {error}"))?
        .join("oauth-broker");
    fs::create_dir_all(&directory)
        .map_err(|error| format!("OAuth起動フォルダを作れません: {error}"))?;
    let nonce = format!(
        "{}-{}-{}",
        now_ms(),
        std::process::id(),
        OAUTH_TRANSACTION_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    );
    let transaction = directory.join(nonce);
    fs::create_dir(&transaction)
        .map_err(|error| format!("OAuth取引フォルダを作れません: {error}"))?;
    let script = transaction.join("login.ps1");
    let result = transaction.join("result.status");
    let login = powershell_invocation(program, login_args);
    let verify = powershell_invocation(program, token_args);
    let content = format!(
        "# CODETAS-OAUTH-BROKER-V2\r\n{login}\r\n$status = $LASTEXITCODE\r\nif ($status -eq 0) {{\r\n  {verify} *> $null\r\n  $status = $LASTEXITCODE\r\n}}\r\nif ($status -eq 0) {{\r\n  Set-Content -LiteralPath {} -Value 'ok' -NoNewline\r\n  Write-Host '\nCODETAS: login verified. You can close this terminal.'\r\n}} else {{\r\n  Set-Content -LiteralPath {} -Value 'failed' -NoNewline\r\n  Write-Host '\nCODETAS: login was not applied. You can close this terminal.'\r\n}}\r\nexit $status\r\n",
        powershell_quote_value(&result.to_string_lossy()),
        powershell_quote_value(&result.to_string_lossy()),
    );
    atomic_write(&script, content.as_bytes())?;
    Ok((script, result))
}

#[cfg(target_os = "macos")]
fn launch_oauth_terminal(script: &Path) -> Result<(), String> {
    let launch = Command::new("/usr/bin/open")
        .args(["-a", "Terminal"])
        .arg(script)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    match launch {
        Ok(_) => Ok(()),
        Err(error) => Err(format!("Terminalを起動できません: {error}")),
    }
}

#[cfg(target_os = "linux")]
fn launch_oauth_terminal(script: &Path) -> Result<(), String> {
    let candidates: [(&str, &[&str]); 4] = [
        ("x-terminal-emulator", &["-e"]),
        ("gnome-terminal", &["--"]),
        ("konsole", &["-e"]),
        ("xterm", &["-e"]),
    ];
    let mut attempted = Vec::new();
    for (terminal, prefix) in candidates {
        let mut command = Command::new(terminal);
        command
            .args(prefix)
            .arg(script)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        match command.spawn() {
            Ok(_) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                attempted.push(terminal);
            }
            Err(error) => return Err(format!("{terminal}を起動できません: {error}")),
        }
    }
    Err(format!(
        "OAuth用ターミナルが見つかりません: {}",
        attempted.join(", ")
    ))
}

#[cfg(target_os = "windows")]
fn launch_oauth_terminal(script: &Path) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    Command::new("powershell.exe")
        .creation_flags(0x0000_0010)
        .args(["-NoLogo", "-NoExit", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(script)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("OAuthターミナルを起動できません: {error}"))
}

#[cfg(unix)]
fn shell_quote_value(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(target_os = "windows")]
fn powershell_invocation(program: &Path, args: &[String]) -> String {
    let mut output = format!("& {}", powershell_quote_value(&program.to_string_lossy()));
    for argument in args {
        output.push(' ');
        output.push_str(&powershell_quote_value(argument));
    }
    output
}

#[cfg(target_os = "windows")]
fn powershell_quote_value(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

#[tauri::command]
pub fn start_gateway_service() -> Result<ServiceStatus, String> {
    if !service::start()? {
        return Err("常駐Gatewayの起動状態を確認できません".into());
    }
    service::status()
}

#[tauri::command]
pub fn restart_gateway_service() -> Result<ServiceStatus, String> {
    if !service::restart()? {
        return Err("常駐Gatewayの再起動状態を確認できません".into());
    }
    service::status()
}

#[tauri::command]
pub fn stop_gateway_service() -> Result<ServiceStatus, String> {
    if !service::stop()? {
        return Err("常駐Gatewayを停止できませんでした".into());
    }
    let status = service::status()?;
    if status.running {
        return Err("常駐Gatewayが停止状態になっていません".into());
    }
    Ok(status)
}

#[tauri::command]
pub async fn uninstall_gateway_service(
    app: AppHandle,
    manager: State<'_, GatewayManager>,
) -> Result<ServiceUninstallReport, String> {
    let previous = load_settings(&app)?;
    let mut settings = previous.clone();
    settings.runtime.standalone_service = false;
    settings.validate()?;
    let catalog_snapshot = snapshot_codex_catalog_transition(&app, &previous, &settings)?;
    save_settings(&app, &settings)?;
    let report = match service::uninstall() {
        Ok(report) => report,
        Err(error) => {
            let restore_errors =
                restore_settings_and_catalog(&app, &previous, catalog_snapshot.as_ref());
            return Err(if restore_errors.is_empty() {
                format!("常駐サービスを解除できないため旧設定へ復元しました: {error}")
            } else {
                format!(
                    "常駐サービスを解除できず、旧状態の復元にも失敗しました: {error}; {}",
                    restore_errors.join("; ")
                )
            });
        }
    };
    if settings.runtime.auto_start {
        let handle = start_managed_gateway(&app, settings).await?;
        *manager.handle.lock().await = Some(handle);
    }
    Ok(report)
}

#[tauri::command]
pub async fn sync_client_integrations(
    app: AppHandle,
    manager: State<'_, GatewayManager>,
    input: ExternalClientIntegrationInput,
) -> Result<ClientIntegrationReport, String> {
    let previous = load_settings(&app)?;
    let mut settings = previous.clone();
    let integrations = ClientIntegrationSettings {
        codex: settings.integrations.codex,
        claude_code: input.claude_code,
        claude_desktop: input.claude_desktop,
        opencode: input.opencode,
        grok: input.grok,
        pi: input.pi,
        ..settings.integrations.clone()
    };
    clients::apply_switches(&mut settings, integrations);
    clients::reconcile_claude_desktop_profile(&mut settings)?;
    settings.validate()?;
    let data_directory = app
        .path()
        .app_data_dir()
        .map_err(|error| format!("CODETASデータフォルダを特定できません: {error}"))?;
    let executable = env::current_exe()
        .map_err(|error| format!("CODETAS実行ファイルを特定できません: {error}"))?;
    let runtime_url = runtime_gateway_url(&app);
    let report = match clients::sync(
        &data_directory,
        &executable,
        &settings,
        runtime_url.as_deref(),
    ) {
        Ok(report) => report,
        Err(error) => {
            let restore = clients::sync(
                &data_directory,
                &executable,
                &previous,
                runtime_url.as_deref(),
            );
            return Err(match restore {
                Ok(_) => format!("クライアント連携を更新できないため旧成果物へ復元しました: {error}"),
                Err(restore) => format!(
                    "クライアント連携を更新できず、旧成果物の復元にも失敗しました: {error}; {restore}"
                ),
            });
        }
    };
    if let Err(error) = persist_and_apply_settings(&app, &manager, &previous, &settings).await {
        let _ = clients::sync(
            &data_directory,
            &executable,
            &previous,
            runtime_url.as_deref(),
        );
        return Err(error);
    }
    Ok(report)
}

async fn current_observability_summary(
    app: &AppHandle,
    manager: &State<'_, GatewayManager>,
) -> Result<ObservabilitySummary, String> {
    let guard = manager.handle.lock().await;
    if let Some(handle) = guard.as_ref() {
        return Ok(handle.observability_summary());
    }
    Ok(read_observability_summary(observability_path(app)?))
}

fn diagnostic(
    id: &str,
    level: DiagnosticLevel,
    summary: &str,
    remediation: Option<String>,
) -> DiagnosticCheck {
    DiagnosticCheck {
        id: id.into(),
        level,
        summary: summary.into(),
        remediation,
    }
}

#[tauri::command]
pub async fn install_provider_preset(
    app: AppHandle,
    manager: State<'_, GatewayManager>,
    input: PresetInstallInput,
) -> Result<GatewayStatus, String> {
    let preset = provider_presets()
        .into_iter()
        .find(|preset| preset.id == input.preset_id)
        .ok_or_else(|| format!("unknown provider preset: {}", input.preset_id))?;
    let provider = preset.instantiate(input.base_url.as_deref())?;
    let provider_id = provider.id.clone();
    let previous = load_settings(&app)?;
    let mut settings = previous.clone();
    if let Some(existing) = settings
        .providers
        .iter_mut()
        .find(|item| item.id == provider_id)
    {
        *existing = provider;
    } else {
        settings.providers.push(provider);
    }
    if input.make_default || settings.default_provider.is_none() {
        settings.default_provider = Some(provider_id);
    }
    clients::reconcile_claude_desktop_profile(&mut settings)?;
    settings.validate()?;
    persist_and_apply_settings(&app, &manager, &previous, &settings).await?;
    let running = runtime_is_running(&manager, &settings).await?;
    status(&app, running, settings)
}

#[tauri::command]
pub async fn refresh_gateway_provider_models(
    app: AppHandle,
    manager: State<'_, GatewayManager>,
    provider_id: String,
) -> Result<GatewaySettings, String> {
    let previous = load_settings(&app)?;
    let mut settings = previous.clone();
    let provider = settings
        .providers
        .iter()
        .find(|provider| provider.id == provider_id && provider.enabled)
        .cloned()
        .ok_or_else(|| "enabled provider was not found".to_string())?;
    let discovered = discover_provider_models(&provider)
        .await
        .map_err(|error| error.to_string())?;
    let mut merged = settings
        .model_catalog
        .iter()
        .filter(|model| model.provider_id == provider_id)
        .cloned()
        .map(|model| (model.model_id.clone(), model))
        .collect::<BTreeMap<_, _>>();
    for model in discovered {
        merged.entry(model.model_id.clone()).or_insert(model);
    }
    let discovered_ids = merged.keys().cloned().collect::<Vec<_>>();

    settings
        .model_catalog
        .retain(|model| model.provider_id != provider_id);
    settings.model_catalog.extend(merged.into_values());
    if let Some(current) = settings
        .providers
        .iter_mut()
        .find(|item| item.id == provider_id)
    {
        current.models = discovered_ids;
    }
    clients::reconcile_claude_desktop_profile(&mut settings)?;
    settings.validate()?;
    persist_and_apply_settings(&app, &manager, &previous, &settings).await?;
    Ok(settings)
}

#[tauri::command]
pub fn sync_codex_model_catalog(app: AppHandle) -> Result<String, String> {
    let previous = load_settings(&app)?;
    let settings = previous.clone();
    let path = codex_home()?.join("codetas-model-catalog.json");
    let previous_catalog = read_optional_file(&path)?;
    let configured = codex_gateway_is_configured(&app, &settings)?;
    let journal_path = codex_journal_path(&app)?;
    let journal = read_codex_journal(&journal_path)?
        .ok_or("Codex接続を先に適用し、CODETASの所有情報を作成してください")?;
    validate_codex_journal_paths(&app, &journal)?;
    let config_path = codex_config_path()?;
    let previous_config = configured
        .then(|| read_optional_file(&config_path))
        .transpose()?
        .flatten();
    let path = write_codex_catalog(&settings)?;
    let result = (|| {
        if configured {
            set_codex_catalog_path(&path)?;
        }
        save_settings(&app, &settings)
    })();
    if let Err(error) = result {
        let mut restore_errors = Vec::new();
        if let Err(restore) = save_settings(&app, &previous) {
            restore_errors.push(format!("旧設定: {restore}"));
        }
        if configured {
            if let Err(restore) = restore_optional_file(&config_path, previous_config.as_deref()) {
                restore_errors.push(format!("旧Codex設定: {restore}"));
            }
        }
        if let Err(restore) = restore_optional_file(&path, previous_catalog.as_deref()) {
            restore_errors.push(format!("旧カタログ: {restore}"));
        }
        return Err(if restore_errors.is_empty() {
            format!("Codexモデルカタログを同期できないため旧状態へ復元しました: {error}")
        } else {
            format!(
                "Codexモデルカタログを同期できず、旧状態の復元にも問題がありました: {error}; {}",
                restore_errors.join("; ")
            )
        });
    }
    Ok(path.to_string_lossy().into_owned())
}

#[tauri::command]
pub fn gateway_configuration(app: AppHandle) -> Result<GatewaySettings, String> {
    load_settings(&app)
}

#[tauri::command]
pub async fn check_for_codetas_update(app: AppHandle) -> Result<UpdateCheck, String> {
    let settings = load_settings(&app)?;
    check_signed_update(&settings.updates, env!("CARGO_PKG_VERSION")).await
}

#[tauri::command]
pub async fn install_codetas_update(
    app: AppHandle,
    manager: State<'_, GatewayManager>,
) -> Result<(), String> {
    let settings = load_settings(&app)?;
    let signed = check_signed_update(&settings.updates, env!("CARGO_PKG_VERSION")).await?;
    if !signed.update_available {
        return Err("利用できるCODETAS更新はありません".into());
    }
    let endpoint = settings
        .updates
        .installer_endpoint
        .as_deref()
        .ok_or("更新インストーラのendpointが設定されていません")?;
    let installer_public_key = settings
        .updates
        .installer_public_key
        .as_deref()
        .ok_or("更新インストーラの公開鍵が設定されていません")?;
    let endpoint = tauri::Url::parse(endpoint)
        .map_err(|_| "更新インストーラのendpointが不正です".to_string())?;
    let updater = app
        .updater_builder()
        .endpoints(vec![endpoint])
        .map_err(|error| format!("更新endpointを構成できません: {error}"))?
        .pubkey(installer_public_key)
        .build()
        .map_err(|error| format!("更新検証器を構成できません: {error}"))?;
    let update = updater
        .check()
        .await
        .map_err(|error| format!("更新インストーラを確認できません: {error}"))?
        .ok_or("署名済みmanifestと一致する更新インストーラがありません")?;
    if update.version != signed.manifest.version {
        return Err("更新manifestとインストーラのバージョンが一致しません".into());
    }
    let expected_url = tauri::Url::parse(&signed.manifest.download_url)
        .map_err(|_| "署名済み更新URLが不正です".to_string())?;
    if update.download_url != expected_url {
        return Err("更新manifestとインストーラのダウンロードURLが一致しません".into());
    }

    let bytes = update
        .download(|_, _| {}, || {})
        .await
        .map_err(|error| format!("署名済み更新をダウンロードできません: {error}"))?;
    if bytes.len() as u64 != signed.manifest.artifact_size_bytes {
        return Err("更新ファイルのサイズが署名済みmanifestと一致しません".into());
    }
    let digest = Sha256::digest(&bytes);
    let actual_sha256 = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    if !actual_sha256.eq_ignore_ascii_case(&signed.manifest.sha256) {
        return Err("更新ファイルのSHA-256が署名済みmanifestと一致しません".into());
    }

    let prior_handle = manager.handle.lock().await.take();
    let embedded_was_running = prior_handle.is_some();
    if let Some(handle) = prior_handle {
        handle.shutdown().await;
    }
    let service_was_running = settings.runtime.standalone_service && service::status()?.running;
    if service_was_running && !service::stop()? {
        if embedded_was_running {
            let handle = start_managed_gateway(&app, settings.clone()).await?;
            *manager.handle.lock().await = Some(handle);
        }
        return Err("更新前に常駐Gatewayを停止できませんでした".into());
    }

    if let Err(error) = update.install(&bytes) {
        let mut restore_errors = Vec::new();
        if service_was_running {
            match service::start() {
                Ok(true) => {}
                Ok(false) => {
                    restore_errors.push("常駐Gateway: 起動操作後の稼働状態を確認できません".into())
                }
                Err(restore_error) => {
                    restore_errors.push(format!("常駐Gateway: {restore_error}"));
                }
            }
        } else if embedded_was_running {
            match start_managed_gateway(&app, settings).await {
                Ok(handle) => *manager.handle.lock().await = Some(handle),
                Err(restore_error) => {
                    restore_errors.push(format!("埋め込みGateway: {restore_error}"))
                }
            }
        }
        let restore = if restore_errors.is_empty() {
            "停止したGatewayは復元しました".to_string()
        } else {
            format!("Gateway復元エラー: {}", restore_errors.join("; "))
        };
        return Err(format!("更新を適用できません: {error}; {restore}"));
    }

    app.restart();
}

#[tauri::command]
pub async fn save_gateway_configuration(
    app: AppHandle,
    manager: State<'_, GatewayManager>,
    mut configuration: GatewaySettings,
) -> Result<GatewaySettings, String> {
    // Provider/route edits can invalidate generated Claude Desktop aliases. Rebuild the
    // owned profile before validation so stale aliases never make the settings impossible
    // to save or repair.
    clients::reconcile_claude_desktop_profile(&mut configuration)?;
    configuration.validate()?;
    let previous = load_settings(&app)?;
    let catalog_snapshot = snapshot_codex_catalog_transition(&app, &previous, &configuration)?;
    if previous.runtime.standalone_service != configuration.runtime.standalone_service {
        return Err("常駐サービスの切替は専用の登録・解除操作を使用してください".into());
    }
    if previous.runtime.standalone_service {
        service::validate_service_settings(&configuration)?;
        save_settings(&app, &configuration)?;
        if let Some(handle) = manager.handle.lock().await.take() {
            handle.shutdown().await;
        }
        if let Err(error) = service::restart().and_then(|running| {
            running
                .then_some(())
                .ok_or_else(|| "再起動後の起動状態を確認できません".to_string())
        }) {
            let mut restore_errors =
                restore_settings_and_catalog(&app, &previous, catalog_snapshot.as_ref());
            let restored = service::restart().and_then(|running| {
                running
                    .then_some(())
                    .ok_or_else(|| "旧設定での再起動後も稼働状態を確認できません".to_string())
            });
            if let Err(restore) = restored {
                restore_errors.push(format!("旧設定での再起動: {restore}"));
            }
            return Err(if restore_errors.is_empty() {
                format!("常駐サービスを再起動できないため旧設定へ復元しました: {error}")
            } else {
                format!(
                    "常駐サービスを再起動できず、旧状態の復元にも失敗しました: {error}; {}",
                    restore_errors.join("; ")
                )
            });
        }
        return Ok(configuration);
    }
    let endpoint_changed = previous.runtime.port != configuration.runtime.port
        || previous.runtime.host != configuration.runtime.host;
    if endpoint_changed {
        let previous_handle = manager.handle.lock().await.take();
        let was_running = previous_handle.is_some();
        if let Some(handle) = previous_handle {
            handle.shutdown().await;
        }
        if let Err(error) = save_settings(&app, &configuration) {
            if was_running {
                match start_managed_gateway(&app, previous).await {
                    Ok(handle) => *manager.handle.lock().await = Some(handle),
                    Err(restore) => {
                        return Err(format!(
                            "設定ファイルを保存できず、旧Gatewayの復元にも失敗しました: {error}; {restore}"
                        ));
                    }
                }
            }
            return Err(error);
        }
        if was_running {
            match start_managed_gateway(&app, configuration.clone()).await {
                Ok(handle) => *manager.handle.lock().await = Some(handle),
                Err(error) => {
                    let mut restore_errors =
                        restore_settings_and_catalog(&app, &previous, catalog_snapshot.as_ref());
                    let restore = start_managed_gateway(&app, previous).await;
                    return Err(match restore {
                        Ok(handle) => {
                            *manager.handle.lock().await = Some(handle);
                            if restore_errors.is_empty() {
                                format!("新しいGateway endpointを起動できないため旧設定へ復元しました: {error}")
                            } else {
                                format!("新しいGateway endpointを起動できず、旧永続状態の復元にも失敗しました: {error}; {}", restore_errors.join("; "))
                            }
                        }
                        Err(restore) => {
                            restore_errors.push(format!("旧Gateway: {restore}"));
                            format!("新しいGateway endpointを起動できず、旧状態の復元にも失敗しました: {error}; {}", restore_errors.join("; "))
                        }
                    });
                }
            }
        }
        return Ok(configuration);
    }

    save_settings(&app, &configuration)?;
    let guard = manager.handle.lock().await;
    if let Some(handle) = guard.as_ref() {
        if let Err(error) = handle.set_settings(configuration.clone()).await {
            let mut restore_errors =
                restore_settings_and_catalog(&app, &previous, catalog_snapshot.as_ref());
            let restore = handle.set_settings(previous).await;
            if let Err(restore) = restore {
                restore_errors.push(format!("旧runtime設定: {restore}"));
            }
            return Err(if restore_errors.is_empty() {
                format!("設定を反映できないため旧設定へ復元しました: {error}")
            } else {
                format!(
                    "設定を反映できず、旧状態の復元にも失敗しました: {error}; {}",
                    restore_errors.join("; ")
                )
            });
        }
    }
    Ok(configuration)
}

pub fn converge_codex_integration(app: AppHandle) -> Result<String, String> {
    let settings = load_settings(&app)?;
    if !settings.codex.auto_connect {
        return Ok("Codex auto-connection is disabled".into());
    }
    install_codex_gateway_config(app, CodexGatewayInstallInput { model: None })
}

#[tauri::command]
pub async fn start_provider_gateway(
    app: AppHandle,
    manager: State<'_, GatewayManager>,
) -> Result<GatewayStatus, String> {
    // Manual starts also converge Codex in case its config changed while CODETAS
    // was already running. Normal app startup performs this synchronously in
    // `setup`, before the asynchronous gateway task is spawned.
    if let Err(error) = converge_codex_integration(app.clone()) {
        eprintln!("CODETAS: Codex automatic connection was skipped: {error}");
    }
    let settings = load_settings(&app)?;
    let mut guard = manager.handle.lock().await;
    if settings.runtime.standalone_service {
        let prior_handle = guard.take();
        drop(guard);
        if let Some(handle) = prior_handle {
            handle.shutdown().await;
        }
        let service_status = service::status()?;
        let running = service_status.running || (service_status.installed && service::start()?);
        return status(&app, running, settings);
    }
    if guard.is_none() {
        let handle = start_managed_gateway(&app, settings.clone()).await?;
        *guard = Some(handle);
    } else if let Some(handle) = guard.as_ref() {
        handle.set_settings(settings.clone()).await?;
    }
    status(&app, true, settings)
}

#[tauri::command]
pub async fn stop_provider_gateway(
    app: AppHandle,
    manager: State<'_, GatewayManager>,
) -> Result<GatewayStatus, String> {
    let settings = load_settings(&app)?;
    let running = if settings.runtime.standalone_service {
        if service::status()?.installed {
            if !service::stop()? {
                return Err("常駐Gatewayを停止できませんでした".into());
            }
            let running = service::status()?.running;
            if running {
                return Err("常駐Gatewayが停止状態になっていません".into());
            }
            false
        } else {
            false
        }
    } else {
        if let Some(handle) = manager.handle.lock().await.take() {
            handle.shutdown().await;
        }
        false
    };
    status(&app, running, settings)
}

pub async fn shutdown_embedded_gateway(manager: &GatewayManager) {
    if let Some(handle) = manager.handle.lock().await.take() {
        handle.shutdown().await;
    }
}

#[tauri::command]
pub async fn provider_gateway_status(
    app: AppHandle,
    manager: State<'_, GatewayManager>,
) -> Result<GatewayStatus, String> {
    let guard = manager.handle.lock().await;
    let embedded_running = guard.is_some();
    let settings = if let Some(handle) = guard.as_ref() {
        handle.settings().await
    } else {
        load_settings(&app)?
    };
    drop(guard);
    let running =
        embedded_running || (settings.runtime.standalone_service && service::status()?.running);
    status(&app, running, settings)
}

#[tauri::command]
pub async fn upsert_gateway_provider(
    app: AppHandle,
    manager: State<'_, GatewayManager>,
    input: ProviderUpsertInput,
) -> Result<GatewayStatus, String> {
    input.provider.validate()?;
    let previous = load_settings(&app)?;
    let mut settings = previous.clone();
    let provider_id = input.provider.id.clone();
    if let Some(existing) = settings
        .providers
        .iter_mut()
        .find(|provider| provider.id == provider_id)
    {
        *existing = input.provider;
    } else {
        settings.providers.push(input.provider);
    }
    if input.make_default || settings.default_provider.is_none() {
        settings.default_provider = Some(provider_id);
    }
    clients::reconcile_claude_desktop_profile(&mut settings)?;
    settings.validate()?;
    persist_and_apply_settings(&app, &manager, &previous, &settings).await?;
    let running = runtime_is_running(&manager, &settings).await?;
    status(&app, running, settings)
}

#[tauri::command]
pub async fn remove_gateway_provider(
    app: AppHandle,
    manager: State<'_, GatewayManager>,
    provider_id: String,
) -> Result<GatewayStatus, String> {
    let previous = load_settings(&app)?;
    let mut settings = previous.clone();
    let before = settings.providers.len();
    settings
        .providers
        .retain(|provider| provider.id != provider_id);
    if settings.providers.len() == before {
        return Err("provider was not found".into());
    }
    if settings.default_provider.as_deref() == Some(provider_id.as_str()) {
        settings.default_provider = settings
            .providers
            .iter()
            .find(|provider| provider.enabled)
            .map(|provider| provider.id.clone());
    }
    clients::reconcile_claude_desktop_profile(&mut settings)?;
    settings.validate()?;
    persist_and_apply_settings(&app, &manager, &previous, &settings).await?;
    let running = runtime_is_running(&manager, &settings).await?;
    status(&app, running, settings)
}

#[tauri::command]
pub async fn set_default_gateway_provider(
    app: AppHandle,
    manager: State<'_, GatewayManager>,
    provider_id: String,
) -> Result<GatewayStatus, String> {
    let previous = load_settings(&app)?;
    let mut settings = previous.clone();
    if !settings
        .providers
        .iter()
        .any(|provider| provider.id == provider_id && provider.enabled)
    {
        return Err("default provider must reference an enabled provider".into());
    }
    settings.default_provider = Some(provider_id);
    settings.validate()?;
    persist_and_apply_settings(&app, &manager, &previous, &settings).await?;
    let running = runtime_is_running(&manager, &settings).await?;
    status(&app, running, settings)
}

#[tauri::command]
pub fn install_codex_gateway_config(
    app: AppHandle,
    input: CodexGatewayInstallInput,
) -> Result<String, String> {
    let mut settings = load_settings(&app)?;
    let previous_settings = settings.clone();
    if settings.security.require_local_token {
        return Err(
            "Codexの履歴を維持する接続方式ではローカルトークンを使用できません。Gatewayをloopback限定にしてrequireLocalTokenを無効にしてください"
                .into(),
        );
    }
    if !matches!(
        settings.runtime.host.as_str(),
        "localhost" | "127.0.0.1" | "::1"
    ) {
        return Err(
            "Codexの履歴を維持する接続方式はlocalhost、127.0.0.1、または::1でのみ使用できます"
                .into(),
        );
    }
    enable_codex_openai_passthrough(&mut settings)?;
    auto_register_authenticated_cli_providers(&mut settings)?;
    let gateway_base_url = runtime_gateway_url(&app).unwrap_or_else(|| gateway_url(&settings));
    let config_path = codex_config_path()?;
    let original_config = read_optional_file(&config_path)?;
    let existing = match fs::read_to_string(&config_path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(format!("Codex設定を読めません: {error}")),
    };
    let mut document = if existing.trim().is_empty() {
        DocumentMut::new()
    } else {
        existing
            .parse::<DocumentMut>()
            .map_err(|error| format!("既存のCodex設定を解析できません: {error}"))?
    };

    let requested_model = input
        .model
        .as_deref()
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .map(codex_public_model_id);
    let existing_model = document
        .get("model")
        .and_then(Item::as_str)
        .map(str::trim)
        .filter(|model| !model.is_empty());
    let model = select_codex_model(&settings, requested_model.as_deref(), existing_model)
        .ok_or("Codexへ接続するには有効なモデルが必要です")?;
    if model.len() > 300 || model.chars().any(char::is_control) {
        return Err("model must be a printable value up to 300 characters".into());
    }
    settings.codex.auto_sync_catalog = true;
    settings.codex.auto_connect = true;
    let installed_agents = if settings.agents.multi_agent_v2 {
        let installed = CodexAgentInstall {
            enabled: true,
            max_concurrent_threads_per_session: i64::from(settings.agents.max_threads),
        };
        if !document.contains_key("agents") {
            document["agents"] = Item::Table(Table::new());
        }
        let agents = document["agents"]
            .as_table_mut()
            .ok_or("agents must be a TOML table")?;
        agents["enabled"] = value(installed.enabled);
        agents["max_concurrent_threads_per_session"] =
            value(installed.max_concurrent_threads_per_session);
        Some(installed)
    } else {
        restore_agents_before_disable(&app, &mut document)?;
        None
    };
    let existing_provider = document.get("model_provider").and_then(Item::as_str);
    if existing_provider
        .is_some_and(|provider| provider != "openai" && provider != GATEWAY_PROVIDER_ID)
    {
        return Err(format!(
            "既存の外部model_providerを上書きしません: {}",
            existing_provider.unwrap_or_default()
        ));
    }
    let catalog_path = codex_home()?.join("codetas-model-catalog.json");
    let original_catalog = read_optional_file(&catalog_path)?;
    let catalog_content = serialize_codex_catalog(&settings, Some(&model))?;
    configure_native_codex_gateway(&mut document, &gateway_base_url, &model, &catalog_path)?;

    if let Some(parent) = config_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("Codex設定フォルダを作れません: {error}"))?;
    }
    let installed_base_url = gateway_base_url;
    let journal_path = codex_journal_path(&app)?;
    let original_journal = read_optional_file(&journal_path)?;
    let mut created_backups = Vec::new();
    let transaction = (|| {
        created_backups = prepare_codex_journal(
            &app,
            &config_path,
            &catalog_path,
            &model,
            &installed_base_url,
            false,
            CodexRoutingMode::OpenAiBaseUrl,
            installed_agents,
        )?;
        atomic_write(&catalog_path, &catalog_content)?;
        atomic_write(&config_path, document.to_string().as_bytes())?;
        save_settings(&app, &settings)
    })();
    if let Err(error) = transaction {
        let mut restore_errors = Vec::new();
        for (label, path, snapshot) in [
            ("Codex設定", &config_path, original_config.as_deref()),
            ("復元journal", &journal_path, original_journal.as_deref()),
        ] {
            if let Err(restore) = restore_optional_file(path, snapshot) {
                restore_errors.push(format!("{label}: {restore}"));
            }
        }
        if let Err(restore) = write_settings_only(&app, &previous_settings) {
            restore_errors.push(format!("CODETAS設定: {restore}"));
        }
        if let Err(restore) = restore_optional_file(&catalog_path, original_catalog.as_deref()) {
            restore_errors.push(format!("モデルカタログ: {restore}"));
        }
        for backup in created_backups {
            match fs::remove_file(backup) {
                Ok(()) => {}
                Err(remove) if remove.kind() == std::io::ErrorKind::NotFound => {}
                Err(remove) => {
                    restore_errors.push(format!("新規バックアップ: {remove}"));
                }
            }
        }
        return Err(if restore_errors.is_empty() {
            format!("Codex接続を適用できないため旧状態へ復元しました: {error}")
        } else {
            format!(
                "Codex接続を適用できず、旧状態の復元にも問題がありました: {error}; {}",
                restore_errors.join("; ")
            )
        });
    }
    Ok(config_path.to_string_lossy().into_owned())
}

fn select_codex_model(
    settings: &GatewaySettings,
    requested: Option<&str>,
    existing: Option<&str>,
) -> Option<String> {
    requested
        .filter(|model| is_codex_catalog_model(settings, model))
        .or_else(|| existing.filter(|model| is_codex_catalog_model(settings, model)))
        .map(str::to_string)
        .or_else(|| {
            settings
                .providers
                .iter()
                .find(|provider| provider.id == "openai" && provider.enabled)
                .and_then(|provider| {
                    provider
                        .default_model
                        .as_deref()
                        .filter(|model| provider.models.iter().any(|candidate| candidate == model))
                        .or_else(|| provider.models.first().map(String::as_str))
                        .map(str::to_string)
                })
        })
}

fn is_codex_catalog_model(settings: &GatewaySettings, model: &str) -> bool {
    build_codex_catalog(settings)
        .models
        .iter()
        .any(|entry| entry.get("slug").and_then(JsonValue::as_str) == Some(model))
}

fn codex_public_model_id(model: &str) -> String {
    model
        .strip_prefix("openai/")
        .filter(|model| !model.is_empty())
        .unwrap_or(model)
        .to_string()
}

#[tauri::command]
pub fn restore_codex_gateway_config(app: AppHandle) -> Result<CodexRestoreReport, String> {
    let journal_path = codex_journal_path(&app)?;
    let journal = read_codex_journal(&journal_path)?.ok_or("CODETASのCodex復元情報がありません")?;
    let (config_path, catalog_path, backup_path, catalog_backup_path) =
        validate_codex_journal_paths(&app, &journal)?;
    let current = fs::read_to_string(&config_path)
        .map_err(|error| format!("現在のCodex設定を読めません: {error}"))?;
    let mut document = current
        .parse::<DocumentMut>()
        .map_err(|error| format!("現在のCodex設定を解析できません: {error}"))?;
    let backup = match backup_path.as_deref() {
        Some(path) => fs::read_to_string(path)
            .map_err(|error| format!("Codex設定バックアップを読めません: {error}"))?
            .parse::<DocumentMut>()
            .map_err(|error| format!("Codex設定バックアップを解析できません: {error}"))?,
        None => DocumentMut::new(),
    };
    let mut conflicts = Vec::new();

    let installed_provider = match journal.routing_mode {
        CodexRoutingMode::ProviderTable => GATEWAY_PROVIDER_ID,
        CodexRoutingMode::OpenAiBaseUrl => "openai",
    };
    restore_owned_string(
        &mut document,
        &backup,
        "model_provider",
        installed_provider,
        &mut conflicts,
    );
    restore_owned_string(
        &mut document,
        &backup,
        "model",
        &journal.installed_model,
        &mut conflicts,
    );
    restore_owned_string(
        &mut document,
        &backup,
        "model_catalog_json",
        &journal.catalog_path,
        &mut conflicts,
    );
    match journal.routing_mode {
        CodexRoutingMode::ProviderTable => {
            restore_owned_provider(&mut document, &backup, &journal, &mut conflicts)?;
        }
        CodexRoutingMode::OpenAiBaseUrl => restore_owned_string(
            &mut document,
            &backup,
            "openai_base_url",
            &journal.installed_base_url,
            &mut conflicts,
        ),
    }
    if let Some(installed_agents) = journal.installed_agents.as_ref() {
        restore_owned_agent_settings(&mut document, &backup, installed_agents, &mut conflicts)?;
    }
    let current_catalog = read_optional_file(&catalog_path)?;
    let original_catalog = match (journal.catalog_existed, catalog_backup_path.as_deref()) {
        (Some(true), Some(path)) => Some(
            read_optional_file(path)?.ok_or("Codexモデルカタログのバックアップが見つかりません")?,
        ),
        (Some(false), None) | (None, _) => None,
        _ => return Err("Codexモデルカタログの復元情報が矛盾しています".into()),
    };
    let mut settings = load_settings(&app)?;
    let expected_catalog = serde_json::to_value(build_codex_catalog(&settings))
        .map_err(|error| format!("CODETASモデルカタログを検証できません: {error}"))?;
    let current_catalog_json = current_catalog
        .as_deref()
        .and_then(|bytes| serde_json::from_slice::<JsonValue>(bytes).ok());
    let catalog_owned = current_catalog_json
        .as_ref()
        .is_some_and(|actual| actual == &expected_catalog);
    let catalog_already_restored = match journal.catalog_existed {
        Some(true) => current_catalog.as_deref() == original_catalog.as_deref(),
        Some(false) => current_catalog.is_none(),
        None => false,
    };
    if !catalog_owned && !catalog_already_restored {
        conflicts
            .push("model_catalog_json: CODETASの生成後にカタログ内容が変更されています".into());
    }
    let mut removed_catalog = false;
    if conflicts.is_empty() {
        let current_config = fs::read(&config_path)
            .map_err(|error| format!("現在のCodex設定を退避できません: {error}"))?;
        let preserve_legacy_catalog = journal.catalog_existed.is_none();
        // Disconnecting is persistent: stop CODETAS from re-applying the Codex
        // connection on the next startup or gateway start.
        let previous_settings = settings.clone();
        settings.codex.auto_connect = false;
        let restore_result = (|| {
            atomic_write(&config_path, document.to_string().as_bytes())?;
            if !preserve_legacy_catalog {
                restore_optional_file(&catalog_path, original_catalog.as_deref())?;
            }
            fs::remove_file(&journal_path)
                .map_err(|error| format!("CODETAS復元情報を削除できません: {error}"))?;
            write_settings_only(&app, &settings)
        })();
        if let Err(error) = restore_result {
            let mut rollback_errors = Vec::new();
            if let Err(rollback) = atomic_write(&config_path, &current_config) {
                rollback_errors.push(format!("Codex設定: {rollback}"));
            }
            if let Err(rollback) = restore_optional_file(&catalog_path, current_catalog.as_deref())
            {
                rollback_errors.push(format!("モデルカタログ: {rollback}"));
            }
            if let Err(rollback) = write_settings_only(&app, &previous_settings) {
                rollback_errors.push(format!("CODETAS設定: {rollback}"));
            }
            return Err(if rollback_errors.is_empty() {
                format!("Codex接続を復元できないため変更を取り消しました: {error}")
            } else {
                format!(
                    "Codex接続を復元できず、変更の取り消しにも失敗しました: {error}; {}",
                    rollback_errors.join("; ")
                )
            });
        }
        removed_catalog = journal.catalog_existed == Some(false) && current_catalog.is_some();
        for owned_backup in [backup_path.as_deref(), catalog_backup_path.as_deref()]
            .into_iter()
            .flatten()
        {
            let _ = fs::remove_file(owned_backup);
        }
    }
    Ok(CodexRestoreReport {
        restored: conflicts.is_empty(),
        config_path: config_path.to_string_lossy().into_owned(),
        conflicts,
        removed_catalog,
    })
}

#[tauri::command]
pub async fn uninstall_codetas_integration(
    app: AppHandle,
    manager: State<'_, GatewayManager>,
) -> Result<CodetasUninstallReport, String> {
    let journal_path = codex_journal_path(&app)?;
    let restore = if read_codex_journal(&journal_path)?.is_some() {
        Some(restore_codex_gateway_config(app.clone())?)
    } else {
        None
    };
    if let Some(report) = restore.as_ref().filter(|report| !report.restored) {
        return Ok(CodetasUninstallReport {
            restored_codex: false,
            removed_settings: false,
            removed_catalog: report.removed_catalog,
            removed_observability: false,
            removed_service: false,
            stopped_gateway: false,
            conflicts: report.conflicts.clone(),
        });
    }

    let prior_handle = manager.handle.lock().await.take();
    let embedded_stopped = prior_handle.is_some();
    if let Some(handle) = prior_handle {
        handle.shutdown().await;
    }
    let service_status = service::status()?;
    let service_report = if service_status.installed {
        Some(service::uninstall()?)
    } else {
        None
    };
    let removed_service = service_report
        .as_ref()
        .is_some_and(|report| report.removed_definition);
    let stopped_gateway =
        embedded_stopped || service_report.as_ref().is_some_and(|report| report.stopped);
    let settings = settings_path(&app)?;
    let removed_settings = remove_owned_file(&settings)?;
    let _ = remove_owned_file(&settings.with_file_name("auth.json"));
    let _ = remove_owned_file(&settings.with_file_name("auth.json.lock"));
    // Without the ownership journal, a matching file name is not sufficient proof that
    // CODETAS created the catalog. Leave it untouched rather than deleting user state.
    let removed_catalog = restore
        .as_ref()
        .is_some_and(|report| report.removed_catalog);
    let data_directory = app
        .path()
        .app_data_dir()
        .map_err(|error| format!("CODETASデータフォルダを特定できません: {error}"))?;
    let _ = clients::remove_all(&data_directory)?;
    let removed_observability = remove_owned_directory(&observability_path(&app)?)?;
    Ok(CodetasUninstallReport {
        restored_codex: restore.as_ref().map_or(true, |report| report.restored),
        removed_settings,
        removed_catalog,
        removed_observability,
        removed_service,
        stopped_gateway,
        conflicts: Vec::new(),
    })
}

async fn update_running_gateway(
    manager: &State<'_, GatewayManager>,
    settings: &GatewaySettings,
) -> Result<(), String> {
    let guard = manager.handle.lock().await;
    if settings.runtime.standalone_service {
        drop(guard);
        if !service::restart()? {
            return Err("常駐サービスを再起動できませんでした".into());
        }
        return Ok(());
    }
    if let Some(handle) = guard.as_ref() {
        handle.set_settings(settings.clone()).await?;
    }
    Ok(())
}

async fn persist_and_apply_settings(
    app: &AppHandle,
    manager: &State<'_, GatewayManager>,
    previous: &GatewaySettings,
    next: &GatewaySettings,
) -> Result<(), String> {
    let catalog_snapshot = snapshot_codex_catalog_transition(app, previous, next)?;
    save_settings(app, next)?;
    if let Err(error) = update_running_gateway(manager, next).await {
        let mut restore_errors =
            restore_settings_and_catalog(app, previous, catalog_snapshot.as_ref());
        if let Err(runtime) = update_running_gateway(manager, previous).await {
            restore_errors.push(format!("旧runtime設定: {runtime}"));
        }
        let restore = if restore_errors.is_empty() {
            "旧設定へ復元しました".to_string()
        } else {
            format!("旧状態の復元に失敗しました: {}", restore_errors.join("; "))
        };
        return Err(format!(
            "設定をruntimeへ反映できませんでした: {error}; {restore}"
        ));
    }
    Ok(())
}

async fn runtime_is_running(
    manager: &State<'_, GatewayManager>,
    settings: &GatewaySettings,
) -> Result<bool, String> {
    if manager.handle.lock().await.is_some() {
        return Ok(true);
    }
    if settings.runtime.standalone_service {
        return Ok(service::status()?.running);
    }
    Ok(false)
}

fn status(
    app: &AppHandle,
    running: bool,
    settings: GatewaySettings,
) -> Result<GatewayStatus, String> {
    let gateway_url = if running {
        runtime_gateway_url(app).unwrap_or_else(|| gateway_url(&settings))
    } else {
        gateway_url(&settings)
    };
    let codex_configured = codex_gateway_is_configured(app, &settings).unwrap_or(false);
    Ok(GatewayStatus {
        running,
        url: gateway_url,
        providers: settings.providers,
        default_provider: settings.default_provider,
        codex_configured,
        settings_path: Some(settings_path(app)?.to_string_lossy().into_owned()),
    })
}

async fn start_managed_gateway(
    app: &AppHandle,
    settings: GatewaySettings,
) -> Result<GatewayHandle, String> {
    let configured_url = gateway_url(&settings);
    let handle = start_gateway_with_options(
        settings.runtime.port,
        settings,
        GatewayRuntimeOptions {
            observability_directory: Some(observability_path(app)?),
            runtime_state_path: Some(settings_path(app)?.with_file_name("gateway-runtime.json")),
            auth_store_path: Some(auth_store_file_path(app)?),
        },
    )
    .await
    .map_err(|error| error.to_string())?;
    let actual_url = handle.url();
    if actual_url != configured_url {
        reconcile_owned_codex_runtime_url(app, &actual_url)?;
    }
    Ok(handle)
}

fn load_settings(app: &AppHandle) -> Result<GatewaySettings, String> {
    configure_desktop_auth_store(app)?;
    let path = settings_path(app)?;
    let content = match fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut settings = GatewaySettings::default();
            if cfg!(feature = "validation-build") {
                settings.runtime.port = 43_431;
            }
            if adopt_local_cli_sessions(&mut settings).unwrap_or(false) {
                let _ = save_settings(app, &settings);
            }
            return Ok(settings);
        }
        Err(error) => return Err(format!("プロバイダ設定を読めません: {error}")),
    };
    let (mut settings, migrated) = parse_gateway_settings_json(content.as_bytes())
        .map_err(|error| format!("プロバイダ設定を移行できません: {error}"))?;
    let adopted = adopt_local_cli_sessions(&mut settings).unwrap_or(false);
    if migrated || adopted {
        save_settings(app, &settings)?;
    }
    Ok(settings)
}

fn save_settings(app: &AppHandle, settings: &GatewaySettings) -> Result<(), String> {
    let path = settings_path(app)?;
    let content = serde_json::to_vec_pretty(settings)
        .map_err(|error| format!("プロバイダ設定を生成できません: {error}"))?;
    let catalog_snapshot = sync_owned_codex_catalog(app, settings)?;
    if let Err(error) = atomic_write(&path, &content) {
        if let Some((catalog_path, previous_catalog)) = catalog_snapshot {
            if let Err(restore) = restore_optional_file(&catalog_path, previous_catalog.as_deref())
            {
                return Err(format!(
                    "プロバイダ設定を保存できず、Codexカタログの復元にも失敗しました: {error}; {restore}"
                ));
            }
        }
        return Err(error);
    }
    Ok(())
}

fn sync_owned_codex_catalog(
    app: &AppHandle,
    settings: &GatewaySettings,
) -> Result<Option<(PathBuf, Option<Vec<u8>>)>, String> {
    let snapshot = snapshot_owned_codex_catalog(app, settings)?;
    let Some((catalog_path, previous)) = snapshot else {
        return Ok(None);
    };
    let content = serialize_codex_catalog(settings, None)?;
    atomic_write(&catalog_path, &content)?;
    Ok(Some((catalog_path, previous)))
}

fn snapshot_owned_codex_catalog(
    app: &AppHandle,
    settings: &GatewaySettings,
) -> Result<Option<(PathBuf, Option<Vec<u8>>)>, String> {
    if !settings.codex.auto_sync_catalog {
        return Ok(None);
    }
    let journal_path = codex_journal_path(app)?;
    let Some(journal) = read_codex_journal(&journal_path)? else {
        return Ok(None);
    };
    let (_, catalog_path, _, _) = validate_codex_journal_paths(app, &journal)?;
    let previous = read_optional_file(&catalog_path)?;
    Ok(Some((catalog_path, previous)))
}

fn snapshot_codex_catalog_transition(
    app: &AppHandle,
    previous: &GatewaySettings,
    next: &GatewaySettings,
) -> Result<Option<(PathBuf, Option<Vec<u8>>)>, String> {
    if !previous.codex.auto_sync_catalog && !next.codex.auto_sync_catalog {
        return Ok(None);
    }
    let mut ownership_settings = next.clone();
    ownership_settings.codex.auto_sync_catalog = true;
    snapshot_owned_codex_catalog(app, &ownership_settings)
}

fn restore_file_snapshot(snapshot: Option<&(PathBuf, Option<Vec<u8>>)>) -> Result<(), String> {
    match snapshot {
        Some((path, content)) => restore_optional_file(path, content.as_deref()),
        None => Ok(()),
    }
}

fn restore_settings_and_catalog(
    app: &AppHandle,
    previous: &GatewaySettings,
    catalog_snapshot: Option<&(PathBuf, Option<Vec<u8>>)>,
) -> Vec<String> {
    let mut errors = Vec::new();
    if let Err(error) = write_settings_only(app, previous) {
        errors.push(format!("旧設定ファイル: {error}"));
    }
    if let Err(error) = restore_file_snapshot(catalog_snapshot) {
        errors.push(format!("旧Codexカタログ: {error}"));
    }
    errors
}

fn write_settings_only(app: &AppHandle, settings: &GatewaySettings) -> Result<(), String> {
    let content = serde_json::to_vec_pretty(settings)
        .map_err(|error| format!("プロバイダ設定を生成できません: {error}"))?;
    atomic_write(&settings_path(app)?, &content)
}

fn settings_path(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_config_dir()
        .map(|directory| directory.join("providers.json"))
        .map_err(|error| format!("CODETAS設定フォルダを特定できません: {error}"))
}

fn observability_path(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_data_dir()
        .map(|directory| directory.join("observability"))
        .map_err(|error| format!("CODETAS観測フォルダを特定できません: {error}"))
}

fn gateway_runtime_state_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(settings_path(app)?.with_file_name("gateway-runtime.json"))
}

fn runtime_gateway_url(app: &AppHandle) -> Option<String> {
    let path = gateway_runtime_state_path(app).ok()?;
    let metadata = fs::symlink_metadata(&path).ok()?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > 64 * 1024 {
        return None;
    }
    let value = serde_json::from_slice::<JsonValue>(&fs::read(path).ok()?).ok()?;
    if value.get("version").and_then(JsonValue::as_u64) != Some(1) {
        return None;
    }
    let url = value.get("url").and_then(JsonValue::as_str)?;
    let safe_loopback = url.starts_with("http://127.0.0.1:")
        || url.starts_with("http://[::1]:")
        || url.starts_with("http://localhost:");
    (safe_loopback && url.ends_with("/v1") && url.len() <= 512).then(|| url.to_string())
}

fn remove_owned_directory(path: &Path) -> Result<bool, String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(format!("CODETAS所有フォルダを確認できません: {error}")),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err("CODETAS所有フォルダは通常ディレクトリではないため削除しません".into());
    }
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("CODETAS所有フォルダを削除できません: {error}")),
    }
}

fn codex_config_path() -> Result<PathBuf, String> {
    Ok(codex_home()?.join("config.toml"))
}

fn codex_journal_path(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_config_dir()
        .map(|directory| directory.join("codex-install-journal.json"))
        .map_err(|error| format!("CODETAS設定フォルダを特定できません: {error}"))
}

fn read_codex_journal(path: &Path) -> Result<Option<CodexInstallJournal>, String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("Codex復元情報を確認できません: {error}")),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("Codex復元情報は通常ファイルではないため使用しません".into());
    }
    if metadata.len() > 1024 * 1024 {
        return Err("Codex復元情報が大きすぎます".into());
    }
    let bytes = fs::read(path).map_err(|error| format!("Codex復元情報を読めません: {error}"))?;
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|error| format!("Codex復元情報を解析できません: {error}"))
}

fn validate_codex_journal_paths(
    app: &AppHandle,
    journal: &CodexInstallJournal,
) -> Result<(PathBuf, PathBuf, Option<PathBuf>, Option<PathBuf>), String> {
    let journal_path = codex_journal_path(app)?;
    let validated = validate_codex_journal_paths_from_journal_path(&journal_path, journal)?;
    if validated.0 != codex_config_path()?
        || validated.1 != codex_home()?.join("codetas-model-catalog.json")
    {
        return Err("Codex復元情報が現在のCODEX_HOME外を参照しています".into());
    }
    Ok(validated)
}

fn validate_codex_journal_paths_from_journal_path(
    journal_path: &Path,
    journal: &CodexInstallJournal,
) -> Result<(PathBuf, PathBuf, Option<PathBuf>, Option<PathBuf>), String> {
    if journal.version != CODEX_JOURNAL_VERSION {
        return Err("unsupported Codex restore journal version".into());
    }
    let config_path = PathBuf::from(&journal.config_path);
    let catalog_path = PathBuf::from(&journal.catalog_path);
    let config_home = config_path
        .parent()
        .ok_or("Codex復元情報の設定パスに親フォルダがありません")?;
    if config_path.file_name().and_then(|name| name.to_str()) != Some("config.toml")
        || catalog_path != config_home.join("codetas-model-catalog.json")
    {
        return Err("Codex復元情報の設定・カタログパスの組み合わせが不正です".into());
    }
    let backup_path = journal.backup_path.as_deref().map(PathBuf::from);
    if let Some(backup) = backup_path.as_deref() {
        let expected_parent = journal_path
            .parent()
            .ok_or("CODETAS復元フォルダを特定できません")?;
        let valid_name = backup
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| {
                name.starts_with("codex-config-before-") && name.ends_with(".toml")
            });
        if backup.parent() != Some(expected_parent) || !valid_name {
            return Err("Codex復元情報のバックアップパスが所有境界外です".into());
        }
    }
    let catalog_backup_path = journal.catalog_backup_path.as_deref().map(PathBuf::from);
    if let Some(backup) = catalog_backup_path.as_deref() {
        let expected_parent = journal_path
            .parent()
            .ok_or("CODETAS復元フォルダを特定できません")?;
        let valid_name = backup
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| {
                name.starts_with("codex-catalog-before-") && name.ends_with(".json")
            });
        if backup.parent() != Some(expected_parent) || !valid_name {
            return Err("Codex復元情報のカタログバックアップが所有境界外です".into());
        }
    }
    match (journal.catalog_existed, catalog_backup_path.is_some()) {
        (Some(true), true) | (Some(false), false) | (None, _) => {}
        _ => return Err("Codex復元情報のカタログ存在状態が矛盾しています".into()),
    }
    Ok((config_path, catalog_path, backup_path, catalog_backup_path))
}

fn prepare_codex_journal(
    app: &AppHandle,
    config_path: &Path,
    catalog_path: &Path,
    model: &str,
    base_url: &str,
    local_token: bool,
    routing_mode: CodexRoutingMode,
    installed_agents: Option<CodexAgentInstall>,
) -> Result<Vec<PathBuf>, String> {
    let journal_path = codex_journal_path(app)?;
    let existing = match read_codex_journal(&journal_path)? {
        Some(journal) => {
            validate_codex_journal_paths(app, &journal)?;
            Some(journal)
        }
        None => None,
    };
    let mut created_backups = Vec::new();
    let backup_path = if let Some(existing) = existing.as_ref() {
        if Path::new(&existing.config_path) != config_path {
            return Err("既存のCODETAS復元情報が別のCodex設定を参照しています".into());
        }
        existing.backup_path.clone()
    } else {
        create_codex_backup(
            &journal_path,
            config_path,
            "codex-config-before",
            "toml",
            &mut created_backups,
        )?
    };
    let (catalog_backup_path, catalog_existed) = if let Some(existing) = existing.as_ref() {
        (
            existing.catalog_backup_path.clone(),
            existing.catalog_existed,
        )
    } else {
        match create_codex_backup(
            &journal_path,
            catalog_path,
            "codex-catalog-before",
            "json",
            &mut created_backups,
        ) {
            Ok(path) => {
                let existed = path.is_some();
                (path, Some(existed))
            }
            Err(error) => {
                for backup in &created_backups {
                    let _ = fs::remove_file(backup);
                }
                return Err(error);
            }
        }
    };
    let journal = CodexInstallJournal {
        version: CODEX_JOURNAL_VERSION,
        config_path: config_path.to_string_lossy().into_owned(),
        backup_path,
        catalog_path: catalog_path.to_string_lossy().into_owned(),
        catalog_backup_path,
        catalog_existed,
        installed_model: model.to_string(),
        installed_base_url: base_url.to_string(),
        routing_mode,
        installed_local_token: local_token,
        installed_agents,
    };
    let content = serde_json::to_vec_pretty(&journal)
        .map_err(|error| format!("CODETAS復元情報を生成できません: {error}"))?;
    if let Err(error) = atomic_write(&journal_path, &content) {
        for backup in &created_backups {
            let _ = fs::remove_file(backup);
        }
        return Err(error);
    }
    Ok(created_backups)
}

fn enable_codex_openai_passthrough(settings: &mut GatewaySettings) -> Result<(), String> {
    if let Some(provider) = settings
        .providers
        .iter_mut()
        .find(|provider| provider.id == "openai")
    {
        provider.enabled = true;
        return settings.validate();
    }

    let mut provider = provider_presets()
        .into_iter()
        .find(|preset| preset.id == "openai")
        .ok_or("OpenAI Codexログインのpassthrough presetがありません")?
        .instantiate(None)?;
    provider.enabled = true;
    settings.providers.push(provider);
    settings.validate()
}

fn auto_register_authenticated_cli_providers(settings: &mut GatewaySettings) -> Result<(), String> {
    // Codex Desktop authentication is forwarded by the native `openai` provider;
    // no token is extracted or copied from the Codex CLI.
    if let Some(codex) = find_cli_executable("codex") {
        let _ = command_succeeds(&codex, &["login", "status"], Duration::from_secs(5));
    }

    // GitHub CLI has a documented token-output command. Store only the absolute
    // executable path and invoke it on demand; the token never enters settings.
    if settings
        .providers
        .iter()
        .all(|provider| provider.id != "github-models")
    {
        if let Some(gh) = find_cli_executable("gh").filter(|path| {
            command_succeeds(
                path,
                &["auth", "status", "--hostname", "github.com"],
                Duration::from_secs(5),
            )
        }) {
            register_github_cli_provider(settings, &gh)?;
        }
    }
    if auth_store_is_configured() {
        adopt_local_cli_sessions(settings)?;
    }
    settings.validate()
}

fn configure_desktop_auth_store(app: &AppHandle) -> Result<(), String> {
    configure_auth_store_path(Some(auth_store_file_path(app)?));
    Ok(())
}

fn auth_store_file_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(settings_path(app)?.with_file_name("auth.json"))
}

fn register_github_cli_provider(settings: &mut GatewaySettings, gh: &Path) -> Result<(), String> {
    if !gh.is_absolute() {
        return Err("GitHub CLI command must use an absolute path".into());
    }
    if settings
        .providers
        .iter()
        .any(|provider| provider.id == "github-models")
    {
        return Ok(());
    }
    let mut provider = provider_presets()
        .into_iter()
        .find(|preset| preset.id == "github-models")
        .ok_or("GitHub Models presetがありません")?
        .instantiate(None)?;
    provider.credential = ProviderCredential {
        source: CredentialSource::Command,
        reference: None,
        transport: CredentialTransport::Bearer,
        header_name: None,
        command: Some(CredentialCommand {
            program: gh.to_string_lossy().into_owned(),
            args: vec![
                "auth".into(),
                "token".into(),
                "--hostname".into(),
                "github.com".into(),
            ],
            cwd: None,
            timeout_ms: 5_000,
            refresh_interval_ms: 300_000,
        }),
    };
    provider.enabled = true;
    settings.providers.push(provider);
    settings.validate()
}

fn find_cli_executable(name: &str) -> Option<PathBuf> {
    if name.is_empty() || name.contains('/') || name.contains('\\') {
        return None;
    }
    let mut candidates = Vec::new();
    if let Some(path) = env::var_os("PATH") {
        candidates.extend(env::split_paths(&path).map(|directory| directory.join(name)));
    }
    if let Some(home) = dirs::home_dir() {
        candidates.extend([
            home.join(".local/bin").join(name),
            home.join(".cargo/bin").join(name),
            home.join(".npm-global/bin").join(name),
            // Vendor CLIs install here; macOS GUI PATH does not include them.
            home.join(".kimi-code/bin").join(name),
            home.join(".opencode/bin").join(name),
            home.join(".claude/bin").join(name),
            home.join(".kiro/bin").join(name),
        ]);
    }
    candidates.extend([
        PathBuf::from("/opt/homebrew/bin").join(name),
        PathBuf::from("/usr/local/bin").join(name),
        PathBuf::from("/usr/bin").join(name),
    ]);
    candidates.into_iter().find_map(|candidate| {
        let metadata = fs::metadata(&candidate).ok()?;
        metadata
            .is_file()
            .then(|| fs::canonicalize(&candidate).unwrap_or(candidate))
    })
}

fn scan_local_cli_candidates(deep: bool, registered: &BTreeSet<String>) -> LocalCliScanReport {
    let clients = LOCAL_CLI_CANDIDATES
        .iter()
        .map(|candidate| scan_local_cli_candidate(*candidate, deep, registered))
        .collect();
    LocalCliScanReport {
        checked_at_ms: now_ms(),
        deep,
        clients,
    }
}

fn scan_local_cli_candidate(
    candidate: LocalCliCandidate,
    deep: bool,
    registered: &BTreeSet<String>,
) -> LocalCliStatus {
    let already_registered = candidate
        .codetas_provider_id
        .is_some_and(|provider_id| registered.contains(provider_id));
    let Some(executable) = find_cli_executable(candidate.executable) else {
        return LocalCliStatus {
            id: candidate.id.into(),
            name: candidate.name.into(),
            installed: false,
            executable: None,
            version: None,
            probe_state: "notInstalled".into(),
            message: "CLIが見つかりません".into(),
            can_register: false,
            needs_codetas_registration: false,
            codetas_provider_id: candidate.codetas_provider_id.map(str::to_string),
            registration_hint: local_cli_registration_hint(candidate.id).into(),
        };
    };
    let version = command_output(&executable, &["--version"], Duration::from_secs(3))
        .filter(|output| output.success)
        .and_then(|output| {
            output
                .text
                .lines()
                .next()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_string)
        });
    let (probe_state, message) = if deep {
        deep_probe_local_cli(candidate.id, &executable)
    } else {
        (
            "installed".into(),
            "インストール済み。登録確認で認証・quotaを検証できます".into(),
        )
    };
    let ready = probe_state == "ready";
    LocalCliStatus {
        id: candidate.id.into(),
        name: candidate.name.into(),
        installed: true,
        executable: Some(executable.to_string_lossy().into_owned()),
        version,
        probe_state,
        message,
        can_register: ready && candidate.supports_provider,
        needs_codetas_registration: candidate.codetas_provider_id.is_some() && !already_registered,
        codetas_provider_id: candidate.codetas_provider_id.map(str::to_string),
        registration_hint: local_cli_registration_hint(candidate.id).into(),
    }
}

fn provider_registration_hint(provider_id: &str) -> &'static str {
    match provider_id {
        "anthropic" | "anthropic-apikey" => {
            "Claude の既存ログインがあれば取り込みます。なければブラウザでログインします。"
        }
        "deepseek" => "DEEPSEEK_API_KEY を Keychain または環境変数で保存します。",
        "zai" => "ZAI_API_KEY を Keychain または環境変数で保存します。",
        "minimax" | "minimax-cn" => {
            "MINIMAX_API_KEY を Keychain または環境変数で保存します。"
        }
        "xai" => "Grok CLI のログインがあれば取り込みます。なければブラウザでログインします。",
        "kimi" => "Kimi CLI のログインがあれば取り込みます。なければブラウザでログインします。",
        _ => "APIキー、既存CLIログイン、またはアプリ内OAuthで接続します。",
    }
}

fn local_cli_registration_hint(id: &str) -> &'static str {
    match id {
        "codex" => "Codexログインはそのまま転送します。別途APIキーは不要です。",
        "claude" => "Claude CLIのログインを取り込みます。未ログインならCODETASでブラウザログインします。",
        "grok" => "Grok CLIのログインを取り込みます。未ログインならCODETASでブラウザログインします。",
        "agy" => "gcloud OAuth、または GOOGLE_ANTIGRAVITY_ACCESS_TOKEN を Keychain 参照で保存します。",
        "qwen" => "Alibaba / DashScope のAPIキーを DASHSCOPE_API_KEY または Keychain 参照で保存します。",
        "kimi" => "Kimi CLIのログインを取り込みます。未ログインならCODETASでブラウザログインします。",
        "opencode" => "OpenCode Go のAPIキーを OPENCODE_API_KEY または Keychain 参照で保存します。",
        "gemini" => "Google AI Studio のAPIキーを GEMINI_API_KEY または Keychain 参照で保存します。",
        "kiro" => "Kiro のアクセストークンを KIRO_ACCESS_TOKEN または Keychain 参照で保存します。",
        _ => "APIキーまたは既存のCLIログインで接続します。",
    }
}

fn deep_probe_local_cli(id: &str, executable: &Path) -> (String, String) {
    let args: &[&str] = match id {
        "codex" => &["login", "status"],
        "claude" => &["auth", "status", "--text"],
        "agy" => &["models"],
        "kimi" => &["provider", "list"],
        "opencode" => &["providers", "list"],
        "grok" | "qwen" | "gemini" | "kiro" => {
            return (
                "installed".into(),
                format!(
                    "{}を検出しました。CODETASに取り込むか、ログインしてください",
                    match id {
                        "grok" => "Grok CLI",
                        "qwen" => "Qwen Code",
                        "gemini" => "Gemini CLI",
                        "kiro" => "Kiro CLI",
                        _ => "CLI",
                    }
                ),
            );
        }
        _ => return ("unsupported".into(), "安全な非対話probeが未定義です".into()),
    };
    let timeout = Duration::from_secs(15);
    let Some(output) = command_output(executable, args, timeout) else {
        return (
            "unavailable".into(),
            "非対話probeに失敗またはタイムアウトしました".into(),
        );
    };
    classify_local_cli_probe(id, output.success, &output.text)
}

fn classify_local_cli_probe(id: &str, success: bool, output: &str) -> (String, String) {
    let normalized = output.to_ascii_lowercase();
    if normalized.contains("not logged in")
        || normalized.contains("unauthorized")
        || (!success && normalized.contains("login"))
    {
        return (
            "authenticationRequired".into(),
            "headless実行の認証が必要です".into(),
        );
    }
    let codex_logged_in = id == "codex"
        && success
        && output.lines().any(|line| {
            let line = line.trim().to_ascii_lowercase();
            line.starts_with("logged in") && !line.starts_with("not logged in")
        });
    if success && (normalized.contains("codetas_cli_ok") || codex_logged_in) {
        return ("ready".into(), "非対話probeに成功しました".into());
    }
    if success && id == "agy" && output.lines().any(|line| line.contains('\t')) {
        return (
            "authenticated".into(),
            "Antigravity CLIはログイン済みです。CODETASが取り込みます".into(),
        );
    }
    if success && id == "kimi" && kimi_provider_list_is_authenticated(output) {
        return (
            "authenticated".into(),
            "Kimi CLIはログイン済みです。CODETASが取り込みます".into(),
        );
    }
    if success && id == "claude" && claude_auth_status_is_authenticated(output) {
        return (
            "authenticated".into(),
            "Claude CLIはログイン済みです。CODETASが取り込みます".into(),
        );
    }
    if success && id == "opencode" && output.lines().any(|line| !line.trim().is_empty()) {
        return (
            "authenticated".into(),
            "OpenCode CLIはログイン済みです。CODETASが取り込みます".into(),
        );
    }
    if normalized.contains("quota") || normalized.contains("insufficient_quota") {
        return (
            "quotaExhausted".into(),
            "認証済みですがquotaが枯渇しています".into(),
        );
    }

    (
        "unavailable".into(),
        "非対話probeは応答しましたが利用可能性を確認できませんでした".into(),
    )
}

fn claude_auth_status_is_authenticated(output: &str) -> bool {
    let normalized = output.to_ascii_lowercase();
    !normalized.contains("not logged in")
        && (normalized.contains("logged in")
            || normalized.contains("oauth")
            || normalized.contains("api key")
            || normalized.contains("anthropic"))
}

fn kimi_provider_list_is_authenticated(output: &str) -> bool {
    output.lines().any(|line| {
        let line = line.to_ascii_lowercase();
        (line.contains("source=oauth") || line.contains("source=api_key") || line.contains("type=kimi"))
            && line.contains("models=")
            && !line.contains("models=0")
    })
}

struct BoundedCommandOutput {
    success: bool,
    text: String,
}

fn command_output(
    program: &Path,
    args: &[&str],
    timeout: Duration,
) -> Option<BoundedCommandOutput> {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // command-group maps to a POSIX process group on Unix and a Job Object on
    // Windows, so timeout cleanup includes descendants started by the CLI.
    let mut child = command.group_spawn().ok()?;
    let stdout = child.inner().stdout.take()?;
    let stderr = child.inner().stderr.take()?;
    // Drain both pipes while the process is running. read_bounded keeps only a
    // bounded prefix but continues to EOF, preventing pipe-capacity deadlocks.
    let (stdout_tx, stdout_rx) = mpsc::channel();
    let (stderr_tx, stderr_rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = stdout_tx.send(read_bounded(stdout, 32 * 1024));
    });
    thread::spawn(move || {
        let _ = stderr_tx.send(read_bounded(stderr, 32 * 1024));
    });
    let started = Instant::now();
    let mut status = None;
    let mut stdout_bytes = None;
    let mut stderr_bytes = None;
    loop {
        if status.is_none() {
            match child.try_wait() {
                Ok(Some(current)) => {
                    status = Some(current);
                    // The group leader may exit before helpers that inherited
                    // its pipes. Probe commands never own long-lived helpers,
                    // so stop the remaining group/job immediately while
                    // preserving the leader's exit status.
                    let _ = child.kill();
                    let _ = child.wait();
                }
                Ok(None) => {}
                Err(_) => break,
            }
        }
        if stdout_bytes.is_none() {
            if let Ok(output) = stdout_rx.try_recv() {
                stdout_bytes = output;
            }
        }
        if stderr_bytes.is_none() {
            if let Ok(output) = stderr_rx.try_recv() {
                stderr_bytes = output;
            }
        }
        if status.is_some() && stdout_bytes.is_some() && stderr_bytes.is_some() {
            break;
        }
        if started.elapsed() >= timeout {
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }
    let completed = status.is_some() && stdout_bytes.is_some() && stderr_bytes.is_some();
    if !completed {
        let _ = child.kill();
        let _ = child.wait();
        // Reader threads are intentionally detached here. Group cleanup closes
        // inherited pipe writers; the readers then finish independently, while
        // this timeout path remains bounded and never waits on an unbounded join.
        return None;
    }
    let success = status?.success();
    let mut bytes = stdout_bytes?;
    let stderr = stderr_bytes?;
    const MAX_OUTPUT_BYTES: usize = 64 * 1024;
    bytes.extend_from_slice(
        &stderr[..stderr
            .len()
            .min(MAX_OUTPUT_BYTES.saturating_sub(bytes.len()))],
    );
    Some(BoundedCommandOutput {
        success,
        text: String::from_utf8_lossy(&bytes).into_owned(),
    })
}

fn read_bounded(mut reader: impl Read, limit: usize) -> Option<Vec<u8>> {
    let mut kept = Vec::with_capacity(limit);
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        let read = reader.read(&mut buffer).ok()?;
        if read == 0 {
            return Some(kept);
        }
        let remaining = limit.saturating_sub(kept.len());
        kept.extend_from_slice(&buffer[..read.min(remaining)]);
    }
}

fn command_succeeds(program: &Path, args: &[&str], timeout: Duration) -> bool {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = match command.group_spawn() {
        Ok(child) => child,
        Err(_) => return false,
    };
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                // Probe commands never own long-lived descendants. Clean up the
                // process group / Job Object even when the leader exited first.
                let _ = child.kill();
                let _ = child.wait();
                return status.success();
            }
            Ok(None) if started.elapsed() < timeout => thread::sleep(Duration::from_millis(25)),
            Ok(None) | Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

fn create_codex_backup(
    journal_path: &Path,
    source: &Path,
    prefix: &str,
    extension: &str,
    created: &mut Vec<PathBuf>,
) -> Result<Option<String>, String> {
    let Some(original) = read_optional_file(source)? else {
        return Ok(None);
    };
    let directory = journal_path
        .parent()
        .ok_or("CODETAS復元フォルダを特定できません")?;
    fs::create_dir_all(directory)
        .map_err(|error| format!("CODETAS復元フォルダを作れません: {error}"))?;
    let unique = format!(
        "{}-{}-{}",
        now_ms(),
        std::process::id(),
        ATOMIC_WRITE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    );
    let backup = directory.join(format!("{prefix}-{unique}.{extension}"));
    atomic_write(&backup, &original)
        .map_err(|error| format!("Codexバックアップを作れません: {error}"))?;
    created.push(backup.clone());
    Ok(Some(backup.to_string_lossy().into_owned()))
}

fn restore_agents_before_disable(
    app: &AppHandle,
    document: &mut DocumentMut,
) -> Result<(), String> {
    let journal_path = codex_journal_path(app)?;
    let Some(journal) = read_codex_journal(&journal_path)? else {
        return Ok(());
    };
    let (_, _, backup_path, _) = validate_codex_journal_paths(app, &journal)?;
    let Some(installed_agents) = journal.installed_agents.as_ref() else {
        return Ok(());
    };
    let backup = match backup_path.as_deref() {
        Some(path) => fs::read_to_string(path)
            .map_err(|error| format!("Codex設定バックアップを読めません: {error}"))?
            .parse::<DocumentMut>()
            .map_err(|error| format!("Codex設定バックアップを解析できません: {error}"))?,
        None => DocumentMut::new(),
    };
    let mut conflicts = Vec::new();
    restore_owned_agent_settings(document, &backup, installed_agents, &mut conflicts)?;
    if conflicts.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "Codexのagents設定がCODETAS接続後に変更されたため無効化を停止しました: {}",
            conflicts.join(" / ")
        ))
    }
}

fn restore_owned_agent_settings(
    document: &mut DocumentMut,
    backup: &DocumentMut,
    installed: &CodexAgentInstall,
    conflicts: &mut Vec<String>,
) -> Result<(), String> {
    restore_owned_agent_bool(document, backup, "enabled", installed.enabled, conflicts)?;
    restore_owned_agent_integer(
        document,
        backup,
        "max_concurrent_threads_per_session",
        installed.max_concurrent_threads_per_session,
        conflicts,
    )?;
    let should_remove = document
        .get("agents")
        .and_then(Item::as_table)
        .is_some_and(Table::is_empty)
        && backup.get("agents").is_none();
    if should_remove {
        document.remove("agents");
    }
    Ok(())
}

fn restore_owned_agent_bool(
    document: &mut DocumentMut,
    backup: &DocumentMut,
    key: &str,
    installed: bool,
    conflicts: &mut Vec<String>,
) -> Result<(), String> {
    let current = document
        .get("agents")
        .and_then(Item::as_table)
        .and_then(|agents| agents.get(key))
        .and_then(Item::as_bool);
    let previous = backup
        .get("agents")
        .and_then(Item::as_table)
        .and_then(|agents| agents.get(key))
        .and_then(Item::as_bool);
    if current != Some(installed) {
        if current == previous {
            return Ok(());
        }
        conflicts.push(format!(
            "agents.{key} was changed or removed after installation"
        ));
        return Ok(());
    }
    restore_agent_item(document, backup, key)
}

fn restore_owned_agent_integer(
    document: &mut DocumentMut,
    backup: &DocumentMut,
    key: &str,
    installed: i64,
    conflicts: &mut Vec<String>,
) -> Result<(), String> {
    let current = document
        .get("agents")
        .and_then(Item::as_table)
        .and_then(|agents| agents.get(key))
        .and_then(Item::as_integer);
    let previous = backup
        .get("agents")
        .and_then(Item::as_table)
        .and_then(|agents| agents.get(key))
        .and_then(Item::as_integer);
    if current != Some(installed) {
        if current == previous {
            return Ok(());
        }
        conflicts.push(format!(
            "agents.{key} was changed or removed after installation"
        ));
        return Ok(());
    }
    restore_agent_item(document, backup, key)
}

fn restore_agent_item(
    document: &mut DocumentMut,
    backup: &DocumentMut,
    key: &str,
) -> Result<(), String> {
    let previous = backup
        .get("agents")
        .and_then(Item::as_table)
        .and_then(|agents| agents.get(key))
        .cloned();
    let agents = document
        .get_mut("agents")
        .and_then(Item::as_table_mut)
        .ok_or("agents must be a TOML table")?;
    if let Some(previous) = previous {
        agents[key] = previous;
    } else {
        agents.remove(key);
    }
    Ok(())
}

fn restore_owned_string(
    document: &mut DocumentMut,
    backup: &DocumentMut,
    key: &str,
    installed_value: &str,
    conflicts: &mut Vec<String>,
) {
    let current = document.get(key).and_then(Item::as_str);
    let previous = backup.get(key).and_then(Item::as_str);
    if current != Some(installed_value) {
        if current != previous {
            conflicts.push(format!("{key} was changed after CODETAS installation"));
        }
        return;
    }
    if let Some(previous) = backup.get(key) {
        document[key] = previous.clone();
    } else {
        document.remove(key);
    }
}

fn restore_owned_provider(
    document: &mut DocumentMut,
    backup: &DocumentMut,
    journal: &CodexInstallJournal,
    conflicts: &mut Vec<String>,
) -> Result<(), String> {
    let current_item = document
        .get("model_providers")
        .and_then(Item::as_table)
        .and_then(|providers| providers.get(GATEWAY_PROVIDER_ID));
    let previous_item = backup
        .get("model_providers")
        .and_then(Item::as_table)
        .and_then(|providers| providers.get(GATEWAY_PROVIDER_ID));
    if current_item.map(|item| item.to_string()) == previous_item.map(|item| item.to_string()) {
        return Ok(());
    }
    let current = current_item.and_then(Item::as_table);
    let owned = current.is_some_and(|provider| {
        let expected_length = if journal.installed_local_token { 5 } else { 4 };
        let token_header_matches = if journal.installed_local_token {
            provider
                .get("env_http_headers")
                .and_then(Item::as_table)
                .and_then(|headers| headers.get("x-codetas-token"))
                .and_then(Item::as_str)
                == Some("CODETAS_GATEWAY_TOKEN")
        } else {
            provider.get("env_http_headers").is_none()
        };
        provider.len() == expected_length
            && provider.get("name").and_then(Item::as_str) == Some("CODETAS Gateway")
            && provider.get("base_url").and_then(Item::as_str)
                == Some(journal.installed_base_url.as_str())
            && provider.get("wire_api").and_then(Item::as_str) == Some("responses")
            && provider.get("supports_websockets").and_then(Item::as_bool) == Some(true)
            && token_header_matches
    });
    if current.is_none() {
        conflicts.push("model_providers.codetas_gateway was removed after installation".into());
        return Ok(());
    }
    if !owned {
        conflicts.push("model_providers.codetas_gateway was changed after installation".into());
        return Ok(());
    }
    let previous = backup
        .get("model_providers")
        .and_then(Item::as_table)
        .and_then(|providers| providers.get(GATEWAY_PROVIDER_ID))
        .cloned();
    let providers = document
        .get_mut("model_providers")
        .and_then(Item::as_table_mut)
        .ok_or("model_providers must be a TOML table")?;
    if let Some(previous) = previous {
        providers[GATEWAY_PROVIDER_ID] = previous;
    } else {
        providers.remove(GATEWAY_PROVIDER_ID);
    }
    Ok(())
}

fn is_codetas_loopback_url(value: &str) -> bool {
    let Some(authority_and_path) = value.strip_prefix("http://") else {
        return false;
    };
    if authority_and_path.contains(['@', '?', '#']) {
        return false;
    }
    let Some((authority, path)) = authority_and_path.split_once('/') else {
        return false;
    };
    let host = authority
        .split_once(':')
        .map(|(host, port)| {
            (!port.is_empty() && port.bytes().all(|byte| byte.is_ascii_digit())).then_some(host)
        })
        .unwrap_or(Some(authority));
    host.is_some_and(|host| matches!(host, "127.0.0.1" | "localhost"))
        && path.trim_end_matches('/') == "v1"
}

fn remove_legacy_codetas_provider_if_owned(
    document: &mut DocumentMut,
    gateway_base_url: &str,
) -> Result<(), String> {
    let Some(item) = document
        .get("model_providers")
        .and_then(Item::as_table)
        .and_then(|providers| providers.get(GATEWAY_PROVIDER_ID))
    else {
        return Ok(());
    };
    let Some(provider) = item.as_table() else {
        return Err("既存のmodel_providers.codetas_gatewayがtableではありません".into());
    };
    let current_keys = provider.iter().map(|(key, _)| key).collect::<BTreeSet<_>>();
    let mut expected_current_keys = ["name", "base_url", "wire_api", "supports_websockets"]
        .into_iter()
        .collect::<BTreeSet<_>>();
    if provider.contains_key("env_http_headers") {
        expected_current_keys.insert("env_http_headers");
    }
    let current_shape_owned = document.get("model_provider").and_then(Item::as_str)
        == Some(GATEWAY_PROVIDER_ID)
        && current_keys == expected_current_keys
        && provider.get("name").and_then(Item::as_str) == Some("CODETAS Gateway")
        && provider.get("base_url").and_then(Item::as_str) == Some(gateway_base_url)
        && provider.get("wire_api").and_then(Item::as_str) == Some("responses")
        && provider.get("supports_websockets").and_then(Item::as_bool) == Some(true)
        && provider.get("env_http_headers").is_none_or(|headers| {
            let Some(headers) = headers.as_table() else {
                return false;
            };
            headers.len() == 1
                && headers.get("x-codetas-token").and_then(Item::as_str)
                    == Some("CODETAS_GATEWAY_TOKEN")
        });
    let legacy_url_key = if provider.contains_key("base_url") {
        Some("base_url")
    } else if provider.contains_key("chatgpt_base_url") {
        Some("chatgpt_base_url")
    } else {
        None
    };
    let legacy_keys = provider.iter().map(|(key, _)| key).collect::<BTreeSet<_>>();
    let mut expected_legacy_keys = [
        "name",
        "wire_api",
        "requires_openai_auth",
        "supports_websockets",
    ]
    .into_iter()
    .collect::<BTreeSet<_>>();
    if let Some(key) = legacy_url_key {
        expected_legacy_keys.insert(key);
    }
    let legacy_url = legacy_url_key
        .and_then(|key| provider.get(key))
        .and_then(Item::as_str);
    let legacy_shape_owned = document.get("model_provider").and_then(Item::as_str)
        == Some(GATEWAY_PROVIDER_ID)
        && legacy_keys == expected_legacy_keys
        && provider.get("name").and_then(Item::as_str) == Some("OpenAI")
        && legacy_url.is_some_and(is_codetas_loopback_url)
        && provider.get("wire_api").and_then(Item::as_str) == Some("responses")
        && provider.get("requires_openai_auth").and_then(Item::as_bool) == Some(false)
        && provider.get("supports_websockets").and_then(Item::as_bool) == Some(true);
    if !current_shape_owned && !legacy_shape_owned {
        return Err(
            "既存のmodel_providers.codetas_gatewayはCODETASの既知の設定と一致しないため変更しません"
                .into(),
        );
    }
    let providers = document
        .get_mut("model_providers")
        .and_then(Item::as_table_mut)
        .ok_or("model_providers must be a TOML table")?;
    providers.remove(GATEWAY_PROVIDER_ID);
    Ok(())
}

fn configure_native_codex_gateway(
    document: &mut DocumentMut,
    gateway_base_url: &str,
    model: &str,
    catalog_path: &Path,
) -> Result<(), String> {
    if let Some(existing_base_url) = document.get("openai_base_url").and_then(Item::as_str) {
        if existing_base_url != gateway_base_url {
            return Err(
                "既存のopenai_base_urlはユーザー所有のため上書きしません。内容を確認してから再実行してください"
                    .into(),
            );
        }
    }
    if let Some(legacy_base_url) = document.get("chatgpt_base_url").and_then(Item::as_str) {
        if legacy_base_url != gateway_base_url {
            return Err(
                "既存のchatgpt_base_urlはユーザー所有のため上書きしません。内容を確認してから再実行してください"
                    .into(),
            );
        }
        document.remove("chatgpt_base_url");
    }
    remove_legacy_codetas_provider_if_owned(document, gateway_base_url)?;
    // Preserve Codex's native provider identity while overriding the built-in
    // OpenAI transport. Namespaced catalog entries then remain selectable as
    // routed models without moving or rewriting existing `openai` sessions.
    document["model_provider"] = value("openai");
    document["model"] = value(model);
    document["model_catalog_json"] = value(catalog_path.to_string_lossy().into_owned());
    document["openai_base_url"] = value(gateway_base_url);
    Ok(())
}

#[cfg(feature = "validation-build")]
fn codex_home() -> Result<PathBuf, String> {
    dirs::home_dir()
        .map(|home| home.join(".codex-codetas-validation"))
        .ok_or_else(|| "検証用Codexホームを特定できません".into())
}

#[cfg(not(feature = "validation-build"))]
fn codex_home() -> Result<PathBuf, String> {
    if let Some(home) = env::var_os("CODEX_HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(home));
    }
    dirs::home_dir()
        .map(|home| home.join(".codex"))
        .ok_or_else(|| "ホームフォルダを特定できません".into())
}

fn write_codex_catalog(settings: &GatewaySettings) -> Result<PathBuf, String> {
    let path = codex_home()?.join("codetas-model-catalog.json");
    let content = serialize_codex_catalog(settings, None)?;
    atomic_write(&path, &content)?;
    Ok(path)
}

fn serialize_codex_catalog(
    settings: &GatewaySettings,
    selected_model: Option<&str>,
) -> Result<Vec<u8>, String> {
    let catalog = build_codex_catalog(settings);
    catalog
        .validate_for_codex(selected_model)
        .map_err(|error| format!("Codexモデルカタログの事前検証に失敗しました: {error}"))?;
    serde_json::to_vec_pretty(&catalog)
        .map_err(|error| format!("Codexモデルカタログを生成できません: {error}"))
}

fn read_optional_file(path: &Path) -> Result<Option<Vec<u8>>, String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("既存ファイルを確認できません: {error}")),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("対象は通常ファイルではないため変更しません".into());
    }
    fs::read(path)
        .map(Some)
        .map_err(|error| format!("既存ファイルを退避できません: {error}"))
}

fn restore_optional_file(path: &Path, content: Option<&[u8]>) -> Result<(), String> {
    match content {
        Some(content) => atomic_write(path, content),
        None => match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!("新規作成ファイルを取り除けません: {error}")),
        },
    }
}

fn set_codex_catalog_path(path: &Path) -> Result<(), String> {
    let config_path = codex_config_path()?;
    let existing = match fs::read_to_string(&config_path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(format!("Codex設定を読めません: {error}")),
    };
    let mut document = if existing.trim().is_empty() {
        DocumentMut::new()
    } else {
        existing
            .parse::<DocumentMut>()
            .map_err(|error| format!("既存のCodex設定を解析できません: {error}"))?
    };
    document["model_catalog_json"] = value(path.to_string_lossy().into_owned());
    atomic_write(&config_path, document.to_string().as_bytes())
}

fn codex_gateway_is_configured(
    app: &AppHandle,
    settings: &GatewaySettings,
) -> Result<bool, String> {
    let path = codex_config_path()?;
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.to_string()),
    };
    let document = match content.parse::<DocumentMut>() {
        Ok(document) => document,
        Err(_) => return Ok(false),
    };
    let expected_url = gateway_url(settings);
    let runtime_url = runtime_gateway_url(app);
    let selected = document.get("model_provider").and_then(Item::as_str);
    let native_base_url = document
        .get("openai_base_url")
        .and_then(Item::as_str)
        .is_some_and(|value| {
            value == expected_url.as_str()
                || runtime_url
                    .as_deref()
                    .is_some_and(|runtime| value == runtime)
        });
    if selected.is_none_or(|provider| provider == "openai") && native_base_url {
        return Ok(true);
    }

    let registered = document
        .get("model_providers")
        .and_then(Item::as_table)
        .and_then(|providers| providers.get(GATEWAY_PROVIDER_ID))
        .and_then(Item::as_table)
        .and_then(|gateway| gateway.get("base_url"))
        .and_then(Item::as_str)
        .is_some_and(|value| {
            value == expected_url.as_str()
                || runtime_url
                    .as_deref()
                    .is_some_and(|runtime| value == runtime)
        });
    Ok(selected == Some(GATEWAY_PROVIDER_ID) && registered)
}

fn reconcile_owned_codex_runtime_url(app: &AppHandle, actual_url: &str) -> Result<(), String> {
    let journal_path = codex_journal_path(app)?;
    reconcile_owned_codex_runtime_url_path(&journal_path, actual_url)
}

pub(crate) fn reconcile_owned_codex_runtime_url_path(
    journal_path: &Path,
    actual_url: &str,
) -> Result<(), String> {
    let Some(mut journal) = read_codex_journal(journal_path)? else {
        return Ok(());
    };
    let (config_path, _, _, _) =
        validate_codex_journal_paths_from_journal_path(journal_path, &journal)?;
    if journal.installed_base_url == actual_url {
        return Ok(());
    }

    let original =
        fs::read(&config_path).map_err(|error| format!("Codex設定を読めません: {error}"))?;
    if original.len() > 16 * 1024 * 1024 {
        return Err("Codex設定が大きすぎます".into());
    }
    let source = std::str::from_utf8(&original).map_err(|_| "Codex設定がUTF-8ではありません")?;
    let mut document = source
        .parse::<DocumentMut>()
        .map_err(|error| format!("Codex設定を解析できません: {error}"))?;
    if journal.routing_mode == CodexRoutingMode::OpenAiBaseUrl {
        let selected = document
            .get("model_provider")
            .and_then(Item::as_str)
            .is_none_or(|provider| provider == "openai");
        let installed = document.get("openai_base_url").and_then(Item::as_str);
        if !selected || installed != Some(journal.installed_base_url.as_str()) {
            return Err(
                "Codex設定がCODETASの前回適用値から変更されているため、動的ポートへ更新しません"
                    .into(),
            );
        }
        document["openai_base_url"] = value(actual_url);
        atomic_write(&config_path, document.to_string().as_bytes())?;

        journal.installed_base_url = actual_url.to_string();
        let next_journal = serde_json::to_vec_pretty(&journal)
            .map_err(|error| format!("Codex復元情報を生成できません: {error}"))?;
        if let Err(error) = atomic_write(journal_path, &next_journal) {
            let _ = atomic_write(&config_path, &original);
            return Err(format!(
                "動的ポートの復元情報を保存できないためCodex設定を戻しました: {error}"
            ));
        }
        return Ok(());
    }

    let selected =
        document.get("model_provider").and_then(Item::as_str) == Some(GATEWAY_PROVIDER_ID);
    let gateway = document
        .get_mut("model_providers")
        .and_then(Item::as_table_mut)
        .and_then(|providers| providers.get_mut(GATEWAY_PROVIDER_ID))
        .and_then(Item::as_table_mut);
    let Some(gateway) = gateway else {
        return Ok(());
    };
    let installed = gateway.get("base_url").and_then(Item::as_str);
    if !selected || installed != Some(journal.installed_base_url.as_str()) {
        return Err(
            "Codex設定がCODETASの前回適用値から変更されているため、動的ポートへ更新しません".into(),
        );
    }
    gateway["base_url"] = value(actual_url);
    atomic_write(&config_path, document.to_string().as_bytes())?;

    journal.installed_base_url = actual_url.to_string();
    let next_journal = serde_json::to_vec_pretty(&journal)
        .map_err(|error| format!("Codex復元情報を生成できません: {error}"))?;
    if let Err(error) = atomic_write(journal_path, &next_journal) {
        let _ = atomic_write(&config_path, &original);
        return Err(format!(
            "動的ポートの復元情報を保存できないためCodex設定を戻しました: {error}"
        ));
    }
    Ok(())
}

fn gateway_url(settings: &GatewaySettings) -> String {
    let host = match settings.runtime.host.as_str() {
        "localhost" | "0.0.0.0" => "127.0.0.1".to_string(),
        "::" => "[::1]".to_string(),
        host if host.contains(':') => format!("[{host}]"),
        host => host.to_string(),
    };
    format!("http://{host}:{}/v1", settings.runtime.port)
}

fn atomic_write(path: &Path, content: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| format!("設定フォルダを作れません: {error}"))?;
    }
    let temporary = path.with_extension(format!(
        "codetas-{}-{}.tmp",
        std::process::id(),
        ATOMIC_WRITE_SEQUENCE.fetch_add(1, Ordering::Relaxed),
    ));
    let _ = fs::remove_file(&temporary);
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temporary)
        .map_err(|error| format!("一時設定ファイルを作れません: {error}"))?;
    if let Err(error) = file.write_all(content).and_then(|_| file.sync_all()) {
        let _ = fs::remove_file(&temporary);
        return Err(format!("設定を書き込めません: {error}"));
    }
    if let Err(error) = replace_file(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(format!("設定を置き換えられません: {error}"));
    }
    Ok(())
}

#[cfg(not(windows))]
fn replace_file(temporary: &Path, path: &Path) -> std::io::Result<()> {
    fs::rename(temporary, path)
}

#[cfg(windows)]
fn replace_file(temporary: &Path, path: &Path) -> std::io::Result<()> {
    if !path.exists() {
        return fs::rename(temporary, path);
    }
    let backup = path.with_extension(format!(
        "codetas-replace-{}-{}.bak",
        std::process::id(),
        WINDOWS_REPLACE_SEQUENCE.fetch_add(1, Ordering::Relaxed),
    ));
    let _ = fs::remove_file(&backup);
    fs::rename(path, &backup)?;
    match fs::rename(temporary, path) {
        Ok(()) => {
            let _ = fs::remove_file(backup);
            Ok(())
        }
        Err(error) => {
            let _ = fs::rename(&backup, path);
            Err(error)
        }
    }
}

fn remove_owned_file(path: &Path) -> Result<bool, String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(format!("CODETAS所有ファイルを確認できません: {error}")),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("CODETAS所有ファイルは通常ファイルではないため削除しません".into());
    }
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("CODETAS所有ファイルを削除できません: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GATEWAY_URL: &str = "http://127.0.0.1:42421/v1";
    const MODEL: &str = "opencode-go/deepseek-v4-flash";

    fn configure(source: &str) -> Result<DocumentMut, String> {
        let mut document = source
            .parse::<DocumentMut>()
            .map_err(|error| error.to_string())?;
        configure_native_codex_gateway(
            &mut document,
            GATEWAY_URL,
            MODEL,
            Path::new("/Users/test/.codex/codetas-model-catalog.json"),
        )?;
        Ok(document)
    }

    #[test]
    fn native_routing_writes_selected_model_and_preserves_openai_identity() {
        let document =
            configure("model_provider = \"openai\"\nmodel = \"opencode-go/deepseek-v4-flash\"\n")
                .unwrap();

        assert_eq!(document["model_provider"].as_str(), Some("openai"));
        assert_eq!(document["model"].as_str(), Some(MODEL));
        assert_eq!(document["openai_base_url"].as_str(), Some(GATEWAY_URL));
        assert!(document.get("chatgpt_base_url").is_none());
        assert!(document.get("model_providers").is_none());
    }

    #[test]
    fn model_selection_preserves_catalog_models_and_rejects_unknown_models() {
        let mut settings = GatewaySettings::default();
        enable_codex_openai_passthrough(&mut settings).unwrap();
        settings.providers.push(ProviderDefinition {
            id: "opencode-go".into(),
            enabled: true,
            models: vec!["deepseek-v4-flash".into()],
            default_model: Some("deepseek-v4-flash".into()),
            ..ProviderDefinition::default()
        });
        assert_eq!(
            select_codex_model(&settings, None, Some("opencode-go/deepseek-v4-flash")).as_deref(),
            Some("opencode-go/deepseek-v4-flash")
        );
        assert_eq!(
            select_codex_model(&settings, None, Some("missing/model")).as_deref(),
            Some("gpt-5.6-sol")
        );
        assert_eq!(
            select_codex_model(&settings, None, Some("gpt-5.6-terra")).as_deref(),
            Some("gpt-5.6-terra")
        );
        assert_eq!(
            select_codex_model(&settings, Some("gpt-5.6-luna"), Some("gpt-5.6-terra")).as_deref(),
            Some("gpt-5.6-luna")
        );
    }

    #[test]
    fn legacy_codetas_provider_is_removed_automatically() {
        let document = configure(
            "model_provider = \"codetas_gateway\"\nopenai_base_url = \"http://127.0.0.1:42421/v1\"\n\
             [model_providers.codetas_gateway]\nname = \"CODETAS Gateway\"\nbase_url = \"http://127.0.0.1:42421/v1\"\nwire_api = \"responses\"\nsupports_websockets = true\n",
        )
        .unwrap();

        assert_eq!(document["model_provider"].as_str(), Some("openai"));
        assert_eq!(document["openai_base_url"].as_str(), Some(GATEWAY_URL));
        assert!(document["model_providers"]
            .as_table()
            .is_some_and(|providers| !providers.contains_key(GATEWAY_PROVIDER_ID)));
    }

    #[test]
    fn historical_openai_named_codetas_provider_is_removed_automatically() {
        let document = configure(
            "model_provider = \"codetas_gateway\"\n\
             [model_providers.codetas_gateway]\nname = \"OpenAI\"\nbase_url = \"http://127.0.0.1:42421/v1\"\nwire_api = \"responses\"\nrequires_openai_auth = false\nsupports_websockets = true\n",
        )
        .unwrap();

        assert_eq!(document["model_provider"].as_str(), Some("openai"));
        assert_eq!(document["openai_base_url"].as_str(), Some(GATEWAY_URL));
        assert!(document["model_providers"]
            .as_table()
            .is_some_and(|providers| !providers.contains_key(GATEWAY_PROVIDER_ID)));
    }

    #[test]
    fn oldest_dynamic_port_codetas_provider_is_removed_automatically() {
        let document = configure(
            "model_provider = \"codetas_gateway\"\n\
             [model_providers.codetas_gateway]\nname = \"OpenAI\"\nchatgpt_base_url = \"http://127.0.0.1:42423/v1\"\nwire_api = \"responses\"\nrequires_openai_auth = false\nsupports_websockets = true\n",
        )
        .unwrap();
        assert_eq!(document["model_provider"].as_str(), Some("openai"));
        assert_eq!(document["openai_base_url"].as_str(), Some(GATEWAY_URL));
        assert!(document["model_providers"]
            .as_table()
            .is_some_and(|providers| !providers.contains_key(GATEWAY_PROVIDER_ID)));
    }

    #[test]
    fn lookalike_legacy_provider_with_remote_url_is_not_removed() {
        let error = configure(
            "model_provider = \"codetas_gateway\"\n\
             [model_providers.codetas_gateway]\nname = \"OpenAI\"\nchatgpt_base_url = \"https://example.invalid/v1\"\nwire_api = \"responses\"\nrequires_openai_auth = false\nsupports_websockets = true\n",
        )
        .unwrap_err();
        assert!(error.contains("既知の設定と一致しない"));
    }

    #[test]
    fn lookalike_legacy_provider_with_extra_keys_is_not_removed() {
        let error = configure(
            "model_provider = \"codetas_gateway\"\n\
             [model_providers.codetas_gateway]\nname = \"OpenAI\"\nchatgpt_base_url = \"http://127.0.0.1:42423/v1\"\nwire_api = \"responses\"\nrequires_openai_auth = false\nsupports_websockets = true\ncustom = \"user-owned\"\n",
        )
        .unwrap_err();
        assert!(error.contains("既知の設定と一致しない"));
    }

    #[test]
    fn current_codetas_provider_with_extra_keys_is_not_removed() {
        let error = configure(
            "model_provider = \"codetas_gateway\"\n\
             [model_providers.codetas_gateway]\nname = \"CODETAS Gateway\"\nbase_url = \"http://127.0.0.1:42421/v1\"\nwire_api = \"responses\"\nsupports_websockets = true\ncustom = \"user-owned\"\n",
        )
        .unwrap_err();
        assert!(error.contains("既知の設定と一致しない"));
    }

    #[test]
    fn owned_chatgpt_base_url_is_migrated_to_openai_base_url() {
        let document = configure(
            "model_provider = \"openai\"\nchatgpt_base_url = \"http://127.0.0.1:42421/v1\"\n",
        )
        .unwrap();
        assert!(document.get("chatgpt_base_url").is_none());
        assert_eq!(document["openai_base_url"].as_str(), Some(GATEWAY_URL));
    }

    #[test]
    fn user_owned_chatgpt_base_url_is_not_overwritten() {
        let error = configure(
            "model_provider = \"openai\"\nchatgpt_base_url = \"https://example.invalid/backend-api/codex\"\n",
        )
        .unwrap_err();

        assert!(error.contains("ユーザー所有"));
    }

    #[test]
    fn user_owned_openai_base_url_is_not_overwritten() {
        let error = configure(
            "model_provider = \"openai\"\nopenai_base_url = \"https://example.invalid/v1\"\n",
        )
        .unwrap_err();
        assert!(error.contains("ユーザー所有"));
    }

    #[test]
    fn cli_registry_covers_supported_local_clients_without_duplicate_ids() {
        let ids = LOCAL_CLI_CANDIDATES
            .iter()
            .map(|candidate| candidate.id)
            .collect::<BTreeSet<_>>();
        assert_eq!(ids.len(), LOCAL_CLI_CANDIDATES.len());
        for id in [
            "codex", "claude", "grok", "agy", "qwen", "kimi", "opencode", "gemini", "kiro",
        ] {
            assert!(ids.contains(id), "missing local CLI candidate: {id}");
        }
        for id in ["codex", "claude", "grok", "kimi"] {
            assert!(
                LOCAL_CLI_CANDIDATES
                    .iter()
                    .find(|candidate| candidate.id == id)
                    .is_some_and(|candidate| candidate.supports_provider),
                "{id} should register from a local login"
            );
        }
    }

    #[test]
    fn gui_safe_cli_search_includes_vendor_home_bins() {
        let home = dirs::home_dir().expect("home");
        let kimi = home.join(".kimi-code/bin/kimi");
        let extras = [
            home.join(".kimi-code/bin"),
            home.join(".opencode/bin"),
            home.join(".claude/bin"),
            home.join(".kiro/bin"),
        ];
        for directory in extras {
            assert!(
                directory.starts_with(&home),
                "vendor CLI directory must stay under the user home"
            );
        }
        assert!(kimi.ends_with(".kimi-code/bin/kimi"));
    }

    #[test]
    fn unsupported_cli_probe_never_runs_a_command() {
        assert_eq!(
            deep_probe_local_cli("unknown-cli", Path::new("/definitely/not/executable")),
            (
                "unsupported".to_string(),
                "安全な非対話probeが未定義です".to_string()
            )
        );
    }

    #[test]
    fn kimi_provider_list_is_authenticated_without_registering() {
        assert!(kimi_provider_list_is_authenticated(
            "managed:kimi-code  type=kimi  models=4  source=oauth\nDefault model: kimi-code/k3\n"
        ));
        assert!(!kimi_provider_list_is_authenticated(
            "managed:kimi-code  type=kimi  models=0  source=oauth\n"
        ));
        assert_eq!(
            classify_local_cli_probe(
                "kimi",
                true,
                "managed:kimi-code  type=kimi  models=4  source=oauth\n"
            ),
            (
                "authenticated".to_string(),
                "Kimi CLIはログイン済みです。CODETASが取り込みます".to_string()
            )
        );
    }

    #[test]
    fn codex_not_logged_in_is_never_ready() {
        assert_eq!(
            classify_local_cli_probe("codex", false, "Not logged in"),
            (
                "authenticationRequired".to_string(),
                "headless実行の認証が必要です".to_string()
            )
        );
        assert_eq!(
            classify_local_cli_probe("codex", true, "Logged in using ChatGPT"),
            ("ready".to_string(), "非対話probeに成功しました".to_string())
        );
    }

    #[cfg(unix)]
    #[test]
    fn cli_probe_drains_large_output_and_bounds_retained_text() {
        let output = command_output(
            Path::new("/bin/sh"),
            &["-c", "yes x | head -c 131072"],
            Duration::from_secs(5),
        )
        .expect("large-output command should finish without a pipe deadlock");
        assert!(output.success);
        assert!(output.text.len() <= 64 * 1024);
    }

    #[cfg(unix)]
    #[test]
    fn cli_probe_cleans_descendant_when_parent_exits_first() {
        let marker = std::env::temp_dir().join(format!(
            "codetas-probe-early-parent-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let script = format!(
            "sleep 30 & child=$!; printf '%s' \"$child\" > {}",
            marker.to_string_lossy()
        );
        let started = Instant::now();
        let output = command_output(
            Path::new("/bin/sh"),
            &["-c", &script],
            Duration::from_millis(250),
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "reader wait ignored the probe deadline"
        );
        assert!(
            output.is_some_and(|result| result.success),
            "the leader's successful status/output should be preserved after descendant cleanup"
        );
        let pid = fs::read_to_string(&marker)
            .expect("child pid marker")
            .trim()
            .to_string();
        let stopped = !Command::new("/bin/kill")
            .args(["-0", &pid])
            .status()
            .expect("kill -0")
            .success();
        let _ = fs::remove_file(marker);
        assert!(stopped, "descendant process {pid} survived parent exit");
    }

    #[cfg(unix)]
    #[test]
    fn cli_probe_timeout_kills_descendant_process_group() {
        let marker = std::env::temp_dir().join(format!(
            "codetas-probe-child-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let script = format!(
            "sleep 30 & child=$!; printf '%s' \"$child\" > {}; wait",
            marker.to_string_lossy()
        );
        assert!(command_output(
            Path::new("/bin/sh"),
            &["-c", &script],
            Duration::from_millis(250),
        )
        .is_none());
        let pid = fs::read_to_string(&marker)
            .expect("child pid marker")
            .trim()
            .to_string();
        let stopped = !Command::new("/bin/kill")
            .args(["-0", &pid])
            .status()
            .expect("kill -0")
            .success();
        let _ = fs::remove_file(marker);
        assert!(stopped, "descendant process {pid} survived timeout");
    }

    #[test]
    fn cli_executable_lookup_rejects_paths_and_finds_installed_gh() {
        assert!(find_cli_executable("../gh").is_none());
        assert!(find_cli_executable("gh").is_some());
    }

    #[test]
    fn claude_auth_status_detects_login_without_billing() {
        assert!(!claude_auth_status_is_authenticated(
            "Not logged in. Run claude auth login to authenticate.\n"
        ));
        assert!(claude_auth_status_is_authenticated(
            "Logged in as user@example.com\n"
        ));
        assert_eq!(
            classify_local_cli_probe("claude", true, "Logged in as user@example.com\n"),
            (
                "authenticated".to_string(),
                "Claude CLIはログイン済みです。CODETASが取り込みます".to_string()
            )
        );
    }

    #[test]
    fn every_local_cli_has_a_codetas_registration_target_and_hint() {
        for candidate in LOCAL_CLI_CANDIDATES {
            assert!(
                candidate.codetas_provider_id.is_some(),
                "{} needs a CODETAS provider id",
                candidate.id
            );
            assert!(
                !local_cli_registration_hint(candidate.id).is_empty(),
                "{} needs a registration hint",
                candidate.id
            );
        }
    }

    #[test]
    fn kimi_environment_credential_without_a_value_still_needs_registration() {
        let provider = ProviderDefinition {
            id: "kimi".into(),
            credential: ProviderCredential {
                source: CredentialSource::Environment,
                reference: Some("KIMI_API_KEY".into()),
                ..ProviderCredential::default()
            },
            ..ProviderDefinition::default()
        };
        assert!(!provider_has_usable_credential(&provider));
    }

    #[test]
    fn authenticated_gh_cli_is_registered_without_persisting_a_token() {
        let mut settings = GatewaySettings::default();
        settings.providers.clear();
        auto_register_authenticated_cli_providers(&mut settings).unwrap();

        let provider = settings
            .providers
            .iter()
            .find(|provider| provider.id == "github-models")
            .expect("authenticated gh should register GitHub Models");
        assert_eq!(provider.credential.source, CredentialSource::Command);
        assert!(provider.credential.reference.is_none());
        let command = provider.credential.command.as_ref().unwrap();
        assert!(Path::new(&command.program).is_absolute());
        assert_eq!(command.args, ["auth", "token", "--hostname", "github.com"]);
    }

    #[test]
    fn existing_github_provider_is_never_replaced_by_cli_detection() {
        let mut settings = GatewaySettings::default();
        settings.providers.clear();
        let mut provider = provider_presets()
            .into_iter()
            .find(|preset| preset.id == "github-models")
            .unwrap()
            .instantiate(None)
            .unwrap();
        provider.name = "User-owned GitHub Models".into();
        settings.providers.push(provider);

        auto_register_authenticated_cli_providers(&mut settings).unwrap();
        let github = settings
            .providers
            .iter()
            .find(|provider| provider.id == "github-models")
            .expect("existing GitHub provider should remain");
        assert_eq!(github.name, "User-owned GitHub Models");
    }
}
