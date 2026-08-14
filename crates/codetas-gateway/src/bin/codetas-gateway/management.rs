use codetas_gateway::GoogleMode;
use codetas_gateway::ProviderTransport;
use std::fs;
use std::path::{Path, PathBuf};

mod cmd;

const PROVIDER_USAGE: &str = "Usage:
  codetas-gateway provider <list|presets> [--json]
  codetas-gateway provider show <id> [--json]
  codetas-gateway provider add <preset> [--id <id>] [--name <name>] [--base-url <url>] [--set-default] [--disabled]
      [--google-mode <ai-studio|vertex|cloud-code-assist>] [--project <id>] [--location <id>] [--azure-deployment <id>] [--azure-api-version <version>]
      [--transport <standard|kiro|github-copilot>] [--kiro-profile-arn <arn>]
  codetas-gateway provider edit <id> [--name <name>] [--base-url <url>] [--default-model <id|->]
      [--google-mode <ai-studio|vertex|cloud-code-assist>] [--project <id|->] [--location <id|->] [--azure-deployment <id|->] [--azure-api-version <version|->]
      [--transport <standard|kiro|github-copilot>] [--kiro-profile-arn <arn|->]
      [--responses-path <path|->] [--realtime-ws-base-url <url|->]
      [--stateless-responses <on|off>] [--strip-model-bracket-suffix <on|off>]
      [--credential-env <VAR>|--credential-keychain <ref>|--credential-oauth <ref>|--credential-command <program>|--credential-forward|--credential-none]
      [--credential-arg <value> ...] [--credential-transport <bearer|x-api-key|custom-header>] [--credential-header <name>]
      [--allow-private-network <on|off>] [--discovery <on|off>]
  codetas-gateway provider <enable|disable|remove|set-default|test> <id> [--json]
  codetas-gateway provider discover <id> [--apply] [--json]
  codetas-gateway provider login <id> [--broker <github-cli|gcloud>]
      # OAuthの秘密は外部CLIが所有し、CODETASはtoken取得コマンドだけを保存";

const ACCOUNT_USAGE: &str = "Usage:
  codetas-gateway account list [provider] [--json]
  codetas-gateway account show <provider> <id> [--json]
  codetas-gateway account add <provider> <id> --label <label>
      [--credential-env <VAR>|--credential-keychain <ref>|--credential-oauth <ref>|--credential-command <program>|--credential-forward|--credential-none]
      [--credential-arg <value> ...] [--credential-transport <bearer|x-api-key|custom-header>] [--credential-header <name>]
  codetas-gateway account use <provider> <id|auto>
  codetas-gateway account <enable|disable|remove> <provider> <id>
  codetas-gateway account strategy <quota|round-robin|fill-first>";

const MODELS_USAGE: &str = "Usage:
  codetas-gateway models list [provider] [--json]
  codetas-gateway models catalog [--json]
  codetas-gateway models add <provider> <model> [--display-name <name>] [--context-window <tokens>] [--max-output-tokens <tokens>] [--modalities <csv>]
  codetas-gateway models edit <provider> <model> [--display-name <name|->] [--context-window <tokens|0>] [--max-output-tokens <tokens|0>] [--modalities <csv|->]
  codetas-gateway models <enable|disable|remove> <provider> <model>
  codetas-gateway models selected <provider> [--set <csv>|--clear]
  codetas-gateway models sync <provider|all> [--json]";

const ROUTE_USAGE: &str = "Usage:
  codetas-gateway route list [--json]
  codetas-gateway route show <id> [--json]
  codetas-gateway route add <id> --name <name> --target <provider/model[@weight]> ... [--alias <name>] [--strategy <failover|weighted-round-robin|least-usage>] [--default-effort <level>]
  codetas-gateway route edit <id> [--name <name>] [--alias <name|->] [--target <provider/model[@weight]> ...] [--strategy <...>] [--default-effort <level|->]
  codetas-gateway route <enable|disable|remove> <id>";

