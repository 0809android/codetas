use super::*;

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
        hermes: input.hermes,
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
    for client in [
        "claudeCode", "claudeDesktop", "claudeDesktopInference", "opencode", "grok", "pi",
        "hermes",
    ] {
        settings.integrations.managed_clients.insert(
            client.into(),
            ManagedClientSettings {
                enabled: false,
                config_path: None,
                owned_fields: Vec::new(),
            },
        );
    }
    for artifact in &report.clients {
        let owned_fields = match artifact.client.as_str() {
            "opencode" => vec!["provider.codetas".into()],
            "pi" => vec!["providers.codetas".into()],
            "hermes" => vec!["providers.codetas".into()],
            "claudeDesktop" => vec!["mcpServers.codetas-project".into()],
            "claudeDesktopInference" => vec!["inferenceModels".into()],
            "claudeCode" | "grok" => vec!["launcher.environment".into()],
            _ => Vec::new(),
        };
        settings.integrations.managed_clients.insert(
            artifact.client.clone(),
            ManagedClientSettings {
                enabled: artifact.enabled,
                config_path: artifact.path.clone(),
                owned_fields,
            },
        );
    }
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

pub(crate) async fn current_observability_summary(
    app: &AppHandle,
    manager: &State<'_, GatewayManager>,
) -> Result<ObservabilitySummary, String> {
    let guard = manager.handle.lock().await;
    if let Some(handle) = guard.as_ref() {
        return Ok(handle.observability_summary());
    }
    Ok(read_observability_summary(observability_path(app)?))
}

pub(crate) fn diagnostic(
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
