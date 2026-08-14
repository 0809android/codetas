use super::*;

pub(super) fn definition_path() -> Result<PathBuf, String> {
    dirs::config_dir()
        .map(|config| config.join("systemd/user/codetas-gateway.service"))
        .ok_or_else(|| "設定フォルダを特定できません".into())
}

pub(super) fn shim_path() -> Result<PathBuf, String> {
    dirs::data_local_dir()
        .map(|data| data.join("codetas/bin/codetas-codex"))
        .ok_or_else(|| "CODETASデータフォルダを特定できません".into())
}

pub(super) fn systemd_quote(path: &Path) -> String {
    format!(
        "\"{}\"",
        path.to_string_lossy()
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('$', "\\$")
    )
}

pub(super) fn service_definition(
    executable: &Path,
    settings: &Path,
    observability: &Path,
) -> Result<String, String> {
    Ok(format!(
        "# {SERVICE_MARKER}\n[Unit]\nDescription=CODETAS Gateway\nAfter=network-online.target\nWants=network-online.target\nStartLimitIntervalSec=60s\nStartLimitBurst=6\n\n[Service]\nType=simple\nExecStart={} --gateway-service --config {} --observability {}\nRestart=on-failure\nRestartSec=5s\nNoNewPrivileges=true\nPrivateTmp=true\n\n[Install]\nWantedBy=default.target\n",
        systemd_quote(executable),
        systemd_quote(settings),
        systemd_quote(observability),
    ))
}

pub(super) fn shim_definition(executable: &Path) -> Result<String, String> {
    Ok(format!(
        "#!/bin/sh\n# {SERVICE_MARKER}\n{} --start-gateway-service >/dev/null 2>&1 || exit $?\nexec codex \"$@\"\n",
        shell_quote(executable)
    ))
}

pub(super) fn reload_service(_definition: &Path) -> Result<bool, String> {
    refresh_service_manager()?;
    run(
        "systemctl",
        &[
            "--user".into(),
            "enable".into(),
            "--now".into(),
            "codetas-gateway.service".into(),
        ],
    )?;
    run(
        "systemctl",
        &[
            "--user".into(),
            "restart".into(),
            "codetas-gateway.service".into(),
        ],
    )?;
    service_is_running()
}

pub(super) fn start_service(_definition: &Path) -> Result<(), String> {
    run(
        "systemctl",
        &[
            "--user".into(),
            "enable".into(),
            "--now".into(),
            "codetas-gateway.service".into(),
        ],
    )
    .map(|_| ())
}

pub(super) fn stop_service(_definition: &Path) -> Result<bool, String> {
    run(
        "systemctl",
        &[
            "--user".into(),
            "disable".into(),
            "--now".into(),
            "codetas-gateway.service".into(),
        ],
    )?;
    Ok(!service_is_running()?)
}

pub(super) fn service_is_running() -> Result<bool, String> {
    Ok(Command::new("systemctl")
        .args(["--user", "is-active", "--quiet", "codetas-gateway.service"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|error| format!("systemctlを実行できません: {error}"))?
        .success())
}

pub(super) fn refresh_service_manager() -> Result<(), String> {
    run("systemctl", &["--user".into(), "daemon-reload".into()]).map(|_| ())
}

pub(super) fn service_registration_exists() -> Result<bool, String> {
    Ok(Command::new("systemctl")
        .args(["--user", "cat", "codetas-gateway.service"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|error| format!("systemctlを実行できません: {error}"))?
        .success())
}
