use super::*;


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

pub(crate) async fn ensure_observability_offline(manager: &State<'_, GatewayManager>) -> Result<(), String> {
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

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}
