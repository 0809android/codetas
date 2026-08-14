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

pub(crate) mod cli_scan;
pub(crate) mod codex;
pub(crate) mod codex_restore;
pub(crate) mod diagnostics;
pub(crate) mod fsutil;
pub(crate) mod gateway_ops;
pub(crate) mod integration;
pub(crate) mod presets;
pub(crate) mod runtime;
pub(crate) mod service_cmds;

pub(crate) use cli_scan::*;
pub(crate) use codex::*;
pub(crate) use codex_restore::*;
pub(crate) use diagnostics::*;
pub(crate) use fsutil::*;
pub(crate) use gateway_ops::*;
pub(crate) use integration::*;
pub(crate) use presets::*;
pub(crate) use runtime::*;
pub(crate) use service_cmds::*;


const GATEWAY_PROVIDER_ID: &str = "codetas_gateway";
const CODEX_JOURNAL_VERSION: u8 = 1;

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