const AGENT_USAGE: &str = "Usage:
  codetas-gateway agent [status] [--json]
  codetas-gateway agent set [--surface <v1|default|v2>] [--threads <n>] [--multi-agent-v2 <on|off>]
      [--main-effort <level|->] [--subagent-effort <level|->] [--subagents <csv|->] [--fallback <csv|->]
      [--web-search-model <id|->] [--vision-model <id|->] [--image-model <id|->] [--video-model <id|->] [--live-model <id|->]
      [--helper-intercept <on|off>] [--helper-target <id|->] [--helper-sources <csv>]";

const ACCESS_USAGE: &str = "Usage:
  codetas-gateway access key list [--json]
  codetas-gateway access key add <id> --label <label> --env <VAR> --scopes <csv> [--expires-at <unix-seconds>]
  codetas-gateway access key <enable|disable|remove> <id>
  codetas-gateway access endpoints [--json]";

const OBSERVE_USAGE: &str = "Usage:
  codetas-gateway observe <summary|usage> [--json]
  codetas-gateway observe breakdown [--since-ms <timestamp>] [--limit <1-50000>] [--json]
  codetas-gateway observe events [--since-ms <timestamp>] [--limit <1-500>] [--provider <id>] [--model <id>] [--status <code>] [--json|--jsonl]
  codetas-gateway observe follow [--since-ms <timestamp>] [--interval-ms <250-60000>] [--provider <id>] [--model <id>] [--status <code>] [--jsonl]";

const SYSTEM_USAGE: &str = "Usage:
  codetas-gateway system <status|validate|catalog> [--json]
  codetas-gateway system settings [--host <host>] [--port <port>] [--auto-start <on|off>] [--standalone-service <on|off>]
      [--allow-remote <on|off>] [--require-local-token <on|off>] [--dns-pinning <on|off>]
      [--request-log <on|off>] [--usage-log <on|off>] [--retention-days <days>] [--max-storage-bytes <bytes>]";


pub async fn run(group: &str, arguments: &[String], config: &Path) -> Result<(), String> {
    match group {
        "provider" => cmd::provider::provider(arguments, config).await,
        "account" => cmd::account::account(arguments, config),
        "models" | "model" => cmd::models::models(arguments, config).await,
        "route" => cmd::route::route(arguments, config),
        "agent" => cmd::agent::agent(arguments, config),
        "access" => cmd::access::access(arguments, config),
        "observe" => cmd::observe::observe(arguments, config),
        "system" => cmd::system::system(arguments, config),
        _ => Err(format!("unknown management group: {group}")),
    }
}

pub(super) fn find_executable(name: &str) -> Result<PathBuf, String> {
    let path = Path::new(name);
    if path.is_absolute() && path.is_file() {
        return fs::canonicalize(path).map_err(|error| format!("cannot resolve {name}: {error}"));
    }
    let search = std::env::var_os("PATH").ok_or_else(|| "PATH is unavailable".to_string())?;
    for directory in std::env::split_paths(&search) {
        let candidate = directory.join(name);
        if candidate.is_file() {
            return fs::canonicalize(&candidate)
                .map_err(|error| format!("cannot resolve {}: {error}", candidate.display()));
        }
        #[cfg(windows)]
        for extension in ["exe", "cmd", "bat"] {
            let candidate = directory.join(format!("{name}.{extension}"));
            if candidate.is_file() {
                return fs::canonicalize(&candidate)
                    .map_err(|error| format!("cannot resolve {}: {error}", candidate.display()));
            }
        }
    }
    Err(format!("{name} was not found on PATH"))
}

pub(super) fn parse_google_mode(value: &str) -> Result<GoogleMode, String> {
    match value {
        "ai-studio" | "aiStudio" => Ok(GoogleMode::AiStudio),
        "vertex" => Ok(GoogleMode::Vertex),
        "cloud-code-assist" | "cloudCodeAssist" => Ok(GoogleMode::CloudCodeAssist),
        _ => Err("--google-mode must be ai-studio, vertex, or cloud-code-assist".into()),
    }
}

pub(super) fn parse_provider_transport(value: &str) -> Result<ProviderTransport, String> {
    match value {
        "standard" => Ok(ProviderTransport::Standard),
        "kiro" => Ok(ProviderTransport::Kiro),
        "github-copilot" => Ok(ProviderTransport::GithubCopilot),
        _ => Err("provider transport must be standard, kiro, or github-copilot".into()),
    }
}
