use super::*;

pub(super) fn definition_path() -> Result<PathBuf, String> {
    dirs::data_local_dir()
        .map(|data| data.join("CODETAS/service/codetas-gateway-task.xml"))
        .ok_or_else(|| "設定フォルダを特定できません".into())
}

pub(super) fn shim_path() -> Result<PathBuf, String> {
    dirs::data_local_dir()
        .map(|data| data.join("CODETAS/bin/codetas-codex.cmd"))
        .ok_or_else(|| "CODETASデータフォルダを特定できません".into())
}

pub(super) fn service_definition(
    executable: &Path,
    settings: &Path,
    observability: &Path,
) -> Result<String, String> {
    let user = std::env::var("USERNAME").map_err(|_| "Windowsユーザー名を取得できません")?;
    let arguments = format!(
        "--gateway-service --config {} --observability {}",
        windows_quote(settings),
        windows_quote(observability)
    );
    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!-- {SERVICE_MARKER} -->
<Task version="1.4" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
<Triggers><LogonTrigger><Enabled>true</Enabled></LogonTrigger></Triggers>
<Principals><Principal id="Author"><UserId>{}</UserId><LogonType>InteractiveToken</LogonType><RunLevel>LeastPrivilege</RunLevel></Principal></Principals>
<Settings><MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy><DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries><StopIfGoingOnBatteries>false</StopIfGoingOnBatteries><ExecutionTimeLimit>PT0S</ExecutionTimeLimit><RestartOnFailure><Interval>PT5S</Interval><Count>10</Count></RestartOnFailure><Enabled>true</Enabled></Settings>
<Actions Context="Author"><Exec><Command>{}</Command><Arguments>{}</Arguments></Exec></Actions>
</Task>"#,
        xml_escape(&user),
        xml_escape(&executable.to_string_lossy()),
        xml_escape(&arguments),
    ))
}

pub(super) fn shim_definition(executable: &Path) -> Result<String, String> {
    Ok(format!(
        "@echo off\r\nrem {SERVICE_MARKER}\r\n{} --start-gateway-service >NUL 2>&1 || exit /b %ERRORLEVEL%\r\ncodex %*\r\n",
        windows_quote(executable)
    ))
}

pub(super) fn reload_service(definition: &Path) -> Result<bool, String> {
    if service_registration_exists()? && !stop_service(definition)? {
        return Err("Task Schedulerの旧Gatewayを安全に停止・解除できません".into());
    }
    run(
        "schtasks.exe",
        &[
            "/Create".into(),
            "/TN".into(),
            WINDOWS_TASK_NAME.into(),
            "/XML".into(),
            definition.to_string_lossy().into_owned(),
            "/F".into(),
        ],
    )?;
    start_service(definition)?;
    service_is_running()
}

pub(super) fn start_service(_definition: &Path) -> Result<(), String> {
    run(
        "schtasks.exe",
        &["/Run".into(), "/TN".into(), WINDOWS_TASK_NAME.into()],
    )?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !service_is_running()? {
        if std::time::Instant::now() >= deadline {
            return Err("Task SchedulerがGatewayを10秒以内に起動できませんでした".into());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    Ok(())
}

pub(super) fn stop_service(_definition: &Path) -> Result<bool, String> {
    if service_is_running()? {
        if let Err(error) = run(
            "schtasks.exe",
            &["/End".into(), "/TN".into(), WINDOWS_TASK_NAME.into()],
        ) {
            if service_is_running()? {
                return Err(error);
            }
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while service_is_running()? {
            if std::time::Instant::now() >= deadline {
                return Err(
                    "Task Scheduler ended the task but the Gateway process did not stop within 10 seconds"
                        .into(),
                );
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
    run(
        "schtasks.exe",
        &[
            "/Delete".into(),
            "/TN".into(),
            WINDOWS_TASK_NAME.into(),
            "/F".into(),
        ],
    )?;
    Ok(!service_registration_exists()?)
}

pub(super) fn service_is_running() -> Result<bool, String> {
    if !service_registration_exists()? {
        return Ok(false);
    }
    let script = format!(
        "(Get-ScheduledTask -TaskName '{}').State -eq 'Running'",
        WINDOWS_TASK_NAME.replace('\'', "''")
    );
    run(
        "powershell.exe",
        &[
            "-NoProfile".into(),
            "-NonInteractive".into(),
            "-Command".into(),
            script,
        ],
    )
    .map(|output| output.eq_ignore_ascii_case("true"))
}

pub(super) fn service_registration_exists() -> Result<bool, String> {
    Ok(Command::new("schtasks.exe")
        .args(["/Query", "/TN", WINDOWS_TASK_NAME])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|error| format!("schtasks.exeを実行できません: {error}"))?
        .success())
}

pub(super) fn refresh_service_manager() -> Result<(), String> {
    Ok(())
}
