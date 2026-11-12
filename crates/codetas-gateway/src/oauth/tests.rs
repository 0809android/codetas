use super::*;
use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use tempfile::tempdir;

#[test]
fn account_scoped_session_keys_resolve_to_the_base_provider() {
    assert_eq!(session_key_provider("anthropic"), "anthropic");
    assert_eq!(session_key_provider("anthropic#0f9a"), "anthropic");
    assert_eq!(session_key_provider("anthropic#"), "anthropic");
}

#[test]
fn relogin_matches_a_stored_account_by_any_shared_identifier() {
    let stored = |account_id: Option<&str>, email: Option<&str>| OAuthStoredAccount {
        store_key: "anthropic#x".into(),
        account_id: account_id.map(str::to_string),
        email: email.map(str::to_string),
        expires_at_ms: 0,
        source: "oauth".into(),
    };
    let session = |account_id: Option<&str>, email: Option<&str>| OAuthSession {
        access: "a".into(),
        refresh: "r".into(),
        expires_at_ms: 0,
        source: "oauth".into(),
        account_id: account_id.map(str::to_string),
        email: email.map(str::to_string),
    };
    assert!(same_oauth_identity(
        &stored(None, Some("User@Example.com")),
        &session(Some("acct-1"), Some("user@example.com"))
    ));
    assert!(same_oauth_identity(
        &stored(Some("acct-1"), None),
        &session(Some("acct-1"), Some("other@example.com"))
    ));
    // Different ids with the same email are different accounts.
    assert!(!same_oauth_identity(
        &stored(Some("acct-1"), Some("a@example.com")),
        &session(Some("acct-2"), Some("a@example.com"))
    ));
    assert!(!same_oauth_identity(
        &stored(Some("acct-1"), Some("a@example.com")),
        &session(Some("acct-2"), Some("b@example.com"))
    ));
    assert!(!same_oauth_identity(
        &stored(None, None),
        &session(None, None)
    ));
}

#[test]
fn account_slugs_stay_unique_for_long_shared_prefixes() {
    let session = |email: String| OAuthSession {
        access: "access".into(),
        refresh: "refresh".into(),
        expires_at_ms: 0,
        source: "oauth".into(),
        account_id: None,
        email: Some(email),
    };
    let prefix = "a".repeat(60);
    let first = session(format!("{prefix}1@example.com"));
    let second = session(format!("{prefix}2@example.com"));
    let first_slug = oauth_account_slug(&first);
    let second_slug = oauth_account_slug(&second);
    assert_ne!(first_slug, second_slug);
    assert!(first_slug.len() <= 48);
    // Same identity must map to the same store key so re-login updates
    // instead of duplicating.
    assert_eq!(first_slug, oauth_account_slug(&first));
}

#[test]
fn parses_antigravity_keyring_payload() {
    let raw = r#"{"auth_method":"consumer","token":{"access_token":"atok","token_type":"Bearer","refresh_token":"rtok","expiry":"2026-08-16T11:05:30+09:00"}}"#;
    let stored = format!("antigravity-oauth:{}", STANDARD.encode(raw));
    let session = parse_antigravity_credentials(&stored).unwrap();
    assert_eq!(session.access, "atok");
    assert_eq!(session.refresh, "rtok");
    assert_eq!(session.expires_at_ms, 1_786_845_930_000);
    assert!(session.email.is_none());
}

#[test]
fn parses_antigravity_signed_in_email() {
    let raw = r#"{"auth_method":"consumer","id_token_email":"User@Example.com","token":{"access_token":"atok","refresh_token":"rtok","expiry":"2026-08-16T11:05:30+09:00"}}"#;
    let session = parse_antigravity_credentials(raw).unwrap();
    assert_eq!(session.email.as_deref(), Some("user@example.com"));
}

#[test]
fn antigravity_supports_native_login_and_cli_import() {
    let descriptor = OAUTH_PROVIDER_REGISTRY
        .iter()
        .find(|item| item.id == "google-antigravity")
        .unwrap();
    assert!(descriptor.native_login);
    assert!(descriptor.cli_import);
    assert_eq!(descriptor.flow, "authorization-code-pkce");
    assert!(provider_supports_native_oauth("google-antigravity"));
}

#[test]
fn antigravity_login_never_replaces_a_different_agy_account() {
    let session = |email: Option<&str>| OAuthSession {
        access: "access".into(),
        refresh: "refresh".into(),
        expires_at_ms: now_ms() + 3_600_000,
        source: "local-cli".into(),
        account_id: None,
        email: email.map(str::to_string),
    };
    assert!(antigravity_identity_conflict(
        &session(Some("one@example.com")),
        &session(Some("two@example.com"))
    ));
    assert!(!antigravity_identity_conflict(
        &session(Some("One@Example.com")),
        &session(Some("one@example.com"))
    ));
    // Without an email on both sides the identity cannot be compared; keep
    // the legacy import behaviour instead of blocking a valid refresh.
    assert!(!antigravity_identity_conflict(
        &session(None),
        &session(Some("two@example.com"))
    ));
    let message = antigravity_other_account_message(
        &session(Some("one@example.com")),
        &session(Some("two@example.com")),
    );
    assert!(message.contains("one@example.com"), "{message}");
    assert!(message.contains("two@example.com"), "{message}");
}

