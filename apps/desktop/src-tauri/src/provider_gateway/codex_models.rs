//! Explicit, online discovery using a borrowed Codex login. No token is copied
//! into CODETAS settings, logs, model metadata, or a second credential store.
use super::*;
use codetas_gateway::{discover_codex_online_models, ModelDiscoveryError, ModelMetadata};

const MAX_AUTH_BYTES: u64 = 1024 * 1024;

struct Login {
    access: String,
    account: String,
}

fn parse_login(bytes: &[u8]) -> Result<Login, String> {
    let value: JsonValue = serde_json::from_slice(bytes).map_err(|_| {
        "Codexのログイン情報を読めません。Codexで再ログインしてください".to_string()
    })?;
    let field = |name: &str| {
        value
            .get("tokens")
            .and_then(|tokens| tokens.get(name))
            .and_then(JsonValue::as_str)
            .filter(|value| !value.trim().is_empty())
            .map(str::to_string)
    };
    if value
        .get("auth_mode")
        .and_then(JsonValue::as_str)
        .is_some_and(|mode| mode != "chatgpt")
    {
        return Err(
            "ChatGPTログインではありません。CodexでChatGPTアカウントにログインしてください".into(),
        );
    }
    Ok(Login {
        access: field("access_token")
            .ok_or("Codexのアクセストークンがありません。Codexでログインしてください")?,
        account: field("account_id")
            .ok_or("CodexのアカウントIDがありません。Codexで再ログインしてください")?,
    })
}

fn read_login(home: &Path) -> Result<Login, String> {
    // Never fall back to a stale file when Codex uses a different credential
    // backend. Fail explicitly instead of choosing the wrong account.
    let config_path = home.join("config.toml");
    if config_path.exists() {
        let config = fs::read_to_string(config_path)
            .map_err(|_| "Codexの認証保存方式を確認できません".to_string())?
            .parse::<DocumentMut>()
            .map_err(|_| "Codexの設定ファイルが不正です".to_string())?;
        if config
            .get("cli_auth_credentials_store")
            .and_then(Item::as_str)
            .is_some_and(|store| store != "file")
        {
            return Err("このオンライン取得はCodexのファイル保存ログインに対応しています。別の認証保存方式は未対応です（認証設定は変更していません）".into());
        }
    }
    let file = fs::File::open(home.join("auth.json")).map_err(|_| {
        "Codexのログイン情報がありません。Codexでログインしてから再実行してください".to_string()
    })?;
    let mut bytes = Vec::new();
    file.take(MAX_AUTH_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "Codexのログイン情報を読めません".to_string())?;
    if bytes.len() as u64 > MAX_AUTH_BYTES {
        return Err("Codexのログイン情報がサイズ上限を超えています".into());
    }
    parse_login(&bytes)
}

fn parse_version(output: &[u8]) -> Result<String, String> {
    let text = std::str::from_utf8(output).unwrap_or("");
    let version = text.trim().strip_prefix("codex-cli ").unwrap_or("");
    if version.is_empty()
        || version.len() > 80
        || !version
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b".-+".contains(&byte))
    {
        return Err("Codex CLIのバージョンを確認できません".into());
    }
    Ok(version.into())
}

pub(super) async fn fetch_models(
    app: &AppHandle,
    provider: &ProviderDefinition,
    home: &Path,
) -> Result<Vec<ModelMetadata>, String> {
    let executable = super::codex_cli_update::latest_executable(app).await?;
    fetch_models_with_executable(provider, home, executable).await
}

async fn fetch_models_with_executable(
    provider: &ProviderDefinition,
    home: &Path,
    executable: PathBuf,
) -> Result<Vec<ModelMetadata>, String> {
    let output = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::process::Command::new(&executable)
            .arg("--version")
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| "Codex CLIのバージョン確認がタイムアウトしました")?
    .map_err(|_| "Codex CLIを起動できません")?;
    if !output.status.success() {
        return Err("Codex CLIのバージョン確認に失敗しました".into());
    }
    let version = parse_version(&output.stdout)?;
    fetch_with_retry(
        || read_login(home),
        |login| {
            let version = &version;
            async move {
                discover_codex_online_models(provider, &login.access, &login.account, version).await
            }
        },
        || async move {
            let refresh_home = home.to_path_buf();
            // Codex owns refresh-token rotation and persistence.
            tauri::async_runtime::spawn_blocking(move || {
                crate::codex_app_server::refresh_codex_login(&executable, &refresh_home)
            })
            .await
            .map_err(|_| "Codexの認証更新処理に失敗しました".to_string())?
        },
    )
    .await
}

