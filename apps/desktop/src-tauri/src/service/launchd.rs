use super::*;

pub(super) fn definition_path() -> Result<PathBuf, String> {
    dirs::home_dir()
        .map(|home| {
            home.join("Library/LaunchAgents")
                .join(format!("{SERVICE_LABEL}.plist"))
        })
        .ok_or_else(|| "ホームフォルダを特定できません".into())
}

pub(super) fn shim_path() -> Result<PathBuf, String> {
    dirs::data_local_dir()
        .map(|data| data.join("codetas/bin/codetas-codex"))
        .ok_or_else(|| "CODETASデータフォルダを特定できません".into())
}

pub(super) fn service_definition(
    executable: &Path,
    settings: &Path,
    observability: &Path,
) -> Result<String, String> {
    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<!-- {SERVICE_MARKER} -->
<plist version="1.0"><dict>
<key>Label</key><string>{SERVICE_LABEL}</string>
<key>ProgramArguments</key><array>
<string>{}</string><string>--gateway-service</string>
<string>--config</string><string>{}</string>
<string>--observability</string><string>{}</string>
</array>
<key>RunAtLoad</key><true/>
<key>KeepAlive</key><true/>
<key>ThrottleInterval</key><integer>5</integer>
<key>ProcessType</key><string>Background</string>
<key>StandardOutPath</key><string>/dev/null</string>
<key>StandardErrorPath</key><string>/dev/null</string>
</dict></plist>
"#,
        xml_escape(&executable.to_string_lossy()),
        xml_escape(&settings.to_string_lossy()),
        xml_escape(&observability.to_string_lossy()),
    ))
}

pub(super) fn shim_definition(executable: &Path) -> Result<String, String> {
    Ok(format!(
        "#!/bin/sh\n# {SERVICE_MARKER}\n{} --start-gateway-service >/dev/null 2>&1 || exit $?\nexec codex \"$@\"\n",
        shell_quote(executable)
    ))
}

pub(super) fn service_domain() -> Result<String, String> {
    let uid = run("id", &["-u".into()])?;
    if !uid.chars().all(|character| character.is_ascii_digit()) {
        return Err("現在のユーザーIDを検証できません".into());
    }
    Ok(format!("gui/{uid}"))
}

pub(super) fn reload_service(definition: &Path) -> Result<bool, String> {
    let domain = service_domain()?;
    if service_registration_exists()? && !stop_service(definition)? {
        return Err("launchdの旧Gatewayを安全に停止・解除できません".into());
    }
    run(
        "launchctl",
        &[
            "bootstrap".into(),
            domain.clone(),
            definition.to_string_lossy().into_owned(),
        ],
    )?;
    run(
        "launchctl",
        &[
            "kickstart".into(),
            "-k".into(),
            format!("{domain}/{SERVICE_LABEL}"),
        ],
    )?;
    service_is_running()
}

pub(super) fn start_service(_definition: &Path) -> Result<(), String> {
    let domain = service_domain()?;
    run(
        "launchctl",
        &[
            "kickstart".into(),
            "-k".into(),
            format!("{domain}/{SERVICE_LABEL}"),
        ],
    )
    .map(|_| ())
}

pub(super) fn stop_service(_definition: &Path) -> Result<bool, String> {
    let domain = service_domain()?;
    let target = format!("{domain}/{SERVICE_LABEL}");
    let status = Command::new("launchctl")
        .args(["bootout", target.as_str()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|error| format!("launchctlを実行できません: {error}"))?;
    Ok(status.success())
}

pub(super) fn service_is_running() -> Result<bool, String> {
    let domain = service_domain()?;
    let target = format!("{domain}/{SERVICE_LABEL}");
    Ok(Command::new("launchctl")
        .args(["print", target.as_str()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|error| format!("launchctlを実行できません: {error}"))?
        .success())
}

pub(super) fn refresh_service_manager() -> Result<(), String> {
    Ok(())
}

pub(super) fn service_registration_exists() -> Result<bool, String> {
    service_is_running()
}