#[test]
fn antigravity_client_file_requires_both_values() {
    let parsed = parse_antigravity_client_file(
        r#"{"clientId":"id.apps.googleusercontent.com","clientSecret":"GOCSPX-test"}"#,
    )
    .unwrap();
    assert_eq!(parsed.0, "id.apps.googleusercontent.com");
    assert_eq!(parsed.1, "GOCSPX-test");
    // Snake case is accepted for hand-written files.
    assert!(
        parse_antigravity_client_file(r#"{"client_id":"id","client_secret":"secret"}"#).is_some()
    );
    // A half-configured file must not enable native login.
    assert!(parse_antigravity_client_file(r#"{"clientId":"id"}"#).is_none());
    assert!(parse_antigravity_client_file("not json").is_none());
}

#[test]
fn parses_rfc3339_offsets_as_utc() {
    assert_eq!(chrono_like_to_ms("1970-01-01T09:00:00+09:00"), Ok(0));
    assert_eq!(chrono_like_to_ms("1970-01-01T00:00:00Z"), Ok(0));
}

#[test]
fn parses_kimi_seconds_expiry() {
    let session = parse_kimi_credentials(
        r#"{"access_token":"atok","refresh_token":"rtok","expires_at":1786151767}"#,
    )
    .unwrap();
    assert_eq!(session.access, "atok");
    assert_eq!(session.refresh, "rtok");
    assert_eq!(session.expires_at_ms, 1_786_151_767_000);
    assert_eq!(session.source, "local-cli");
}

#[test]
fn parses_claude_millis_expiry() {
    let session = parse_claude_credentials(
            r#"{"claudeAiOauth":{"accessToken":"atok","refreshToken":"rtok","expiresAt":1786151767000}}"#,
        )
        .unwrap();
    assert_eq!(session.expires_at_ms, 1_786_151_767_000);
}

#[test]
fn parses_grok_rfc3339_expiry() {
    let session = parse_xai_credentials(
            r#"{"https://auth.x.ai::abc":{"key":"atok","refresh_token":"rtok","expires_at":"2026-08-13T01:02:03Z","user_id":"u1","email":"a@b.c"}}"#,
        )
        .unwrap();
    assert_eq!(session.account_id.as_deref(), Some("u1"));
    assert_eq!(session.email.as_deref(), Some("a@b.c"));
    assert!(session.expires_at_ms > 1_700_000_000_000);
}

#[test]
fn parses_muse_cli_auth_preferring_api_key() {
    let session = parse_muse_credentials(
            r#"{"schema_version":1,"providers":{"meta":{"access_token":"dca:atok","obtained_via":"device_code","mechanism":"oauth","api_key":"LLM|key","api_base_url":"https://api.meta.ai/v1","user_email":"User@Example.com"}}}"#,
        )
        .unwrap();
    assert_eq!(session.access, "LLM|key");
    assert!(session.refresh.is_empty());
    assert_eq!(session.email.as_deref(), Some("user@example.com"));
    assert_eq!(session.source, "local-cli");
    assert!(cli_session_is_importable(&session));
}

#[test]
fn parses_muse_cli_account_token_when_api_key_is_absent() {
    let session = parse_muse_credentials(
        r#"{"providers":{"meta":{"access_token":"dca:atok","user_email":"a@b.c"}}}"#,
    )
    .unwrap();
    assert_eq!(session.access, "dca:atok");
    assert!(cli_session_is_importable(&session));
}

#[test]
fn adopt_imports_muse_and_wires_oauth_without_settings_secret() {
    let directory = tempdir().unwrap();
    let home = directory.path().join("home");
    fs::create_dir_all(home.join(".config/muse")).unwrap();
    fs::write(
            home.join(".config/muse/auth.json"),
            r#"{"schema_version":1,"providers":{"meta":{"api_key":"LLM|imported","access_token":"dca:atok","user_email":"a@b.c"}}}"#,
        )
        .unwrap();
    let store = directory.path().join("auth.json");
    let mut settings = GatewaySettings::default();
    settings.providers.clear();
    assert!(adopt_local_cli_sessions_from(&mut settings, &store, &home).unwrap());
    let provider = settings
        .providers
        .iter()
        .find(|provider| provider.id == "meta")
        .unwrap();
    assert!(provider.enabled);
    assert_eq!(provider.credential.source, CredentialSource::OAuth);
    assert_eq!(provider.credential.reference.as_deref(), Some("meta"));
    let settings_json = serde_json::to_string(&settings).unwrap();
    assert!(!settings_json.contains("LLM|imported"));
    assert!(!settings_json.contains("dca:atok"));
    let stored = load_store(&store).unwrap();
    assert_eq!(stored.providers["meta"].access, "LLM|imported");
}

#[test]
fn parses_qwen_settings_env_key() {
    let session = parse_qwen_cli_key(
            r#"{"env":{"BAILIAN_TOKEN_PLAN_API_KEY":"sk-sp-qwen"},"modelProviders":{"openai":[{"id":"qwen3.8-max","envKey":"BAILIAN_TOKEN_PLAN_API_KEY"}]}}"#,
            &["BAILIAN_TOKEN_PLAN_API_KEY", "DASHSCOPE_API_KEY"],
        )
        .unwrap();
    assert_eq!(session.access, "sk-sp-qwen");
    assert!(cli_session_is_importable(&session));
}

#[test]
fn qwen_token_plan_intl_url_selects_the_intl_preset() {
    let target = parse_qwen_cli_target(
            r#"{"env":{"BAILIAN_TOKEN_PLAN_API_KEY":"sk-sp-qwen"},"model":{"name":"qwen3.8-max","baseUrl":"https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1"},"modelProviders":{"openai":[{"id":"qwen3.8-max","envKey":"BAILIAN_TOKEN_PLAN_API_KEY","baseUrl":"https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1"}]}}"#,
        )
        .unwrap();
    assert_eq!(target.provider_id, "alibaba-token-plan-intl");
    assert_eq!(target.session.access, "sk-sp-qwen");
    assert_eq!(target.base_url, None);
}

#[test]
fn qwen_beijing_coding_plan_overrides_the_intl_coding_preset_url() {
    let target = parse_qwen_cli_target(
            r#"{"env":{"BAILIAN_CODING_PLAN_API_KEY":"sk-sp-cn"},"modelProviders":{"openai":[{"id":"qwen3-coder-plus","envKey":"BAILIAN_CODING_PLAN_API_KEY","baseUrl":"https://coding.dashscope.aliyuncs.com/v1"}]}}"#,
        )
        .unwrap();
    assert_eq!(target.provider_id, "alibaba");
    assert_eq!(
        target.base_url.as_deref(),
        Some("https://coding.dashscope.aliyuncs.com/v1")
    );
}

#[test]
fn qwen_settings_do_not_impersonate_zai_without_zai_key() {
    assert!(parse_qwen_cli_key(
            r#"{"env":{"BAILIAN_TOKEN_PLAN_API_KEY":"sk-sp-qwen"},"modelProviders":{"openai":[{"id":"glm-5.2","envKey":"BAILIAN_TOKEN_PLAN_API_KEY"}]}}"#,
            &["ZAI_API_KEY", "ZHIPU_API_KEY", "GLM_API_KEY"],
        )
        .is_none());
}

#[test]
fn adopt_imports_qwen_settings_into_alibaba_token_plan() {
    let directory = tempdir().unwrap();
    let home = directory.path().join("home");
    fs::create_dir_all(home.join(".qwen")).unwrap();
    fs::write(
            home.join(".qwen/settings.json"),
            r#"{"env":{"BAILIAN_TOKEN_PLAN_API_KEY":"sk-sp-imported"},"modelProviders":{"openai":[{"id":"qwen3.8-max","envKey":"BAILIAN_TOKEN_PLAN_API_KEY"}]}}"#,
        )
        .unwrap();
    let store = directory.path().join("auth.json");
    let mut settings = GatewaySettings::default();
    settings.providers.clear();
    assert!(adopt_local_cli_sessions_from(&mut settings, &store, &home).unwrap());
    let provider = settings
        .providers
        .iter()
        .find(|provider| provider.id == "alibaba-token-plan-intl")
        .unwrap();
    assert!(provider.enabled);
    assert_eq!(provider.credential.source, CredentialSource::OAuth);
    assert_eq!(
        provider.credential.reference.as_deref(),
        Some("alibaba-token-plan-intl")
    );
    let settings_json = serde_json::to_string(&settings).unwrap();
    assert!(!settings_json.contains("sk-sp-imported"));
    assert_eq!(
        load_store(&store).unwrap().providers["alibaba-token-plan-intl"].access,
        "sk-sp-imported"
    );
    assert!(!settings
        .providers
        .iter()
        .any(|provider| provider.id == "zai"));
    assert!(!settings
        .providers
        .iter()
        .any(|provider| provider.id == "minimax"));
}

#[test]
fn adopt_imports_dedicated_zai_and_minimax_cli_files() {
    let directory = tempdir().unwrap();
    let home = directory.path().join("home");
    fs::create_dir_all(home.join(".z.ai")).unwrap();
    fs::create_dir_all(home.join(".minimax")).unwrap();
    fs::write(
        home.join(".z.ai/auth.json"),
        r#"{"api_key":"zai-imported"}"#,
    )
    .unwrap();
    fs::write(
        home.join(".minimax/auth.json"),
        r#"{"api_key":"minimax-imported"}"#,
    )
    .unwrap();
    let store = directory.path().join("auth.json");
    let mut settings = GatewaySettings::default();
    settings.providers.clear();
    assert!(adopt_local_cli_sessions_from(&mut settings, &store, &home).unwrap());
    assert_eq!(
        load_store(&store).unwrap().providers["zai"].access,
        "zai-imported"
    );
    assert_eq!(
        load_store(&store).unwrap().providers["minimax"].access,
        "minimax-imported"
    );
}

#[test]
fn adopt_imports_only_the_mistral_key_from_the_vibe_env_file() {
    let directory = tempdir().unwrap();
    let home = directory.path().join("home");
    fs::create_dir_all(home.join(".vibe")).unwrap();
    fs::write(
            home.join(".vibe/.env"),
            "# MISTRAL_API_KEY=commented-out\nOPENROUTER_API_KEY=other-provider\nexport MISTRAL_API_KEY='vibe-imported'\n",
        )
        .unwrap();
    let store = directory.path().join("auth.json");
    let mut settings = GatewaySettings::default();
    settings.providers.clear();
    assert!(adopt_local_cli_sessions_from(&mut settings, &store, &home).unwrap());
    let session = load_store(&store).unwrap().providers["mistral"].clone();
    assert_eq!(session.access, "vibe-imported");
    assert_eq!(session.source, "local-cli");
    assert!(cli_session_is_importable(&session));
}

#[test]
fn adopt_does_not_bring_back_a_dismissed_login() {
    let directory = tempdir().unwrap();
    let home = directory.path().join("home");
    fs::create_dir_all(home.join(".vibe")).unwrap();
    fs::write(home.join(".vibe/.env"), "MISTRAL_API_KEY=vibe-imported\n").unwrap();
    let store = directory.path().join("auth.json");
    let mut settings = GatewaySettings::default();
    settings.providers.clear();
    settings
        .account_pool
        .dismissed_cli_imports
        .push("mistral".into());
    adopt_local_cli_sessions_from(&mut settings, &store, &home).unwrap();
    assert!(settings.providers.iter().all(|p| p.id != "mistral"));
    assert!(!load_store(&store)
        .unwrap()
        .providers
        .contains_key("mistral"));

    settings.account_pool.dismissed_cli_imports.clear();
    assert!(adopt_local_cli_sessions_from(&mut settings, &store, &home).unwrap());
    assert!(settings
        .providers
        .iter()
        .any(|p| p.id == "mistral" && p.enabled));
}

fn key_session(access: &str) -> OAuthSession {
    OAuthSession {
        access: access.into(),
        refresh: String::new(),
        expires_at_ms: i64::MAX / 2,
        source: "local-cli".into(),
        account_id: None,
        email: None,
    }
}

fn store_session(path: &Path, key: &str, session: OAuthSession) {
    mutate_store(path, |store| {
        store.providers.insert(key.into(), session);
        Ok(())
    })
    .unwrap();
}

fn stored_access(path: &Path, key: &str) -> Option<String> {
    stored_session(path, key)
        .unwrap()
        .map(|session| session.access)
}

fn write_vibe_key(home: &Path, key: &str) {
    fs::create_dir_all(home.join(".vibe")).unwrap();
    fs::write(home.join(".vibe/.env"), format!("MISTRAL_API_KEY={key}\n")).unwrap();
}

fn write_muse_key(home: &Path, key: &str) {
    fs::create_dir_all(home.join(".config/muse")).unwrap();
    fs::write(
        home.join(".config/muse/auth.json"),
        format!(r#"{{"providers":{{"meta":{{"api_key":"{key}"}}}}}}"#),
    )
    .unwrap();
}

fn write_kimi_login(home: &Path) {
    fs::create_dir_all(home.join(".kimi-code/credentials")).unwrap();
    fs::write(
        home.join(".kimi-code/credentials/kimi-code.json"),
        r#"{"access_token":"cli-access","refresh_token":"cli-refresh","expires_at":32503680000}"#,
    )
    .unwrap();
}

fn mistral_connection(
    base_url: Option<&str>,
    credential: ProviderCredential,
) -> ProviderDefinition {
    let mut provider = provider_presets()
        .into_iter()
        .find(|preset| preset.id == "mistral")
        .unwrap()
        .instantiate(base_url)
        .unwrap();
    provider.credential = credential;
    provider
}

fn unset_env_credential() -> ProviderCredential {
    ProviderCredential {
        source: CredentialSource::Environment,
        reference: Some("CODETAS_TEST_UNSET_MISTRAL_KEY".into()),
        ..ProviderCredential::default()
    }
}

#[tokio::test]
async fn a_token_lookup_with_a_stored_session_does_not_read_the_vibe_login() {
    let directory = tempdir().unwrap();
    let home = directory.path().join("home");
    let store = directory.path().join("auth.json");
    store_session(&store, "mistral", key_session("stored-key"));
    write_vibe_key(&home, "vibe-key");
    assert_eq!(
        lookup_access_token(&store, &home, "mistral").await.unwrap(),
        Some("stored-key".into())
    );
    // The Keychain allowance is untouched, so nothing read the login.
    fs::remove_file(home.join(".vibe/.env")).unwrap();
    let mut keychain_read = false;
    detect_vibe_limited(&home, || {
        keychain_read = true;
        None
    });
    assert!(keychain_read);
}

#[tokio::test]
async fn a_token_lookup_fills_a_missing_session_and_keeps_it() {
    let directory = tempdir().unwrap();
    let home = directory.path().join("home");
    let store = directory.path().join("auth.json");
    write_vibe_key(&home, "vibe-key");
    assert_eq!(
        lookup_access_token(&store, &home, "mistral").await.unwrap(),
        Some("vibe-key".into())
    );
    assert_eq!(
        stored_access(&store, "mistral").as_deref(),
        Some("vibe-key")
    );
    write_vibe_key(&home, "newer-key");
    assert_eq!(
        lookup_access_token(&store, &home, "mistral").await.unwrap(),
        Some("vibe-key".into())
    );
}

#[tokio::test]
async fn a_missing_keychain_first_login_is_not_read_again_for_a_minute() {
    let directory = tempdir().unwrap();
    let home = directory.path().join("home");
    let store = directory.path().join("auth.json");
    assert_eq!(
        lookup_access_token(&store, &home, "meta").await.unwrap(),
        None
    );
    // Found by the next read, so only skipping that read leaves it unseen.
    write_muse_key(&home, "LLM|first");
    assert_eq!(
        lookup_access_token(&store, &home, "meta").await.unwrap(),
        None
    );
    assert_eq!(stored_access(&store, "meta"), None);
}

#[tokio::test]
async fn a_vibe_env_key_is_found_as_soon_as_it_is_written() {
    let directory = tempdir().unwrap();
    let home = directory.path().join("home");
    let store = directory.path().join("auth.json");
    assert_eq!(
        lookup_access_token(&store, &home, "mistral").await.unwrap(),
        None
    );
    write_vibe_key(&home, "vibe-key");
    assert_eq!(
        lookup_access_token(&store, &home, "mistral").await.unwrap(),
        Some("vibe-key".into())
    );
}

#[test]
fn only_the_keychain_step_of_the_vibe_login_is_limited() {
    let directory = tempdir().unwrap();
    let home = directory.path().join("home");
    let reads = std::sync::atomic::AtomicUsize::new(0);
    let keychain = || {
        reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        None
    };
    assert!(detect_vibe_limited(&home, keychain).is_none());
    assert!(detect_vibe_limited(&home, keychain).is_none());
    assert_eq!(reads.load(std::sync::atomic::Ordering::SeqCst), 1);
    // The file answers every time, and a rotated key in it shows up at once.
    for key in ["first", "second"] {
        write_vibe_key(&home, key);
        let session = detect_vibe_limited(&home, keychain).unwrap();
        assert_eq!(session.access, key);
    }
    assert_eq!(reads.load(std::sync::atomic::Ordering::SeqCst), 1);
}

/// Waits until a caller is parked on the read in progress.
fn wait_for_a_waiter(key: &CliLoginKey) {
    let started = Instant::now();
    while cli_login_reads()
        .get(key)
        .and_then(|state| state.running.as_ref().map(std::sync::Arc::strong_count))
        != Some(3)
    {
        assert!(started.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn a_request_during_a_read_waits_for_its_answer() {
    let directory = tempdir().unwrap();
    let key = ("meta".to_string(), directory.path().to_path_buf());
    let (started, read_started) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel::<()>();
    let reader = {
        let key = key.clone();
        std::thread::spawn(move || {
            limited_cli_login_read(key, Duration::from_secs(10), move || {
                started.send(()).unwrap();
                released.recv().unwrap();
                Some(key_session("from-the-read"))
            })
        })
    };
    read_started.recv().unwrap();
    let waiter = {
        let key = key.clone();
        std::thread::spawn(move || {
            limited_cli_login_read(key, Duration::from_secs(10), || {
                panic!("a second read while one is in progress")
            })
        })
    };
    wait_for_a_waiter(&key);
    release.send(()).unwrap();
    assert_eq!(reader.join().unwrap().unwrap().access, "from-the-read");
    assert_eq!(waiter.join().unwrap().unwrap().access, "from-the-read");
}

#[test]
fn the_wait_for_a_read_in_progress_is_bounded() {
    let directory = tempdir().unwrap();
    let key = ("meta".to_string(), directory.path().to_path_buf());
    let (started, read_started) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel::<()>();
    let reader = {
        let key = key.clone();
        std::thread::spawn(move || {
            limited_cli_login_read(key, Duration::from_secs(10), move || {
                started.send(()).unwrap();
                released.recv().unwrap();
                None
            })
        })
    };
    read_started.recv().unwrap();
    let waited = Instant::now();
    assert!(
        limited_cli_login_read(key, Duration::from_millis(50), || panic!("a second read"))
            .is_none()
    );
    assert!(waited.elapsed() < Duration::from_secs(5));
    release.send(()).unwrap();
    assert!(reader.join().unwrap().is_none());
}

#[tokio::test]
async fn a_login_that_is_only_a_file_is_not_limited() {
    let directory = tempdir().unwrap();
    let home = directory.path().join("home");
    let store = directory.path().join("auth.json");
    assert_eq!(
        lookup_access_token(&store, &home, "zai").await.unwrap(),
        None
    );
    fs::create_dir_all(home.join(".z.ai")).unwrap();
    fs::write(home.join(".z.ai/auth.json"), r#"{"api_key":"zai-key"}"#).unwrap();
    assert_eq!(
        lookup_access_token(&store, &home, "zai").await.unwrap(),
        Some("zai-key".into())
    );
}

#[tokio::test]
async fn a_meta_login_replaces_the_stored_key_at_most_once_a_minute() {
    let directory = tempdir().unwrap();
    let home = directory.path().join("home");
    let store = directory.path().join("auth.json");
    store_session(&store, "meta", key_session("stored-key"));
    write_muse_key(&home, "LLM|first");
    assert_eq!(
        lookup_access_token(&store, &home, "meta").await.unwrap(),
        Some("LLM|first".into())
    );
    write_muse_key(&home, "LLM|second");
    assert_eq!(
        lookup_access_token(&store, &home, "meta").await.unwrap(),
        Some("LLM|first".into())
    );
}

#[test]
fn a_cli_login_read_before_the_lock_yields_to_what_the_store_holds_now() {
    let directory = tempdir().unwrap();
    let store = directory.path().join("auth.json");
    // Filled while the read waited on a prompt.
    store_session(&store, "mistral", key_session("stored-meanwhile"));
    assert_eq!(
        current_access_token(&store, "mistral", Some(key_session("cli-key"))).unwrap(),
        Some("stored-meanwhile".into())
    );
    // Still empty after the wait.
    assert_eq!(
        current_access_token(&store, "kimi", Some(key_session("cli-key"))).unwrap(),
        Some("cli-key".into())
    );
    // A provider whose CLI key wins takes it either way.
    store_session(&store, "meta", key_session("stored-key"));
    assert_eq!(
        current_access_token(&store, "meta", Some(key_session("cli-key"))).unwrap(),
        Some("cli-key".into())
    );
    // An account key never takes a CLI login.
    store_session(&store, "kimi#b", key_session("account-key"));
    assert_eq!(
        current_access_token(&store, "kimi#b", Some(key_session("cli-key"))).unwrap(),
        Some("account-key".into())
    );
}

#[tokio::test]
async fn recovery_fails_without_a_retry_when_the_cli_key_is_unchanged() {
    let directory = tempdir().unwrap();
    let home = directory.path().join("home");
    let store = directory.path().join("auth.json");
    store_session(&store, "mistral", key_session("same-key"));
    write_vibe_key(&home, "same-key");
    let error = force_refresh_access_token_at(&store, &home, "mistral")
        .await
        .unwrap_err();
    assert!(error.contains("同じ"), "{error}");
    // Reading the file costs nothing, so a key rotated since is picked up.
    write_vibe_key(&home, "new-key");
    assert_eq!(
        force_refresh_access_token_at(&store, &home, "mistral")
            .await
            .unwrap(),
        "new-key"
    );
}

#[tokio::test]
async fn a_repeated_rejection_does_not_read_a_keychain_first_login_again() {
    let directory = tempdir().unwrap();
    let home = directory.path().join("home");
    let store = directory.path().join("auth.json");
    store_session(&store, "meta", key_session("LLM|same"));
    write_muse_key(&home, "LLM|same");
    let error = force_refresh_access_token_at(&store, &home, "meta")
        .await
        .unwrap_err();
    assert!(error.contains("同じ"), "{error}");
    // Found by a second read, so only skipping that read leaves it unseen.
    write_muse_key(&home, "LLM|new");
    assert!(force_refresh_access_token_at(&store, &home, "meta")
        .await
        .is_err());
    assert_eq!(stored_access(&store, "meta").as_deref(), Some("LLM|same"));
}

fn write_zai_key(home: &Path, key: &str) {
    fs::create_dir_all(home.join(".z.ai")).unwrap();
    fs::write(
        home.join(".z.ai/auth.json"),
        format!(r#"{{"api_key":"{key}"}}"#),
    )
    .unwrap();
}

#[tokio::test]
async fn a_second_rejection_right_after_a_recovery_takes_the_new_key_without_reading() {
    for provider in ["mistral", "zai"] {
        let directory = tempdir().unwrap();
        let home = directory.path().join("home");
        let store = directory.path().join("auth.json");
        let write_key = |key: &str| match provider {
            "mistral" => write_vibe_key(&home, key),
            _ => write_zai_key(&home, key),
        };
        store_session(&store, provider, key_session("revoked"));
        write_key("rotated");
        assert_eq!(
            force_refresh_access_token_at(&store, &home, provider)
                .await
                .unwrap(),
            "rotated"
        );
        // Sent before the recovery, so it comes back rejected with the old key.
        write_key("not-read");
        assert_eq!(
            force_refresh_access_token_at(&store, &home, provider)
                .await
                .unwrap(),
            "rotated",
            "{provider}"
        );
        assert_eq!(stored_access(&store, provider).as_deref(), Some("rotated"));
    }
}

#[tokio::test]
async fn a_rejection_right_after_startup_adoption_replaced_the_key_retries_with_it() {
    let directory = tempdir().unwrap();
    let home = directory.path().join("home");
    let store = directory.path().join("auth.json");
    store_session(&store, "zai", key_session("revoked"));
    write_zai_key(&home, "rotated");
    let mut settings = GatewaySettings::default();
    settings.providers.clear();
    adopt_local_cli_sessions_from(&mut settings, &store, &home).unwrap();
    assert_eq!(stored_access(&store, "zai").as_deref(), Some("rotated"));
    // Sent with the old key before the adoption, so it comes back rejected.
    write_zai_key(&home, "not-read");
    assert_eq!(
        force_refresh_access_token_at(&store, &home, "zai")
            .await
            .unwrap(),
        "rotated"
    );
}

#[tokio::test]
async fn a_rejection_after_a_lookup_took_the_new_key_retries_with_it() {
    let directory = tempdir().unwrap();
    let home = directory.path().join("home");
    let store = directory.path().join("auth.json");
    store_session(&store, "zai", key_session("revoked"));
    write_zai_key(&home, "rotated");
    assert_eq!(
        lookup_access_token(&store, &home, "zai").await.unwrap(),
        Some("rotated".into())
    );
    assert_eq!(
        force_refresh_access_token_at(&store, &home, "zai")
            .await
            .unwrap(),
        "rotated"
    );
}

#[tokio::test]
async fn a_rejection_once_the_interval_has_passed_gets_no_retry_for_the_same_key() {
    let directory = tempdir().unwrap();
    let home = directory.path().join("home");
    let store = directory.path().join("auth.json");
    store_session(&store, "zai", key_session("current"));
    write_zai_key(&home, "current");
    cli_key_changes().insert(
        ("zai".to_string(), store.clone()),
        Instant::now() - CLI_LOGIN_READ_INTERVAL,
    );
    let error = force_refresh_access_token_at(&store, &home, "zai")
        .await
        .unwrap_err();
    assert!(error.contains("同じ"), "{error}");
}

fn imported_agy_session() -> OAuthSession {
    OAuthSession {
        refresh: "agy-refresh".into(),
        email: Some("user@example.com".into()),
        ..key_session("agy-access")
    }
}

#[test]
fn a_rejected_agy_login_goes_through_the_cli_once_a_minute() {
    let directory = tempdir().unwrap();
    let home = directory.path().join("home");
    let store = directory.path().join("auth.json");
    let session = imported_agy_session();
    let gate = || gate_agy_refresh_after_rejection(&store, &home, "google-antigravity", &session);
    assert!(gate().is_none());
    assert!(gate().unwrap().unwrap_err().contains("1分"));
    // An account key shares `agy`'s single login, so it shares the limit too.
    assert!(
        gate_agy_refresh_after_rejection(&store, &home, "google-antigravity#b", &session)
            .unwrap()
            .is_err()
    );
}

#[tokio::test]
async fn a_repeated_rejection_of_an_agy_login_fails_before_reaching_the_cli() {
    let directory = tempdir().unwrap();
    let home = directory.path().join("home");
    let store = directory.path().join("auth.json");
    store_session(&store, "google-antigravity", imported_agy_session());
    assert!(take_cli_login_read((
        "google-antigravity".to_string(),
        home.clone()
    )));
    let error = force_refresh_access_token_at(&store, &home, "google-antigravity")
        .await
        .unwrap_err();
    assert!(error.contains("1分"), "{error}");
}

#[tokio::test]
async fn a_second_rejection_of_an_agy_login_takes_the_token_just_refreshed() {
    let directory = tempdir().unwrap();
    let home = directory.path().join("home");
    let store = directory.path().join("auth.json");
    store_session(&store, "google-antigravity", imported_agy_session());
    note_cli_key_changed(&store, "google-antigravity");
    assert_eq!(
        force_refresh_access_token_at(&store, &home, "google-antigravity")
            .await
            .unwrap(),
        "agy-access"
    );
}

#[tokio::test]
async fn an_agy_refresh_in_progress_does_not_hold_up_other_providers() {
    let directory = tempdir().unwrap();
    let home = directory.path().join("home");
    let store = directory.path().join("auth.json");
    store_session(&store, "anthropic", key_session("anthropic-access"));
    store_session(&store, "google-antigravity", imported_agy_session());
    let _agy = auth_refresh_lock_for("google-antigravity").lock().await;
    // No refresh token, so reaching the session fails before any network call.
    let other = tokio::time::timeout(
        Duration::from_secs(5),
        force_refresh_access_token_at(&store, &home, "anthropic"),
    )
    .await
    .expect("another provider's recovery waited for agy");
    assert!(other.unwrap_err().contains("refresh token"));
    // Another Antigravity recovery does wait, so `agy models` runs one at a time.
    assert!(tokio::time::timeout(
        Duration::from_millis(100),
        force_refresh_access_token_at(&store, &home, "google-antigravity"),
    )
    .await
    .is_err());
}

#[tokio::test]
async fn recovery_stores_a_new_cli_key_for_the_retry() {
    let directory = tempdir().unwrap();
    let home = directory.path().join("home");
    let store = directory.path().join("auth.json");
    store_session(&store, "mistral", key_session("rejected-key"));
    write_vibe_key(&home, "new-key");
    assert_eq!(
        force_refresh_access_token_at(&store, &home, "mistral")
            .await
            .unwrap(),
        "new-key"
    );
    assert_eq!(stored_access(&store, "mistral").as_deref(), Some("new-key"));
}

#[tokio::test]
async fn recovery_uses_the_session_of_the_key_the_request_used() {
    let directory = tempdir().unwrap();
    let home = directory.path().join("home");
    let store = directory.path().join("auth.json");
    let mut base = key_session("base-access");
    base.refresh = "base-refresh".into();
    store_session(&store, "kimi", base);
    // No refresh token, so reaching it fails before any network call.
    store_session(&store, "kimi#b", key_session("account-access"));
    let error = force_refresh_access_token_at(&store, &home, "kimi#b")
        .await
        .unwrap_err();
    assert!(error.contains("kimi#b"), "{error}");
    assert_eq!(
        stored_access(&store, "kimi").as_deref(),
        Some("base-access")
    );
}

#[tokio::test]
async fn recovery_with_no_session_does_not_import_the_cli_login() {
    let directory = tempdir().unwrap();
    let home = directory.path().join("home");
    let store = directory.path().join("auth.json");
    write_kimi_login(&home);
    store_session(&store, "kimi#b", key_session("account-access"));
    let error = force_refresh_access_token_at(&store, &home, "kimi")
        .await
        .unwrap_err();
    assert!(error.contains("見つかりません"), "{error}");
    assert_eq!(stored_access(&store, "kimi"), None);
}

#[tokio::test]
async fn recovery_of_a_static_account_key_does_not_take_the_cli_login() {
    let directory = tempdir().unwrap();
    let home = directory.path().join("home");
    let store = directory.path().join("auth.json");
    write_vibe_key(&home, "vibe-key");
    store_session(&store, "mistral#work", key_session("work-key"));
    let error = force_refresh_access_token_at(&store, &home, "mistral#work")
        .await
        .unwrap_err();
    assert!(error.contains("CLIログインから更新できません"), "{error}");
    assert_eq!(
        stored_access(&store, "mistral#work").as_deref(),
        Some("work-key")
    );
}

#[test]
fn recovery_targets_the_login_the_request_used() {
    let provider = ProviderDefinition {
        id: "anthropic".into(),
        credential: oauth_session_credential("anthropic"),
        ..ProviderDefinition::default()
    };
    let account = oauth_session_credential("anthropic#b");
    assert_eq!(request_login_key(&provider, Some(&account)), "anthropic#b");
    assert_eq!(request_login_key(&provider, None), "anthropic");
    let unnamed = ProviderCredential::default();
    assert_eq!(request_login_key(&provider, Some(&unnamed)), "anthropic");
}

#[test]
fn only_a_mistral_connection_elsewhere_refuses_a_cli_login() {
    let official = mistral_connection(None, ProviderCredential::default());
    assert!(provider_may_take_cli_login(&official));
    assert!(registration_may_enable_provider(&official));
    let proxy = mistral_connection(
        Some("https://proxy.example/v1"),
        ProviderCredential::default(),
    );
    assert!(!provider_may_take_cli_login(&proxy));
    assert!(!registration_may_enable_provider(&proxy));
    let keyed_proxy = mistral_connection(
        Some("https://proxy.example/v1"),
        ProviderCredential {
            source: CredentialSource::Keychain,
            reference: Some("api-key/mistral".into()),
            ..ProviderCredential::default()
        },
    );
    assert!(!provider_may_take_cli_login(&keyed_proxy));
    assert!(registration_may_enable_provider(&keyed_proxy));
    let other = ProviderDefinition {
        id: "kimi".into(),
        base_url: "https://proxy.example/v1".into(),
        ..ProviderDefinition::default()
    };
    assert!(provider_may_take_cli_login(&other));
}

#[test]
fn adopt_never_attaches_the_vibe_key_to_a_proxy_connection() {
    for (credential, enabled, has_session) in [
        (ProviderCredential::default(), true, false),
        (ProviderCredential::default(), false, false),
        (unset_env_credential(), true, false),
        (unset_env_credential(), false, false),
        (ProviderCredential::default(), false, true),
        (unset_env_credential(), true, true),
    ] {
        let directory = tempdir().unwrap();
        let home = directory.path().join("home");
        let store = directory.path().join("auth.json");
        write_vibe_key(&home, "vibe-key");
        if has_session {
            store_session(&store, "mistral", key_session("stored-key"));
        }
        let mut provider = mistral_connection(Some("https://proxy.example/v1"), credential.clone());
        provider.enabled = enabled;
        let mut settings = GatewaySettings::default();
        settings.providers.clear();
        settings.providers.push(provider);
        assert!(!adopt_local_cli_sessions_from(&mut settings, &store, &home).unwrap());
        let provider = &settings.providers[0];
        assert_eq!(provider.enabled, enabled);
        assert_eq!(provider.credential.source, credential.source);
        assert_eq!(provider.credential.reference, credential.reference);
        assert_eq!(
            stored_access(&store, "mistral"),
            has_session.then(|| "stored-key".to_string())
        );
    }
}

#[test]
fn adopt_still_wires_a_mistral_connection_on_the_official_host() {
    let directory = tempdir().unwrap();
    let home = directory.path().join("home");
    let store = directory.path().join("auth.json");
    write_vibe_key(&home, "vibe-key");
    let mut provider = mistral_connection(None, unset_env_credential());
    provider.enabled = false;
    let mut settings = GatewaySettings::default();
    settings.providers.clear();
    settings.providers.push(provider);
    assert!(adopt_local_cli_sessions_from(&mut settings, &store, &home).unwrap());
    assert!(settings.providers[0].enabled);
    assert_eq!(
        settings.providers[0].credential.source,
        CredentialSource::OAuth
    );
    assert_eq!(
        stored_access(&store, "mistral").as_deref(),
        Some("vibe-key")
    );
}

#[test]
fn adopt_keeps_no_vibe_key_for_a_connection_with_its_own_key() {
    let directory = tempdir().unwrap();
    let home = directory.path().join("home");
    let store = directory.path().join("auth.json");
    write_vibe_key(&home, "vibe-key");
    let keychain = ProviderCredential {
        source: CredentialSource::Keychain,
        reference: Some("api-key/mistral".into()),
        ..ProviderCredential::default()
    };
    let mut settings = GatewaySettings::default();
    settings.providers.clear();
    settings
        .providers
        .push(mistral_connection(None, keychain.clone()));
    assert!(!adopt_local_cli_sessions_from(&mut settings, &store, &home).unwrap());
    assert_eq!(settings.providers[0].credential.source, keychain.source);
    assert_eq!(stored_access(&store, "mistral"), None);
}

#[test]
fn adopting_twice_reads_a_missing_keychain_first_login_once() {
    let directory = tempdir().unwrap();
    let home = directory.path().join("home");
    let store = directory.path().join("auth.json");
    let mut settings = GatewaySettings::default();
    settings.providers.clear();
    assert!(!adopt_local_cli_sessions_from(&mut settings, &store, &home).unwrap());
    // Found by a second read, so only skipping that read leaves it unseen.
    write_muse_key(&home, "LLM|first");
    assert!(!adopt_local_cli_sessions_from(&mut settings, &store, &home).unwrap());
    assert_eq!(stored_access(&store, "meta"), None);
}

/// One login each, and the connection's id, for a family adoption treats its own way.
fn write_cli_login(home: &Path, provider: &str) {
    match provider {
        "kimi" => write_kimi_login(home),
        "meta" => write_muse_key(home, "LLM|key"),
        "zai" => write_zai_key(home, "zai-key"),
        "minimax" => {
            fs::create_dir_all(home.join(".minimax")).unwrap();
            fs::write(home.join(".minimax/auth.json"), r#"{"api_key":"mm-key"}"#).unwrap();
        }
        "alibaba-token-plan-intl" => {
            fs::create_dir_all(home.join(".qwen")).unwrap();
            fs::write(
                home.join(".qwen/settings.json"),
                r#"{"env":{"DASHSCOPE_API_KEY":"qwen-key"}}"#,
            )
            .unwrap();
        }
        "mistral" => write_vibe_key(home, "vibe-key"),
        other => panic!("no login for {other}"),
    }
}

fn write_deepai_models(home: &Path) {
    let directory = home.join(".deepai-code/userdata/deepai/pi/deepai");
    fs::create_dir_all(&directory).unwrap();
    fs::write(
            directory.join("models.json"),
            r#"{"providers":{"deepai":{"baseUrl":"http://127.0.0.1:60011","apiKey":"deepai-key","models":[{"id":"kimi-k3","name":"Kimi K3","input":["text","image"],"contextWindow":1048576,"maxTokens":131072}]}}}"#,
        )
        .unwrap();
}

#[test]
fn deepai_runtime_endpoint_reports_the_port_the_app_is_serving() {
    let home = tempdir().unwrap();
    assert_eq!(deepai_runtime_endpoint(home.path()), None);
    write_deepai_models(home.path());
    assert_eq!(
        deepai_runtime_endpoint(home.path()).as_deref(),
        Some("http://127.0.0.1:60011")
    );
}

#[test]
fn deepai_runtime_endpoint_follows_a_restart_and_ignores_a_remote_url() {
    let home = tempdir().unwrap();
    write_deepai_models(home.path());
    let path = home
        .path()
        .join(".deepai-code/userdata/deepai/pi/deepai/models.json");
    // A restart rewrites the same file on a new port.
    fs::write(
            &path,
            r#"{"providers":{"deepai":{"baseUrl":"http://127.0.0.1:52003","apiKey":"deepai-key","models":[{"id":"kimi-k3"}]}}}"#,
        )
        .unwrap();
    assert_eq!(
        deepai_runtime_endpoint(home.path()).as_deref(),
        Some("http://127.0.0.1:52003")
    );
    // A URL outside loopback is refused rather than followed.
    fs::write(
            &path,
            r#"{"providers":{"deepai":{"baseUrl":"https://example.invalid","apiKey":"deepai-key","models":[{"id":"kimi-k3"}]}}}"#,
        )
        .unwrap();
    assert_eq!(deepai_runtime_endpoint(home.path()), None);
}

const ADOPTION_FAMILIES: [&str; 6] = [
    "kimi",
    "meta",
    "zai",
    "minimax",
    "alibaba-token-plan-intl",
    "mistral",
];

fn preset_connection(id: &str) -> ProviderDefinition {
    provider_presets()
        .into_iter()
        .find(|preset| preset.id == id)
        .unwrap_or_else(|| panic!("no preset for {id}"))
        .instantiate(None)
        .unwrap()
}

#[test]
fn adoption_leaves_a_connection_the_user_switched_off_switched_off() {
    for provider in ADOPTION_FAMILIES {
        let directory = tempdir().unwrap();
        let home = directory.path().join("home");
        let store = directory.path().join("auth.json");
        write_cli_login(&home, provider);
        let mut connection = preset_connection(provider);
        connection.credential = oauth_session_credential(provider);
        connection.enabled = false;
        let mut settings = GatewaySettings::default();
        settings.providers = vec![connection];
        assert!(
            !adopt_local_cli_sessions_from(&mut settings, &store, &home).unwrap(),
            "{provider}"
        );
        assert!(!settings.providers[0].enabled, "{provider}");
        // The login itself is still kept, ready for when it is switched back on.
        assert!(stored_access(&store, provider).is_some(), "{provider}");
    }
}

#[test]
fn a_first_import_still_turns_on_a_new_connection_and_an_unwired_stub() {
    for provider in ADOPTION_FAMILIES {
        let directory = tempdir().unwrap();
        let home = directory.path().join("home");
        write_cli_login(&home, provider);
        let store = directory.path().join("new.json");
        let mut settings = GatewaySettings::default();
        settings.providers.clear();
        assert!(adopt_local_cli_sessions_from(&mut settings, &store, &home).unwrap());
        let created = settings.providers.iter().find(|p| p.id == provider);
        assert!(created.is_some_and(|p| p.enabled), "{provider}");

        // A read of a Keychain-first login was used up above, so use another home.
        let directory = tempdir().unwrap();
        let home = directory.path().join("home");
        write_cli_login(&home, provider);
        let store = directory.path().join("stub.json");
        let mut stub = preset_connection(provider);
        stub.enabled = false;
        stub.credential = unset_env_credential();
        let mut settings = GatewaySettings::default();
        settings.providers = vec![stub];
        assert!(adopt_local_cli_sessions_from(&mut settings, &store, &home).unwrap());
        assert!(settings.providers[0].enabled, "{provider}");
        assert_eq!(
            settings.providers[0].credential.source,
            CredentialSource::OAuth,
            "{provider}"
        );
    }
}

#[test]
fn deepai_runtime_points_the_connection_at_the_running_app() {
    let directory = tempdir().unwrap();
    let store = directory.path().join("auth.json");
    let runtime = DeepAiRuntime {
        session: OAuthSession {
            access: "deepai-key".into(),
            refresh: String::new(),
            expires_at_ms: i64::MAX / 2,
            source: "local-cli".into(),
            account_id: None,
            email: None,
        },
        base_url: Some("http://127.0.0.1:61234".into()),
        models: vec![
            DeepAiModel {
                id: "kimi-k3".into(),
                context_window: Some(1_048_576),
                max_output: Some(131_072),
                modalities: vec!["text".into(), "image".into()],
                responses_api: false,
                reasoning: false,
            },
            DeepAiModel {
                id: "gpt-6-astra".into(),
                context_window: Some(1_048_576),
                max_output: Some(128_000),
                modalities: vec!["text".into()],
                responses_api: true,
                reasoning: false,
            },
        ],
    };
    let mut settings = GatewaySettings::default();
    settings.providers.clear();
    assert!(apply_deepai_runtime_from(&mut settings, &store, &runtime).unwrap());
    settings.validate().unwrap();
    let provider = settings
        .providers
        .iter()
        .find(|provider| provider.id == "deepai")
        .unwrap();
    assert!(provider.enabled);
    assert!(provider.allow_private_network);
    assert_eq!(provider.base_url, "http://127.0.0.1:61234");
    assert_eq!(provider.models, vec!["kimi-k3", "gpt-6-astra"]);
    assert_eq!(provider.default_model.as_deref(), Some("kimi-k3"));
    assert_eq!(
        provider.model_context_windows.get("kimi-k3"),
        Some(&1_048_576)
    );
    assert_eq!(
        provider.model_max_output_tokens.get("kimi-k3"),
        Some(&131_072)
    );
    assert_eq!(
        provider
            .model_input_modalities
            .get("kimi-k3")
            .map(|values| values.iter().map(String::as_str).collect::<Vec<_>>()),
        Some(vec!["text", "image"])
    );
    assert_eq!(
        provider.model_protocols.get("gpt-6-astra"),
        Some(&crate::config::ProviderProtocol::Responses)
    );
    assert!(!provider.model_protocols.contains_key("kimi-k3"));
    assert_eq!(provider.credential.source, CredentialSource::OAuth);
    assert_eq!(provider.credential.reference.as_deref(), Some("deepai"));
    assert_eq!(
        stored_access(&store, "deepai").as_deref(),
        Some("deepai-key")
    );
    assert!(!apply_deepai_runtime_from(&mut settings, &store, &runtime).unwrap());
}

fn deepai_runtime_for(base_url: &str, models: Vec<DeepAiModel>) -> DeepAiRuntime {
    DeepAiRuntime {
        session: OAuthSession {
            access: "deepai-key".into(),
            refresh: String::new(),
            expires_at_ms: i64::MAX / 2,
            source: "local-cli".into(),
            account_id: None,
            email: None,
        },
        base_url: Some(base_url.into()),
        models,
    }
}

fn deepai_model(id: &str) -> DeepAiModel {
    DeepAiModel {
        id: id.into(),
        context_window: Some(1_000),
        max_output: Some(100),
        modalities: vec!["text".into()],
        responses_api: false,
        reasoning: false,
    }
}

#[test]
fn deepai_reasoning_models_get_effort_levels_and_dropped_models_lose_them() {
    let directory = tempdir().unwrap();
    let store = directory.path().join("auth.json");
    let mut thinking = deepai_model("thinker");
    thinking.reasoning = true;
    let runtime = deepai_runtime_for(
        "http://127.0.0.1:60011",
        vec![thinking.clone(), deepai_model("plain")],
    );
    let mut settings = GatewaySettings::default();
    settings.providers.clear();
    assert!(apply_deepai_runtime_from(&mut settings, &store, &runtime).unwrap());
    let provider = settings
        .providers
        .iter()
        .find(|p| p.id == "deepai")
        .unwrap();
    assert_eq!(
        provider.model_reasoning_efforts.get("thinker").unwrap(),
        &vec!["low".to_string(), "medium".into(), "high".into()]
    );
    assert_eq!(
        provider
            .model_default_reasoning_efforts
            .get("thinker")
            .map(String::as_str),
        Some("medium")
    );
    assert!(!provider.model_reasoning_efforts.contains_key("plain"));

    let runtime = deepai_runtime_for("http://127.0.0.1:60011", vec![deepai_model("plain")]);
    assert!(apply_deepai_runtime_from(&mut settings, &store, &runtime).unwrap());
    let provider = settings
        .providers
        .iter()
        .find(|p| p.id == "deepai")
        .unwrap();
    assert!(provider.model_reasoning_efforts.is_empty());
    assert!(provider.model_default_reasoning_efforts.is_empty());
}

#[test]
fn deepai_runtime_respects_a_dismissed_import() {
    let directory = tempdir().unwrap();
    let store = directory.path().join("auth.json");
    let runtime = deepai_runtime_for("http://127.0.0.1:60011", vec![deepai_model("kimi-k3")]);
    let mut settings = GatewaySettings::default();
    settings.providers.clear();
    dismiss_cli_import(&mut settings.account_pool, "deepai");
    assert!(!apply_deepai_runtime_from(&mut settings, &store, &runtime).unwrap());
    assert!(settings
        .providers
        .iter()
        .all(|provider| provider.id != "deepai"));
    assert!(stored_access(&store, "deepai").is_none());
    // Registering again clears the dismissal, so an explicit import works.
    restore_cli_import(&mut settings.account_pool, "deepai");
    assert!(apply_deepai_runtime_from(&mut settings, &store, &runtime).unwrap());
    assert!(settings
        .providers
        .iter()
        .any(|provider| provider.id == "deepai"));
}

#[test]
fn deepai_runtime_drops_metadata_for_models_that_left() {
    let directory = tempdir().unwrap();
    let store = directory.path().join("auth.json");
    let mut settings = GatewaySettings::default();
    settings.providers.clear();
    apply_deepai_runtime_from(
        &mut settings,
        &store,
        &deepai_runtime_for(
            "http://127.0.0.1:60011",
            vec![deepai_model("a"), deepai_model("b")],
        ),
    )
    .unwrap();
    {
        let provider = settings
            .providers
            .iter_mut()
            .find(|provider| provider.id == "deepai")
            .unwrap();
        provider.default_model = Some("b".into());
    }
    assert!(apply_deepai_runtime_from(
        &mut settings,
        &store,
        &deepai_runtime_for(
            "http://127.0.0.1:60012",
            vec![deepai_model("b"), deepai_model("c")],
        ),
    )
    .unwrap());
    let provider = settings
        .providers
        .iter()
        .find(|provider| provider.id == "deepai")
        .unwrap();
    assert_eq!(provider.base_url, "http://127.0.0.1:60012");
    assert_eq!(provider.models, vec!["b", "c"]);
    // A chosen default that is still offered is kept.
    assert_eq!(provider.default_model.as_deref(), Some("b"));
    assert!(!provider.model_context_windows.contains_key("a"));
    assert!(provider.model_context_windows.contains_key("c"));
    settings.validate().unwrap();
}

#[test]
fn deepai_runtime_clears_stale_limits_when_the_context_can_shrink() {
    let directory = tempdir().unwrap();
    let store = directory.path().join("auth.json");
    let mut settings = GatewaySettings::default();
    settings.providers.clear();
    let mut first = deepai_model("m");
    first.context_window = Some(1_000);
    first.max_output = Some(500);
    apply_deepai_runtime_from(
        &mut settings,
        &store,
        &deepai_runtime_for("http://127.0.0.1:60011", vec![first]),
    )
    .unwrap();
    settings.validate().unwrap();

    // The context shrinks and the file no longer offers a usable max output.
    let mut second = deepai_model("m");
    second.context_window = Some(400);
    second.max_output = None;
    apply_deepai_runtime_from(
        &mut settings,
        &store,
        &deepai_runtime_for("http://127.0.0.1:60011", vec![second]),
    )
    .unwrap();
    let provider = settings
        .providers
        .iter()
        .find(|provider| provider.id == "deepai")
        .unwrap();
    assert_eq!(provider.model_context_windows.get("m"), Some(&400));
    assert!(!provider.model_max_output_tokens.contains_key("m"));
    settings.validate().unwrap();

    // The context disappears entirely; the max output must not outlive it.
    let mut third = deepai_model("m");
    third.context_window = None;
    third.max_output = Some(2_000);
    apply_deepai_runtime_from(
        &mut settings,
        &store,
        &deepai_runtime_for("http://127.0.0.1:60011", vec![third]),
    )
    .unwrap();
    let provider = settings
        .providers
        .iter()
        .find(|provider| provider.id == "deepai")
        .unwrap();
    assert!(!provider.model_context_windows.contains_key("m"));
    assert_eq!(provider.model_max_output_tokens.get("m"), Some(&2_000));
    settings.validate().unwrap();
}

#[test]
fn the_stored_deepai_session_records_its_endpoint() {
    let directory = tempdir().unwrap();
    let store = directory.path().join("auth.json");
    let mut settings = GatewaySettings::default();
    settings.providers.clear();
    apply_deepai_runtime_from(
        &mut settings,
        &store,
        &deepai_runtime_for("http://127.0.0.1:60001", vec![deepai_model("m")]),
    )
    .unwrap();
    assert_eq!(
        stored_session(&store, "deepai")
            .unwrap()
            .unwrap()
            .account_id
            .as_deref(),
        Some("http://127.0.0.1:60001")
    );
    apply_deepai_runtime_from(
        &mut settings,
        &store,
        &deepai_runtime_for("http://127.0.0.1:60002", vec![deepai_model("m")]),
    )
    .unwrap();
    assert_eq!(
        stored_session(&store, "deepai")
            .unwrap()
            .unwrap()
            .account_id
            .as_deref(),
        Some("http://127.0.0.1:60002")
    );
}

#[test]
fn deepai_runtime_reads_the_runtime_file_from_the_home() {
    let directory = tempdir().unwrap();
    let home = directory.path().join("home");
    let store = directory.path().join("auth.json");
    write_deepai_models(&home);
    let mut settings = GatewaySettings::default();
    settings.providers.clear();
    assert!(apply_deepai_runtime_at(&mut settings, &store, &home).unwrap());
    let provider = settings
        .providers
        .iter()
        .find(|provider| provider.id == "deepai")
        .unwrap();
    assert_eq!(provider.models, vec!["kimi-k3"]);
    assert_eq!(provider.base_url, "http://127.0.0.1:60011");
    // A home without a runtime config leaves everything untouched.
    let empty = directory.path().join("empty-home");
    assert!(!apply_deepai_runtime_at(&mut settings, &store, &empty).unwrap());
}

#[test]
fn a_runtime_without_a_loopback_endpoint_is_not_present() {
    let directory = tempdir().unwrap();
    let home = directory.path().join("home");
    let runtime = home.join(".deepai-code/userdata/deepai/pi/deepai");
    fs::create_dir_all(&runtime).unwrap();
    let write = |body: &str| fs::write(runtime.join("models.json"), body).unwrap();
    // A base URL that is present but outside loopback is refused at parse time.
    write(r#"{"providers":{"deepai":{"apiKey":"k","baseUrl":"http://example.com:60011"}}}"#);
    assert!(!deepai_runtime_present(&home));
    // A missing base URL parses as a runtime but is not usable.
    write(r#"{"providers":{"deepai":{"apiKey":"k","models":[{"id":"m"}]}}}"#);
    assert!(detect_deepai_runtime(&home).is_some());
    assert!(!deepai_runtime_present(&home));
}

#[test]
fn deepai_runtime_clears_models_when_the_app_reports_none() {
    let directory = tempdir().unwrap();
    let store = directory.path().join("auth.json");
    let mut settings = GatewaySettings::default();
    settings.providers.clear();
    apply_deepai_runtime_from(
        &mut settings,
        &store,
        &deepai_runtime_for("http://127.0.0.1:60011", vec![deepai_model("m")]),
    )
    .unwrap();
    assert!(apply_deepai_runtime_from(
        &mut settings,
        &store,
        &deepai_runtime_for("http://127.0.0.1:60011", Vec::new()),
    )
    .unwrap());
    let provider = settings
        .providers
        .iter()
        .find(|provider| provider.id == "deepai")
        .unwrap();
    assert!(provider.models.is_empty());
    assert!(provider.model_context_windows.is_empty());
    assert!(provider.model_max_output_tokens.is_empty());
    assert!(provider.model_input_modalities.is_empty());
    assert!(provider.model_protocols.is_empty());
    assert_eq!(provider.default_model, None);
    settings.validate().unwrap();
}

#[test]
fn adopting_twice_reads_a_meta_login_once() {
    let directory = tempdir().unwrap();
    let home = directory.path().join("home");
    let store = directory.path().join("auth.json");
    write_muse_key(&home, "LLM|first");
    let mut settings = GatewaySettings::default();
    settings.providers.clear();
    assert!(adopt_local_cli_sessions_from(&mut settings, &store, &home).unwrap());
    write_muse_key(&home, "LLM|second");
    adopt_local_cli_sessions_from(&mut settings, &store, &home).unwrap();
    assert_eq!(stored_access(&store, "meta").as_deref(), Some("LLM|first"));
}

#[tokio::test]
async fn registering_keeps_the_login_it_read_whatever_the_read_limit_says() {
    let directory = tempdir().unwrap();
    let home = directory.path().join("home");
    let store = directory.path().join("auth.json");
    // A recent automatic read does not hold back a person asking for this.
    assert!(detect_local_cli_session_limited("meta", &home).is_none());
    assert!(!import_local_cli_session_at(&store, &home, "meta").await);
    write_muse_key(&home, "LLM|first");
    assert!(detect_local_cli_session_limited("meta", &home).is_none());
    assert!(import_local_cli_session_at(&store, &home, "meta").await);
    assert_eq!(stored_access(&store, "meta").as_deref(), Some("LLM|first"));
    // Kept, so the first request has nothing to read.
    fs::remove_file(home.join(".config/muse/auth.json")).unwrap();
    assert!(import_local_cli_session_at(&store, &home, "meta").await);
}

#[test]
fn dismissing_under_an_alias_covers_the_canonical_provider() {
    let mut pool = AccountPoolSettings::default();
    dismiss_cli_import(&mut pool, "kimi-code");
    assert_eq!(pool.dismissed_cli_imports, ["kimi"]);
    assert!(cli_import_is_dismissed(&pool, "kimi"));
    // Recording it twice under either name keeps one entry.
    dismiss_cli_import(&mut pool, "kimi");
    assert_eq!(pool.dismissed_cli_imports.len(), 1);
    // An entry saved under the alias by an earlier build still counts.
    let mut saved = AccountPoolSettings::default();
    saved.dismissed_cli_imports.push("minimax-cn".into());
    assert!(cli_import_is_dismissed(&saved, "minimax"));
    restore_cli_import(&mut saved, "minimax");
    assert!(saved.dismissed_cli_imports.is_empty());
}

#[test]
fn vibe_env_file_ignores_blank_and_missing_keys() {
    assert_eq!(parse_vibe_dotenv_key("MISTRAL_API_KEY=\n"), None);
    assert_eq!(parse_vibe_dotenv_key("MISTRAL_API_KEY=''\n"), None);
    assert_eq!(parse_vibe_dotenv_key("OTHER=value\n"), None);
    assert_eq!(
        parse_vibe_dotenv_key("MISTRAL_API_KEY=\"quoted\"\n").as_deref(),
        Some("quoted")
    );
    for line in [
        "MISTRAL_API_KEY=plain # note",
        "MISTRAL_API_KEY='plain' # note",
        "MISTRAL_API_KEY=\"plain\"",
    ] {
        assert_eq!(parse_vibe_dotenv_key(line).as_deref(), Some("plain"));
    }
    assert_eq!(
        parse_vibe_dotenv_key("MISTRAL_API_KEY=has#hash").as_deref(),
        Some("has#hash")
    );
    // Vibe drops these lines, so importing them would differ from what it uses.
    assert_eq!(parse_vibe_dotenv_key("MISTRAL_API_KEY='open"), None);
    assert_eq!(parse_vibe_dotenv_key("MISTRAL_API_KEY='a' b"), None);
}

#[test]
fn vibe_env_file_uses_the_last_assignment_like_python_dotenv() {
    let parsed = |raw: &str| parse_vibe_dotenv_key(raw);
    assert_eq!(
        parsed("MISTRAL_API_KEY=old\nOTHER=x\nMISTRAL_API_KEY=new\n").as_deref(),
        Some("new")
    );
    // An empty assignment still replaces the earlier one.
    assert_eq!(parsed("MISTRAL_API_KEY=old\nMISTRAL_API_KEY=\n"), None);
    // A line it cannot parse assigns nothing.
    assert_eq!(
        parsed("MISTRAL_API_KEY=good\nMISTRAL_API_KEY='open\n").as_deref(),
        Some("good")
    );
}

#[test]
fn auth_store_roundtrip_keeps_sessions_private_to_path() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("auth.json");
    let mut store = AuthStoreFile {
        version: AUTH_STORE_VERSION,
        providers: BTreeMap::new(),
    };
    store.providers.insert(
        "kimi".into(),
        OAuthSession {
            access: "atok".into(),
            refresh: "rtok".into(),
            expires_at_ms: 1_786_151_767_000,
            source: "local-cli".into(),
            account_id: None,
            email: None,
        },
    );
    save_store(&path, &store).unwrap();
    store.providers.insert(
        "anthropic".into(),
        OAuthSession {
            access: "atok2".into(),
            refresh: "rtok2".into(),
            expires_at_ms: 1_786_151_767_000,
            source: "oauth".into(),
            account_id: None,
            email: None,
        },
    );
    save_store(&path, &store).unwrap();
    let loaded = load_store(&path).unwrap();
    assert_eq!(loaded.providers["kimi"].refresh, "rtok");
    assert_eq!(loaded.providers["anthropic"].refresh, "rtok2");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[test]
fn adopt_imports_kimi_and_wires_oauth_without_settings_secret() {
    let directory = tempdir().unwrap();
    let home = directory.path().join("home");
    fs::create_dir_all(home.join(".kimi-code/credentials")).unwrap();
    fs::write(
        home.join(".kimi-code/credentials/kimi-code.json"),
        r#"{"access_token":"atok","refresh_token":"rtok","expires_at":1786151767}"#,
    )
    .unwrap();
    let store = directory.path().join("auth.json");
    let mut settings = GatewaySettings::default();
    settings.providers.clear();
    assert!(adopt_local_cli_sessions_from(&mut settings, &store, &home).unwrap());
    let provider = settings
        .providers
        .iter()
        .find(|provider| provider.id == "kimi")
        .unwrap();
    assert!(provider.enabled);
    assert_eq!(provider.credential.source, CredentialSource::OAuth);
    assert_eq!(provider.credential.reference.as_deref(), Some("kimi"));
    let settings_json = serde_json::to_string(&settings).unwrap();
    assert!(!settings_json.contains("atok"));
    assert!(!settings_json.contains("rtok"));
    let stored = load_store(&store).unwrap();
    assert_eq!(stored.providers["kimi"].access, "atok");
}

#[test]
fn adopt_does_not_replace_environment_api_key() {
    let directory = tempdir().unwrap();
    let home = directory.path().join("home");
    fs::create_dir_all(home.join(".kimi-code/credentials")).unwrap();
    fs::write(
        home.join(".kimi-code/credentials/kimi-code.json"),
        r#"{"access_token":"atok","refresh_token":"rtok","expires_at":1786151767}"#,
    )
    .unwrap();
    env::set_var("CODETAS_TEST_KIMI_KEY", "user-key");
    let mut settings = GatewaySettings::default();
    settings.providers.clear();
    let mut provider = provider_presets()
        .into_iter()
        .find(|preset| preset.id == "kimi")
        .unwrap()
        .instantiate(None)
        .unwrap();
    provider.credential = ProviderCredential {
        source: CredentialSource::Environment,
        reference: Some("CODETAS_TEST_KIMI_KEY".into()),
        ..ProviderCredential::default()
    };
    settings.providers.push(provider);
    adopt_local_cli_sessions_from(&mut settings, &directory.path().join("auth.json"), &home)
        .unwrap();
    env::remove_var("CODETAS_TEST_KIMI_KEY");
    assert_eq!(
        settings.providers[0].credential.source,
        CredentialSource::Environment
    );
}

#[test]
fn adopt_enables_disabled_env_stub_when_cli_login_exists() {
    let directory = tempdir().unwrap();
    let home = directory.path().join("home");
    fs::create_dir_all(home.join(".kimi-code/credentials")).unwrap();
    fs::write(
        home.join(".kimi-code/credentials/kimi-code.json"),
        r#"{"access_token":"atok","refresh_token":"rtok","expires_at":1786151767}"#,
    )
    .unwrap();
    let store = directory.path().join("auth.json");
    let mut settings = GatewaySettings::default();
    settings.providers.clear();
    let mut provider = provider_presets()
        .into_iter()
        .find(|preset| preset.id == "kimi")
        .unwrap()
        .instantiate(None)
        .unwrap();
    provider.enabled = false;
    provider.credential = ProviderCredential {
        source: CredentialSource::Environment,
        reference: Some("KIMI_API_KEY".into()),
        ..ProviderCredential::default()
    };
    settings.providers.push(provider);
    assert!(adopt_local_cli_sessions_from(&mut settings, &store, &home).unwrap());
    let kimi = settings
        .providers
        .iter()
        .find(|provider| provider.id == "kimi")
        .unwrap();
    assert!(kimi.enabled);
    assert_eq!(kimi.credential.source, CredentialSource::OAuth);
    assert_eq!(kimi.credential.reference.as_deref(), Some("kimi"));
}

#[test]
fn adopt_does_not_resurrect_removed_provider() {
    let directory = tempdir().unwrap();
    let home = directory.path().join("home");
    fs::create_dir_all(home.join(".kimi-code/credentials")).unwrap();
    fs::write(
        home.join(".kimi-code/credentials/kimi-code.json"),
        r#"{"access_token":"atok","refresh_token":"rtok","expires_at":1786151767}"#,
    )
    .unwrap();
    let store = directory.path().join("auth.json");
    let mut settings = GatewaySettings::default();
    settings.providers.clear();
    assert!(adopt_local_cli_sessions_from(&mut settings, &store, &home).unwrap());
    settings.providers.clear();
    assert!(!adopt_local_cli_sessions_from(&mut settings, &store, &home).unwrap());
    assert!(settings.providers.is_empty());
    assert!(load_store(&store).unwrap().providers.contains_key("kimi"));
}

#[test]
fn store_session_is_kept_when_still_valid() {
    let stored = OAuthSession {
        access: "store-access".into(),
        refresh: "store-refresh".into(),
        expires_at_ms: now_ms() + 3_600_000,
        source: "oauth".into(),
        account_id: None,
        email: None,
    };
    assert!(!session_needs_refresh(&stored));
    let cli = OAuthSession {
        access: "cli-access".into(),
        refresh: "cli-refresh".into(),
        expires_at_ms: now_ms() + 3_600_000,
        source: "local-cli".into(),
        account_id: None,
        email: None,
    };
    assert!(cli_session_is_importable(&cli));
    assert!(!session_needs_refresh(&stored) || stored.refresh.is_empty());
}

#[test]
fn store_session_blocks_cli_import_when_present() {
    let stored = OAuthSession {
        access: "store-access".into(),
        refresh: "store-refresh".into(),
        expires_at_ms: 1,
        source: "oauth".into(),
        account_id: None,
        email: None,
    };
    assert!(store_session_blocks_cli_import(Some(&stored)));
    assert!(!store_session_blocks_cli_import(None));
}

#[test]
fn expired_cli_access_without_refresh_is_not_imported() {
    let session = OAuthSession {
        access: "stale".into(),
        refresh: String::new(),
        expires_at_ms: 1,
        source: "local-cli".into(),
        account_id: None,
        email: None,
    };
    assert!(session_needs_refresh(&session));
    assert!(!cli_session_is_importable(&session));
}

#[test]
fn expired_session_without_access_needs_refresh() {
    let session = OAuthSession {
        access: String::new(),
        refresh: "rtok".into(),
        expires_at_ms: 1,
        source: "oauth".into(),
        account_id: None,
        email: None,
    };
    assert!(session_needs_refresh(&session));
}
