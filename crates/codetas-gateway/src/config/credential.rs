use super::*;

pub(crate) fn resource_endpoint(base_url: &str, resource: &str) -> String {
    let base = base_url.trim().trim_end_matches('/');
    let suffix = format!("/{resource}");
    if base.ends_with(&suffix) {
        base.to_string()
    } else {
        format!("{base}{suffix}")
    }
}

/// Anthropic's Messages API and its model discovery endpoint both live under a
/// versioned prefix. Presets ship `https://api.anthropic.com/v1`, but a provider
/// saved or edited without that suffix would request `/messages` and get a
/// bare 404 from the edge (empty body, no JSON error) instead of a real API
/// response. Normalize so both forms resolve to the same versioned root.
pub(crate) fn versioned_anthropic_base(base_url: &str) -> String {
    let trimmed = base_url.trim().trim_end_matches('/');
    let last = trimmed.rsplit('/').next().unwrap_or_default();
    // Accept ordinary API version segments such as `v1`, `v2`, or `v1beta` so a
    // base that already carries a version is never versioned twice.
    let versioned = last
        .strip_prefix(['v', 'V'])
        .is_some_and(|rest| rest.chars().next().is_some_and(|c| c.is_ascii_digit()))
        && last.chars().all(|c| c.is_ascii_alphanumeric());
    if versioned {
        trimmed.to_string()
    } else {
        format!("{trimmed}/v1")
    }
}

impl ProviderCredential {
    pub(crate) fn validate(&self) -> Result<(), String> {
        match self.source {
            CredentialSource::None => {
                if self.command.is_some() {
                    return Err("credential command requires command source".into());
                }
            }
            CredentialSource::Environment => {
                let reference = self
                    .reference
                    .as_deref()
                    .ok_or("environment credential requires a reference")?;
                validate_env_key(reference)?;
            }
            CredentialSource::Keychain => {
                let reference = self
                    .reference
                    .as_deref()
                    .ok_or("credential source requires a reference")?;
                validate_single_line("credential reference", reference, 240)?;
                if reference.trim().is_empty() {
                    return Err("credential reference cannot be empty".into());
                }
            }
            CredentialSource::OAuth => {
                if let Some(command) = self.command.as_ref() {
                    validate_credential_command(command)?;
                } else {
                    let reference = self.reference.as_deref().ok_or(
                        "OAuth credential requires a provider reference or broker command",
                    )?;
                    validate_single_line("OAuth credential reference", reference, 240)?;
                    if reference.trim().is_empty() {
                        return Err("OAuth credential reference cannot be empty".into());
                    }
                }
            }
            CredentialSource::Command => {
                let command = self
                    .command
                    .as_ref()
                    .ok_or("command credential requires command settings")?;
                validate_credential_command(command)?;
            }
            CredentialSource::Forward => {
                if self.reference.is_some() || self.command.is_some() {
                    return Err("forward credential cannot store a reference or command".into());
                }
            }
        }
        if self.source != CredentialSource::Forward
            && self.transport == CredentialTransport::CustomHeader
        {
            validate_header_name(
                self.header_name
                    .as_deref()
                    .ok_or("custom header credential requires headerName")?,
            )?;
        }
        Ok(())
    }
}

pub(crate) fn validate_credential_command(command: &CredentialCommand) -> Result<(), String> {
    validate_single_line("credential command", &command.program, 1_024)?;
    if command.program.trim().is_empty()
        || command.timeout_ms == 0
        || command.timeout_ms > 60_000
        || command.refresh_interval_ms > 86_400_000
    {
        return Err(
            "credential command requires a program, timeout 1-60000 ms, and refresh interval up to 24 hours"
                .into(),
        );
    }
    for argument in &command.args {
        validate_single_line("credential command argument", argument, 4_096)?;
    }
    if let Some(cwd) = command.cwd.as_deref() {
        validate_single_line("credential command cwd", cwd, 4_096)?;
    }
    Ok(())
}