async fn fetch_with_retry<R, F, U, FF, UF>(
    mut read_login: R,
    mut fetch: F,
    refresh: U,
) -> Result<Vec<ModelMetadata>, String>
where
    R: FnMut() -> Result<Login, String>,
    F: FnMut(Login) -> FF,
    U: FnOnce() -> UF,
    FF: std::future::Future<Output = Result<Vec<ModelMetadata>, ModelDiscoveryError>>,
    UF: std::future::Future<Output = Result<(), String>>,
{
    let login = read_login()?;
    let account = login.account.clone();
    let models = match fetch(login).await {
        Ok(models) => models,
        Err(ModelDiscoveryError::Status { status: 401, .. }) => {
            refresh().await?;
            let refreshed = read_login()?;
            ensure_same_account(&account, &refreshed.account)?;
            fetch(refreshed).await.map_err(|error| error.to_string())?
        }
        Err(error) => return Err(error.to_string()),
    };
    // Both the first request and the retry pass the same post-flight check.
    ensure_same_account(&account, &read_login()?.account)?;
    Ok(models)
}

fn ensure_same_account(expected: &str, actual: &str) -> Result<(), String> {
    if expected != actual {
        return Err("取得中にCodexのアカウントが切り替わりました。もう一度実行してください".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_requires_chatgpt_access_and_account_without_leaking_input() {
        let login = parse_login(
            br#"{"auth_mode":"chatgpt","tokens":{"access_token":"secret","account_id":"account"}}"#,
        )
        .unwrap();
        assert_eq!(login.account, "account");
        assert_eq!(login.access, "secret");
        for bad in [
            r#"{"auth_mode":"apikey","tokens":{"access_token":"secret","account_id":"account"}}"#,
            r#"{"tokens":{"access_token":"secret"}}"#,
            r#"{"tokens":{"access_token":"","account_id":"account"}}"#,
            "secret-not-json",
        ] {
            assert!(!parse_login(bad.as_bytes())
                .err()
                .unwrap()
                .contains("secret"));
        }
    }

    #[tokio::test]
    #[ignore = "Uses the local Codex login and the live official model endpoint"]
    async fn online_discovery_live_smoke() {
        let provider = ProviderDefinition {
            id: "openai".into(),
            ..ProviderDefinition::default()
        };
        let models = fetch_models_with_executable(&provider, &codex_home().unwrap(), find_cli_executable("codex").unwrap())
            .await
            .unwrap();
        assert!(!models.is_empty());
        println!(
            "Online models: {}; gpt-6.1-sol present: {}",
            models.len(),
            models.iter().any(|model| model.model_id == "gpt-6.1-sol")
        );
    }

    #[tokio::test]
    async fn postflight_rejects_account_switches_on_first_request_and_retry() {
        use std::{cell::Cell, collections::VecDeque, future::ready};
        for (retry, changed) in [(false, false), (false, true), (true, false), (true, true)] {
            let last = if changed { "new" } else { "original" };
            let mut accounts = VecDeque::from(if retry {
                vec!["original", "original", last]
            } else {
                vec!["original", last]
            });
            let calls = Cell::new(0);
            let refreshes = Cell::new(0);
            let result = fetch_with_retry(
                || {
                    Ok(Login {
                        access: "test".into(),
                        account: accounts.pop_front().unwrap().into(),
                    })
                },
                |_| {
                    calls.set(calls.get() + 1);
                    ready(if retry && calls.get() == 1 {
                        Err(ModelDiscoveryError::Status {
                            status: 401,
                            message: "expired".into(),
                        })
                    } else {
                        Ok(vec![ModelMetadata::default()])
                    })
                },
                || {
                    refreshes.set(refreshes.get() + 1);
                    ready(Ok(()))
                },
            )
            .await;
            assert_eq!(result.is_err(), changed);
            assert_eq!(calls.get(), if retry { 2 } else { 1 });
            assert_eq!(refreshes.get(), usize::from(retry));
            assert!(accounts.is_empty());
        }
    }

    #[tokio::test]
    async fn retries_only_once_and_only_for_unauthorized() {
        use std::{cell::Cell, future::ready};
        for status in [401, 403, 429] {
            let calls = Cell::new(0);
            let refreshes = Cell::new(0);
            let result = fetch_with_retry(
                || {
                    Ok(Login {
                        access: "test".into(),
                        account: "same".into(),
                    })
                },
                |_| {
                    calls.set(calls.get() + 1);
                    ready(Err(ModelDiscoveryError::Status {
                        status,
                        message: "error".into(),
                    }))
                },
                || {
                    refreshes.set(refreshes.get() + 1);
                    ready(Ok(()))
                },
            )
            .await;
            assert!(result.is_err());
            assert_eq!(calls.get(), if status == 401 { 2 } else { 1 });
            assert_eq!(refreshes.get(), usize::from(status == 401));
        }
    }

    #[test]
    fn version_is_validated() {
        assert_eq!(parse_version(b"codex-cli 0.159.1\n").unwrap(), "0.159.1");
        assert!(parse_version(b"unexpected output").is_err());
        assert!(parse_version(b"codex-cli a\r\ninjected").is_err());
    }
}
