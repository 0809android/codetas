use crate::clients::{self, ClientIntegrationReport};
use crate::service::{self, ServiceInstallReport, ServiceStatus, ServiceUninstallReport};
use codetas_gateway::{
    adopt_local_cli_sessions, auth_store_is_configured, build_codex_catalog, check_signed_update,
    configure_auth_store_path, detect_local_cli_session, discover_provider_models,
    dismiss_cli_import, has_stored_session, list_observability_trash, list_stored_oauth_accounts,
    login_provider_oauth, login_provider_oauth_new_account, oauth_provider_registry,
    oauth_session_credential, parse_gateway_settings_json, preview_observability_cleanup,
    provider_presets, provider_supports_native_oauth, read_observability_breakdown,
    read_observability_summary, read_recent_observability_events, remove_stored_oauth_account,
    restore_cli_import, restore_observability_trash, session_key_provider,
    start_gateway_with_options, test_provider_connection, trash_observability_cleanup,
    AccountReference, AuxiliaryInputMode, ClientIntegrationSettings, CredentialCommand,
    CredentialSource, CredentialTransport, GatewayHandle, GatewayRuntimeOptions, GatewaySettings,
    ManagedClientSettings, ObservabilityBreakdown, ObservabilityCleanupPreview,
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

pub(crate) mod account_usage;
pub(crate) mod agent_tools;
pub(crate) mod cli_scan;
pub(crate) mod codex;
mod codex_cli_update;
mod codex_models;
pub(crate) mod codex_restore;
pub(crate) mod diagnostics;
pub(crate) mod fsutil;
pub(crate) mod gateway_ops;
pub(crate) mod integration;
mod model_refresh;
pub(crate) mod presets;
pub(crate) mod runtime;
pub(crate) mod service_cmds;

#[cfg(test)]
pub(crate) use cli_scan::*;
pub(crate) use codex::*;
pub(crate) use codex_restore::*;
pub(crate) use diagnostics::*;
pub(crate) use fsutil::*;
#[cfg(test)]
pub(crate) use gateway_ops::*;
pub(crate) use integration::*;
pub(crate) use runtime::*;

const GATEWAY_PROVIDER_ID: &str = "codetas_gateway";
const CODEX_JOURNAL_VERSION: u8 = 1;

pub fn load_gateway_settings(app: &AppHandle) -> Result<GatewaySettings, String> {
    load_settings(app)
}

pub fn load_gateway_settings_at(path: &Path) -> Result<GatewaySettings, String> {
    load_settings_file(path).map(|(settings, _)| settings)
}

pub fn apply_official_codex_fallback(app: &AppHandle) -> Result<bool, String> {
    apply_temporary_official_codex_fallback(app)
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum CodexRoutingMode {
    #[default]
    ProviderTable,
    OpenAiBaseUrl,
}

#[derive(Default)]
pub struct GatewayManager {
    handle: Mutex<Option<GatewayHandle>>,
    settings_mutation: Mutex<()>,
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
    locally_owned: bool,
    official_fallback_active: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    fallback_reconnect_error: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderUpsertInput {
    provider: ProviderDefinition,
    make_default: bool,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexGatewayInstallInput {
    pub model: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentPresetApplyInput {
    main_model: Option<String>,
    vision_model: String,
    image_model: Option<String>,
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

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderOAuthAccountView {
    pub account_id: String,
    pub label: String,
    pub email: Option<String>,
    pub store_key: Option<String>,
    pub main: bool,
    pub enabled: bool,
    pub expires_at_ms: i64,
    pub source: String,
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
    hermes: bool,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CodexInstallJournal {
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
    #[serde(default)]
    official_fallback_active: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_watchdog_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_watchdog_error_at: Option<u64>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CodexAgentInstall {
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

impl CodexRestoreReport {
    /// With conflicts left, Codex still points at the gateway, so stopping it would
    /// strand Codex on a URL nothing serves.
    pub fn allows_gateway_stop(&self) -> bool {
        self.conflicts.is_empty()
    }
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
pub(crate) struct LocalCliCandidate {
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
        supports_provider: true,
        codetas_provider_id: Some("google-antigravity"),
    },
    LocalCliCandidate {
        id: "qwen",
        name: "Qwen Code",
        executable: "qwen",
        supports_provider: true,
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
    LocalCliCandidate {
        id: "muse",
        name: "Muse CLI",
        executable: "muse",
        supports_provider: true,
        codetas_provider_id: Some("meta"),
    },
    LocalCliCandidate {
        id: "zai",
        name: "Z.AI / GLM CLI",
        executable: "zai",
        supports_provider: true,
        codetas_provider_id: Some("zai"),
    },
    LocalCliCandidate {
        id: "minimax",
        name: "MiniMax CLI",
        executable: "minimax",
        supports_provider: true,
        codetas_provider_id: Some("minimax"),
    },
    LocalCliCandidate {
        id: "vibe",
        name: "Mistral Vibe",
        executable: "vibe",
        supports_provider: true,
        codetas_provider_id: Some("mistral"),
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

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) const GATEWAY_URL: &str = "http://127.0.0.1:42421/v1";
    pub(super) const BACKEND_URL: &str = "http://127.0.0.1:42421/backend-api";
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
        // Codex 0.166 fails `account/read` on a backend that is not HTTPS.
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
    fn a_backend_url_an_older_build_wrote_is_removed_on_any_port() {
        for port in [42421, 50000] {
            let document = configure(&format!(
                "model_provider = \"openai\"\nchatgpt_base_url = \"http://127.0.0.1:{port}/backend-api\"\n",
            ))
            .unwrap();
            assert!(document.get("chatgpt_base_url").is_none(), "{port}");
            assert_eq!(document["openai_base_url"].as_str(), Some(GATEWAY_URL));
        }
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
    fn official_fallback_reclaim_overwrites_foreign_loopback_openai_base_url() {
        let mut document = "model_provider = \"openai\"\nopenai_base_url = \"http://127.0.0.1:10100/v1\"\nmodel_catalog_json = \"/Users/test/.codex/opencodex-catalog.json\"\n"
            .parse::<DocumentMut>()
            .unwrap();
        configure_native_codex_gateway_with_claim(
            &mut document,
            GATEWAY_URL,
            MODEL,
            Path::new("/Users/test/.codex/codetas-model-catalog.json"),
            CodexGatewayTransportClaim::ReclaimLocalLoopback,
        )
        .unwrap();
        assert_eq!(document["openai_base_url"].as_str(), Some(GATEWAY_URL));
        assert_eq!(
            document["model_catalog_json"].as_str(),
            Some("/Users/test/.codex/codetas-model-catalog.json")
        );
    }

    #[test]
    fn official_fallback_reclaim_does_not_overwrite_remote_openai_base_url() {
        let mut document =
            "model_provider = \"openai\"\nopenai_base_url = \"https://example.invalid/v1\"\n"
                .parse::<DocumentMut>()
                .unwrap();
        let error = configure_native_codex_gateway_with_claim(
            &mut document,
            GATEWAY_URL,
            MODEL,
            Path::new("/Users/test/.codex/codetas-model-catalog.json"),
            CodexGatewayTransportClaim::ReclaimLocalLoopback,
        )
        .unwrap_err();
        assert!(error.contains("ユーザー所有"));
        assert_eq!(
            document["openai_base_url"].as_str(),
            Some("https://example.invalid/v1")
        );
    }

    #[test]
    fn reapply_after_official_fallback_reclaims_foreign_loopback_url() {
        let unique = ATOMIC_WRITE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "codetas-fallback-reclaim-{}-{unique}",
            std::process::id()
        ));
        let codex_home = root.join("codex");
        let journal_dir = root.join("journal");
        fs::create_dir_all(&codex_home).unwrap();
        fs::create_dir_all(&journal_dir).unwrap();
        let config_path = codex_home.join("config.toml");
        let catalog_path = codex_home.join("codetas-model-catalog.json");
        let journal_path = journal_dir.join("codex-install-journal.json");
        fs::write(
            &config_path,
            "model_provider = \"openai\"\nmodel = \"opencode-go/deepseek-v4-flash\"\nopenai_base_url = \"http://127.0.0.1:10100/v1\"\nmodel_catalog_json = \"/tmp/opencodex-catalog.json\"\n",
        )
        .unwrap();
        let mut settings = GatewaySettings::default();
        enable_codex_openai_passthrough(&mut settings).unwrap();
        let model = select_codex_model(&settings, None, None).expect("default Codex model");
        let journal = CodexInstallJournal {
            version: CODEX_JOURNAL_VERSION,
            config_path: config_path.to_string_lossy().into_owned(),
            backup_path: None,
            catalog_path: catalog_path.to_string_lossy().into_owned(),
            catalog_backup_path: None,
            catalog_existed: Some(false),
            installed_model: model.clone(),
            installed_base_url: GATEWAY_URL.into(),
            routing_mode: CodexRoutingMode::OpenAiBaseUrl,
            installed_local_token: false,
            installed_agents: None,
            official_fallback_active: true,
            last_watchdog_error: None,
            last_watchdog_error_at: None,
        };
        fs::write(&journal_path, serde_json::to_vec_pretty(&journal).unwrap()).unwrap();

        let reapplied =
            reapply_codex_gateway_after_official_fallback_at(&journal_path, &settings).unwrap();
        assert!(reapplied);
        let restored = fs::read_to_string(&config_path).unwrap();
        let document = restored.parse::<DocumentMut>().unwrap();
        assert_eq!(document["openai_base_url"].as_str(), Some(GATEWAY_URL));
        assert_eq!(document["model"].as_str(), Some(model.as_str()));
        let next = read_codex_journal(&journal_path).unwrap().unwrap();
        assert!(!next.official_fallback_active);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn temporary_official_fallback_restores_owned_openai_base_url() {
        let backup = "model_provider = \"openai\"\nmodel = \"gpt-5.4\"\n"
            .parse::<DocumentMut>()
            .unwrap();
        let mut document = configure("model_provider = \"openai\"\nmodel = \"gpt-5.4\"\n").unwrap();
        let journal = CodexInstallJournal {
            version: CODEX_JOURNAL_VERSION,
            config_path: "/Users/test/.codex/config.toml".into(),
            backup_path: None,
            catalog_path: "/Users/test/.codex/codetas-model-catalog.json".into(),
            catalog_backup_path: None,
            catalog_existed: Some(false),
            installed_model: MODEL.into(),
            installed_base_url: GATEWAY_URL.into(),
            routing_mode: CodexRoutingMode::OpenAiBaseUrl,
            installed_local_token: false,
            installed_agents: None,
            official_fallback_active: false,
            last_watchdog_error: None,
            last_watchdog_error_at: None,
        };
        let mut conflicts = Vec::new();
        apply_owned_codex_restore(&mut document, &backup, &journal, None, &mut conflicts).unwrap();
        assert!(conflicts.is_empty());
        assert!(document.get("openai_base_url").is_none());
        assert!(document.get("chatgpt_base_url").is_none());
        assert_eq!(document["model_provider"].as_str(), Some("openai"));
        assert_eq!(document["model"].as_str(), Some("gpt-5.4"));
    }

    #[test]
    fn temporary_official_fallback_skips_user_changed_openai_base_url() {
        let backup = "model_provider = \"openai\"\n"
            .parse::<DocumentMut>()
            .unwrap();
        let mut document =
            "model_provider = \"openai\"\nopenai_base_url = \"https://example.invalid/v1\"\n"
                .parse::<DocumentMut>()
                .unwrap();
        let journal = CodexInstallJournal {
            version: CODEX_JOURNAL_VERSION,
            config_path: "/Users/test/.codex/config.toml".into(),
            backup_path: None,
            catalog_path: "/Users/test/.codex/codetas-model-catalog.json".into(),
            catalog_backup_path: None,
            catalog_existed: Some(false),
            installed_model: MODEL.into(),
            installed_base_url: GATEWAY_URL.into(),
            routing_mode: CodexRoutingMode::OpenAiBaseUrl,
            installed_local_token: false,
            installed_agents: None,
            official_fallback_active: false,
            last_watchdog_error: None,
            last_watchdog_error_at: None,
        };
        let mut conflicts = Vec::new();
        apply_owned_codex_restore(&mut document, &backup, &journal, None, &mut conflicts).unwrap();
        assert!(conflicts
            .iter()
            .any(|item| item.contains("openai_base_url")));
        assert_eq!(
            document["openai_base_url"].as_str(),
            Some("https://example.invalid/v1")
        );
    }

    pub(super) fn fallback_temp_root(name: &str) -> PathBuf {
        let unique = ATOMIC_WRITE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("codetas-{name}-{}-{unique}", std::process::id()))
    }

    /// The settings, an official model and a routed one only the gateway can serve,
    /// both of them in the catalog.
    pub(super) fn fallback_settings() -> (GatewaySettings, String, String) {
        let mut settings = GatewaySettings::default();
        enable_codex_openai_passthrough(&mut settings).unwrap();
        settings.providers.push(ProviderDefinition {
            id: "routed".into(),
            enabled: true,
            models: vec!["picked-model".into()],
            ..ProviderDefinition::default()
        });
        let catalog = build_codex_catalog(&settings);
        let official = catalog
            .models
            .iter()
            .filter_map(|model| model.get("slug").and_then(JsonValue::as_str))
            .find(|slug| !slug.contains('/'))
            .unwrap()
            .to_string();
        (settings, official, "routed/picked-model".to_string())
    }

    /// A Codex home connected through the gateway, where `config_model` is the
    /// model Codex last wrote and `installed_model` the one the journal remembers.
    /// Returns the config, catalog and journal paths.
    pub(super) fn write_fallback_fixture(
        root: &Path,
        settings: &GatewaySettings,
        installed_model: &str,
        config_model: &str,
    ) -> (PathBuf, PathBuf, PathBuf) {
        let codex_home = root.join("codex");
        let journal_dir = root.join("journal");
        fs::create_dir_all(&codex_home).unwrap();
        fs::create_dir_all(&journal_dir).unwrap();
        let config_path = codex_home.join("config.toml");
        let catalog_path = codex_home.join("codetas-model-catalog.json");
        let journal_path = journal_dir.join("codex-install-journal.json");
        fs::write(
            &config_path,
            format!(
                "model_provider = \"openai\"\nmodel = \"{config_model}\"\nmodel_catalog_json = \"{}\"\nopenai_base_url = \"{GATEWAY_URL}\"\nchatgpt_base_url = \"{BACKEND_URL}\"\n",
                catalog_path.display()
            ),
        )
        .unwrap();
        fs::write(
            &catalog_path,
            serde_json::to_vec_pretty(&build_codex_catalog(settings)).unwrap(),
        )
        .unwrap();
        let journal = CodexInstallJournal {
            version: CODEX_JOURNAL_VERSION,
            config_path: config_path.to_string_lossy().into_owned(),
            backup_path: None,
            catalog_path: catalog_path.to_string_lossy().into_owned(),
            catalog_backup_path: None,
            catalog_existed: Some(false),
            installed_model: installed_model.into(),
            installed_base_url: GATEWAY_URL.into(),
            routing_mode: CodexRoutingMode::OpenAiBaseUrl,
            installed_local_token: false,
            installed_agents: None,
            official_fallback_active: false,
            last_watchdog_error: None,
            last_watchdog_error_at: None,
        };
        fs::write(&journal_path, serde_json::to_vec_pretty(&journal).unwrap()).unwrap();
        (config_path, catalog_path, journal_path)
    }

    pub(super) fn read_config(path: &Path) -> DocumentMut {
        fs::read_to_string(path)
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap()
    }

    #[test]
    fn temporary_official_fallback_follows_the_model_picked_in_codex() {
        let root = fallback_temp_root("fallback-picked");
        let (settings, installed, picked) = fallback_settings();
        let (config_path, _, journal_path) =
            write_fallback_fixture(&root, &settings, &installed, &picked);

        assert!(
            apply_temporary_official_codex_fallback_at(&journal_path, Some(&settings)).unwrap()
        );
        let fallback = read_config(&config_path);
        assert!(fallback.get("model").is_none());
        assert!(fallback.get("openai_base_url").is_none());
        assert!(fallback.get("chatgpt_base_url").is_none());
        let journal = read_codex_journal(&journal_path).unwrap().unwrap();
        assert!(journal.official_fallback_active);
        assert_eq!(journal.installed_model, picked);

        // Reconnecting returns to the model the user was on, not the older one.
        assert!(
            reapply_codex_gateway_after_official_fallback_at(&journal_path, &settings).unwrap()
        );
        let reconnected = read_config(&config_path);
        assert_eq!(reconnected["model"].as_str(), Some(picked.as_str()));
        assert!(reconnected.get("chatgpt_base_url").is_none());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn temporary_official_fallback_leaves_an_official_model_outside_the_catalog() {
        let root = fallback_temp_root("fallback-official-model");
        let (settings, installed, _) = fallback_settings();
        let (config_path, _, journal_path) =
            write_fallback_fixture(&root, &settings, &installed, "my-own-model");

        assert!(
            apply_temporary_official_codex_fallback_at(&journal_path, Some(&settings)).unwrap()
        );

        let fallback = read_config(&config_path);
        assert_eq!(fallback["model"].as_str(), Some("my-own-model"));
        assert!(fallback.get("openai_base_url").is_none());
        let journal = read_codex_journal(&journal_path).unwrap().unwrap();
        assert!(journal.official_fallback_active);
        assert_eq!(journal.installed_model, installed);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn temporary_official_fallback_removes_a_gateway_model_that_left_the_catalog() {
        let root = fallback_temp_root("fallback-gone-model");
        let (settings, installed, _) = fallback_settings();
        let (config_path, _, journal_path) =
            write_fallback_fixture(&root, &settings, &installed, "gone/model");

        assert!(
            apply_temporary_official_codex_fallback_at(&journal_path, Some(&settings)).unwrap()
        );

        assert!(read_config(&config_path).get("model").is_none());
        let journal = read_codex_journal(&journal_path).unwrap().unwrap();
        assert_eq!(journal.installed_model, installed);
        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn temporary_official_fallback_accepts_a_catalog_named_through_a_codex_home_alias() {
        let root = fallback_temp_root("fallback-alias");
        let (settings, installed, _) = fallback_settings();
        let (config_path, _, journal_path) =
            write_fallback_fixture(&root, &settings, &installed, &installed);
        // The journal uses the symlink while the config keeps the physical path,
        // which is what a catalog sync from a differently launched app leaves.
        let alias = root.join("alias");
        std::os::unix::fs::symlink(root.join("codex"), &alias).unwrap();
        let mut journal = read_codex_journal(&journal_path).unwrap().unwrap();
        journal.config_path = alias.join("config.toml").to_string_lossy().into_owned();
        journal.catalog_path = alias
            .join("codetas-model-catalog.json")
            .to_string_lossy()
            .into_owned();
        fs::write(&journal_path, serde_json::to_vec_pretty(&journal).unwrap()).unwrap();

        assert!(
            apply_temporary_official_codex_fallback_at(&journal_path, Some(&settings)).unwrap()
        );

        let fallback = read_config(&config_path);
        assert!(fallback.get("model_catalog_json").is_none());
        assert!(fallback.get("chatgpt_base_url").is_none());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn temporary_official_fallback_proceeds_and_keeps_a_catalog_it_did_not_generate() {
        let root = fallback_temp_root("fallback-edited-catalog");
        let (settings, installed, _) = fallback_settings();
        let (config_path, catalog_path, journal_path) =
            write_fallback_fixture(&root, &settings, &installed, &installed);
        fs::write(&catalog_path, "{\"models\":[]}").unwrap();

        assert!(
            apply_temporary_official_codex_fallback_at(&journal_path, Some(&settings)).unwrap()
        );

        let fallback = read_config(&config_path);
        assert!(fallback.get("openai_base_url").is_none());
        assert!(fallback.get("chatgpt_base_url").is_none());
        assert_eq!(
            fs::read_to_string(&catalog_path).unwrap(),
            "{\"models\":[]}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn temporary_official_fallback_refuses_when_official_codex_would_load_an_unknown_catalog() {
        let root = fallback_temp_root("fallback-unknown-catalog");
        let (settings, installed, _) = fallback_settings();
        let (config_path, catalog_path, journal_path) =
            write_fallback_fixture(&root, &settings, &installed, &installed);
        // Before CODETAS was installed, the config already pointed at this file.
        let journal_dir = journal_path.parent().unwrap();
        let backup_path = journal_dir.join("codex-config-before-test.toml");
        fs::write(
            &backup_path,
            format!("model_catalog_json = \"{}\"\n", catalog_path.display()),
        )
        .unwrap();
        let catalog_backup_path = journal_dir.join("codex-catalog-before-test.json");
        fs::write(&catalog_backup_path, "{\"models\":[\"original\"]}").unwrap();
        let mut journal = read_codex_journal(&journal_path).unwrap().unwrap();
        journal.backup_path = Some(backup_path.to_string_lossy().into_owned());
        journal.catalog_backup_path = Some(catalog_backup_path.to_string_lossy().into_owned());
        journal.catalog_existed = Some(true);
        fs::write(&journal_path, serde_json::to_vec_pretty(&journal).unwrap()).unwrap();
        fs::write(&catalog_path, "{\"models\":[]}").unwrap();

        let error =
            apply_temporary_official_codex_fallback_at(&journal_path, Some(&settings)).unwrap_err();

        assert!(error.contains("model_catalog_json"), "{error}");
        assert!(read_config(&config_path).get("openai_base_url").is_some());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn recording_no_error_clears_a_recorded_failure() {
        let root = fallback_temp_root("fallback-clear-error");
        let (settings, installed, _) = fallback_settings();
        let (_, _, journal_path) = write_fallback_fixture(&root, &settings, &installed, &installed);

        assert!(record_watchdog_error(&journal_path, Some("failure")).unwrap());
        let journal = read_codex_journal(&journal_path).unwrap().unwrap();
        assert_eq!(journal.last_watchdog_error.as_deref(), Some("failure"));

        assert!(record_watchdog_error(&journal_path, None).unwrap());
        let journal = read_codex_journal(&journal_path).unwrap().unwrap();
        assert_eq!(journal.last_watchdog_error, None);
        assert_eq!(journal.last_watchdog_error_at, None);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn cli_registry_covers_supported_local_clients_without_duplicate_ids() {
        let ids = LOCAL_CLI_CANDIDATES
            .iter()
            .map(|candidate| candidate.id)
            .collect::<BTreeSet<_>>();
        assert_eq!(ids.len(), LOCAL_CLI_CANDIDATES.len());
        for id in [
            "codex", "claude", "grok", "agy", "qwen", "kimi", "opencode", "gemini", "kiro", "muse",
            "zai", "minimax",
        ] {
            assert!(ids.contains(id), "missing local CLI candidate: {id}");
        }
        for id in [
            "codex", "claude", "grok", "kimi", "muse", "qwen", "zai", "minimax",
        ] {
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
