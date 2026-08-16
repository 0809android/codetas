use super::*;

impl ProviderDefinition {
    pub fn validate(&self) -> Result<(), String> {
        validate_provider_id(&self.id)?;
        validate_single_line("name", &self.name, 120)?;
        if self.name.trim().is_empty() {
            return Err("name is required".into());
        }

        let url = Url::parse(self.base_url.trim()).map_err(|_| "baseUrl must be a valid URL")?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err("baseUrl must use http or https".into());
        }
        if url.host_str().is_none() {
            return Err("baseUrl must include a host".into());
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err("baseUrl must not contain credentials".into());
        }
        if url.query().is_some() || url.fragment().is_some() {
            return Err("baseUrl must not contain a query or fragment".into());
        }
        if is_private_destination(&url) && !self.allow_private_network {
            return Err("local or private baseUrl requires allowPrivateNetwork".into());
        }
        if url.scheme() == "http" && !(self.allow_private_network && is_private_destination(&url)) {
            return Err(
                "plaintext HTTP baseUrl is allowed only for an explicitly enabled localhost or private IP"
                    .into(),
            );
        }
        if let Some(path) = self.responses_path.as_deref() {
            if !path.starts_with('/')
                || path.contains("://")
                || path.contains('?')
                || path.contains('#')
            {
                return Err(
                    "responsesPath must be an absolute path without URL, query, or fragment".into(),
                );
            }
        }
        if let Some(value) = self.realtime_ws_base_url.as_deref() {
            let realtime =
                Url::parse(value).map_err(|_| "realtimeWsBaseUrl must be a valid URL")?;
            let secure = matches!(realtime.scheme(), "https" | "wss");
            let local_plaintext = matches!(realtime.scheme(), "http" | "ws")
                && self.allow_private_network
                && is_private_destination(&realtime);
            if (!secure && !local_plaintext)
                || realtime.host_str().is_none()
                || !realtime.username().is_empty()
                || realtime.password().is_some()
                || realtime.query().is_some()
                || realtime.fragment().is_some()
            {
                return Err(
                    "realtimeWsBaseUrl must be credential-free wss/https, or ws/http for an allowed private destination"
                        .into(),
                );
            }
        }
        if self.google_mode != GoogleMode::AiStudio
            && self.protocol != ProviderProtocol::GeminiGenerateContent
        {
            return Err("googleMode vertex/cloudCodeAssist requires geminiGenerateContent".into());
        }
        if self.transport == ProviderTransport::Kiro {
            if self.protocol != ProviderProtocol::Responses {
                return Err("Kiro transport requires the Responses protocol surface".into());
            }
            if let Some(profile_arn) = self.kiro_profile_arn.as_deref() {
                validate_single_line("kiroProfileArn", profile_arn, 1_024)?;
                if !profile_arn.starts_with("arn:") {
                    return Err("kiroProfileArn must be an ARN".into());
                }
            }
        } else if self.kiro_profile_arn.is_some() {
            return Err("kiroProfileArn requires the Kiro transport".into());
        }
        if self.transport == ProviderTransport::GithubCopilot {
            if self.protocol != ProviderProtocol::ChatCompletions {
                return Err("GitHub Copilot transport requires chatCompletions".into());
            }
            let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
            if url.scheme() != "https"
                || (host != "api.githubcopilot.com" && !host.ends_with(".githubcopilot.com"))
            {
                return Err(
                    "GitHub Copilot transport requires an HTTPS *.githubcopilot.com baseUrl".into(),
                );
            }
        }
        for (label, value) in [
            ("project", self.project.as_deref()),
            ("location", self.location.as_deref()),
        ] {
            if let Some(value) = value {
                validate_single_line(label, value, 240)?;
                if value.trim().is_empty()
                    || value.bytes().any(|byte| {
                        !(byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
                    })
                {
                    return Err(format!("{label} must use a printable cloud identifier"));
                }
            }
        }
        match (
            self.azure_deployment.as_deref(),
            self.azure_api_version.as_deref(),
        ) {
            (None, None) => {}
            (Some(deployment), Some(version)) => {
                if self.transport != ProviderTransport::Standard
                    || !matches!(
                        self.protocol,
                        ProviderProtocol::Responses | ProviderProtocol::ChatCompletions
                    )
                    || !url
                        .host_str()
                        .is_some_and(|host| host.ends_with(".openai.azure.com"))
                {
                    return Err(
                        "azureDeployment requires a Standard Responses/Chat provider on *.openai.azure.com"
                            .into(),
                    );
                }
                if self.model_protocols.values().any(|protocol| {
                    !matches!(
                        protocol,
                        ProviderProtocol::Responses | ProviderProtocol::ChatCompletions
                    )
                }) {
                    return Err(
                        "Azure deployment model protocol overrides must remain Responses or Chat"
                            .into(),
                    );
                }
                for (label, value) in [
                    ("azureDeployment", deployment),
                    ("azureApiVersion", version),
                ] {
                    validate_single_line(label, value, 240)?;
                    if value.is_empty()
                        || value.bytes().any(|byte| {
                            !(byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
                        })
                    {
                        return Err(format!("{label} contains unsupported characters"));
                    }
                }
            }
            _ => {
                return Err(
                    "azureDeployment and azureApiVersion must be configured together".into(),
                )
            }
        }

        if let Some(env_key) = self.api_key_env.as_deref() {
            validate_env_key(env_key)?;
        }
        self.credential.validate()?;
        for (name, value) in &self.headers {
            validate_header_name(name)?;
            if is_forbidden_transport_header(name) {
                return Err(format!(
                    "transport-owned header {name} cannot be configured"
                ));
            }
            if is_sensitive_name(name) {
                return Err(format!(
                    "sensitive header {name} must use credential or envHeaders instead of persisted headers"
                ));
            }
            validate_single_line("header value", value, 4_096)?;
        }
        for (name, env_key) in &self.env_headers {
            validate_header_name(name)?;
            if is_forbidden_transport_header(name) {
                return Err(format!(
                    "transport-owned header {name} cannot be configured"
                ));
            }
            validate_env_key(env_key)?;
        }
        for (name, value) in &self.query_params {
            validate_single_line("query parameter name", name, 256)?;
            validate_single_line("query parameter value", value, 2_048)?;
            if name.trim().is_empty() {
                return Err("query parameter name cannot be empty".into());
            }
            if is_sensitive_name(name) {
                return Err(format!(
                    "sensitive query parameter {name} cannot be persisted"
                ));
            }
        }
        if let Some(model) = self.default_model.as_deref() {
            validate_model_id(model)?;
        }
        if self.models.len() > 250 {
            return Err("models must contain at most 250 entries".into());
        }
        for model in &self.models {
            validate_model_id(model)?;
        }
        for model in self.model_protocols.keys() {
            validate_model_id(model)?;
        }
        if self.model_wire_ids.len() > 250 || self.model_reasoning_modes.len() > 250 {
            return Err(
                "modelWireIds and modelReasoningModes must contain at most 250 entries".into(),
            );
        }
        for (model, wire_model) in &self.model_wire_ids {
            validate_model_id(model)?;
            validate_model_id(wire_model)?;
            if model == wire_model {
                return Err(format!(
                    "modelWireIds.{model} must map to a different model id"
                ));
            }
        }
        for (model, mode) in &self.model_reasoning_modes {
            validate_model_id(model)?;
            validate_single_line("model reasoning mode", mode, 64)?;
            if mode.trim().is_empty() {
                return Err(format!("modelReasoningModes.{model} cannot be empty"));
            }
        }
        for (label, limits) in [
            ("modelContextWindows", &self.model_context_windows),
            ("modelMaxInputTokens", &self.model_max_input_tokens),
            ("modelMaxOutputTokens", &self.model_max_output_tokens),
        ] {
            if limits.len() > 250 {
                return Err(format!("{label} must contain at most 250 entries"));
            }
            for (model, limit) in limits {
                validate_model_id(model)?;
                if *limit == 0 || *limit > 100_000_000 {
                    return Err(format!("{label}.{model} must be between 1 and 100000000"));
                }
            }
        }
        for (model, max_input) in &self.model_max_input_tokens {
            if self
                .model_context_windows
                .get(model)
                .is_some_and(|context| max_input > context)
            {
                return Err(format!(
                    "modelMaxInputTokens.{model} cannot exceed modelContextWindows"
                ));
            }
        }
        for (model, max_output) in &self.model_max_output_tokens {
            if self
                .model_context_windows
                .get(model)
                .is_some_and(|context| max_output > context)
            {
                return Err(format!(
                    "modelMaxOutputTokens.{model} cannot exceed modelContextWindows"
                ));
            }
        }
        if self.model_input_modalities.len() > 250 {
            return Err("modelInputModalities must contain at most 250 entries".into());
        }
        for (model, modalities) in &self.model_input_modalities {
            validate_model_id(model)?;
            if modalities.is_empty() || modalities.len() > 4 {
                return Err(format!(
                    "modelInputModalities.{model} must contain between 1 and 4 values"
                ));
            }
            let mut seen = HashSet::new();
            for modality in modalities {
                if !matches!(modality.as_str(), "text" | "image" | "audio" | "video")
                    || !seen.insert(modality.as_str())
                {
                    return Err(format!(
                        "modelInputModalities.{model} contains an unsupported or duplicate value"
                    ));
                }
            }
        }
        if self.model_reasoning_efforts.len() > 250
            || self.model_default_reasoning_efforts.len() > 250
        {
            return Err(
                "modelReasoningEfforts and modelDefaultReasoningEfforts must contain at most 250 entries"
                    .into(),
            );
        }
        for (model, efforts) in &self.model_reasoning_efforts {
            validate_model_id(model)?;
            let mut seen = HashSet::new();
            for effort in efforts {
                if !is_reasoning_effort(effort) || !seen.insert(effort.as_str()) {
                    return Err(format!(
                        "modelReasoningEfforts.{model} contains an unsupported or duplicate effort"
                    ));
                }
            }
        }
        for (model, default) in &self.model_default_reasoning_efforts {
            validate_model_id(model)?;
            if !is_reasoning_effort(default)
                || self
                    .model_reasoning_efforts
                    .get(model)
                    .is_some_and(|efforts| !efforts.is_empty() && !efforts.contains(default))
            {
                return Err(format!(
                    "modelDefaultReasoningEfforts.{model} is not supported by that model"
                ));
            }
        }
        for (label, models) in [
            ("noReasoningModels", self.no_reasoning_models.as_slice()),
            (
                "noTemperatureModels",
                self.no_temperature_models.as_slice(),
            ),
            ("noTopPModels", self.no_top_p_models.as_slice()),
            ("noPenaltyModels", self.no_penalty_models.as_slice()),
            (
                "autoToolChoiceOnlyModels",
                self.auto_tool_choice_only_models.as_slice(),
            ),
            (
                "preserveReasoningContentModels",
                self.preserve_reasoning_content_models.as_slice(),
            ),
            (
                "requiresReasoningPlaceholderModels",
                self.requires_reasoning_placeholder_models
                    .as_deref()
                    .unwrap_or(&[]),
            ),
            ("reasoningSplitModels", self.reasoning_split_models.as_slice()),
            ("thinkingToggleModels", self.thinking_toggle_models.as_slice()),
            ("thinkingBudgetModels", self.thinking_budget_models.as_slice()),
        ] {
            if models.len() > 250 {
                return Err(format!("{label} must contain at most 250 entries"));
            }
            let mut seen = HashSet::new();
            for model in models {
                validate_model_id(model)?;
                if !seen.insert(model.as_str()) {
                    return Err(format!("{label} contains a duplicate model: {model}"));
                }
            }
        }
        self.response_item_id_repair.validate()?;
        for (effort, wire) in &self.reasoning_effort_map {
            if !is_reasoning_effort(effort) {
                return Err(format!("unsupported reasoning effort map key: {effort}"));
            }
            validate_single_line("reasoning effort wire value", wire, 64)?;
            if wire.trim().is_empty() {
                return Err("reasoning effort wire value cannot be empty".into());
            }
        }
        for (model, mapping) in &self.model_reasoning_effort_map {
            validate_model_id(model)?;
            for (effort, wire) in mapping {
                if !is_reasoning_effort(effort) {
                    return Err(format!(
                        "unsupported model reasoning effort map key: {effort}"
                    ));
                }
                validate_single_line("model reasoning effort wire value", wire, 64)?;
                if wire.trim().is_empty() {
                    return Err("model reasoning effort wire value cannot be empty".into());
                }
            }
        }
        if self.limits.connect_timeout_ms == 0
            || self.limits.request_timeout_ms == 0
            || self.limits.stream_idle_timeout_ms == 0
        {
            return Err("provider timeouts must be greater than zero".into());
        }
        if self.limits.request_retries > 10 || self.limits.stream_retries > 10 {
            return Err("provider retries must be between 0 and 10".into());
        }
        if !(1_024..=64 * 1024 * 1024).contains(&self.limits.max_request_bytes) {
            return Err("provider request limit must be between 1 KiB and 64 MiB".into());
        }
        if !(1_024..=512 * 1024 * 1024).contains(&self.limits.max_response_bytes) {
            return Err("provider response limit must be between 1 KiB and 512 MiB".into());
        }
        if self.discovery.enabled {
            if !self.discovery.path.starts_with('/')
                || self.discovery.path.contains("://")
                || self.discovery.path.contains('?')
                || self.discovery.path.contains('#')
            {
                return Err(
                    "model discovery path must be an absolute path without URL, query, or fragment"
                        .into(),
                );
            }
            if self.discovery.max_models == 0 || self.discovery.max_models > 5_000 {
                return Err("model discovery maxModels must be between 1 and 5000".into());
            }
        }
        Ok(())
    }

    pub fn endpoint(&self) -> String {
        self.endpoint_for_model(self.default_model.as_deref().unwrap_or("model"))
    }

    pub fn wire_model_id(&self, model: &str) -> String {
        if let Some(wire) = self.model_wire_ids.get(model) {
            return wire.clone();
        }
        if self.strip_model_bracket_suffix && model.ends_with(']') {
            if let Some(index) = model.rfind('[') {
                if index > 0 {
                    return model[..index].to_string();
                }
            }
        }
        model.to_string()
    }

    pub fn endpoint_for_model(&self, model: &str) -> String {
        self.endpoint_for_model_streaming(model, false)
    }

    pub fn endpoint_for_model_streaming(&self, model: &str, streaming: bool) -> String {
        let base = self.base_url.trim().trim_end_matches('/');
        if let Some(deployment) = self.azure_deployment.as_deref() {
            let root = base
                .split_once("/openai/")
                .map(|(root, _)| root)
                .unwrap_or(base);
            let resource = match self.protocol_for_model(model) {
                ProviderProtocol::Responses => "responses",
                ProviderProtocol::ChatCompletions => "chat/completions",
                _ => unreachable!("Azure deployment protocol is validated"),
            };
            return format!("{root}/openai/deployments/{deployment}/{resource}");
        }
        match self.protocol_for_model(model) {
            ProviderProtocol::Responses if self.responses_path.is_some() => {
                match Url::parse(base) {
                    Ok(mut url) => {
                        url.set_path(self.responses_path.as_deref().unwrap_or("/responses"));
                        url.set_query(None);
                        url.set_fragment(None);
                        url.to_string()
                    }
                    Err(_) => format!(
                        "{base}{}",
                        self.responses_path.as_deref().unwrap_or("/responses")
                    ),
                }
            }
            ProviderProtocol::Responses if base.ends_with("/responses") => base.to_string(),
            ProviderProtocol::Responses => format!("{base}/responses"),
            ProviderProtocol::ChatCompletions if base.ends_with("/chat/completions") => {
                base.to_string()
            }
            ProviderProtocol::ChatCompletions => format!("{base}/chat/completions"),
            ProviderProtocol::AnthropicMessages if base.ends_with("/messages") => base.to_string(),
            ProviderProtocol::AnthropicMessages => format!("{base}/messages"),
            ProviderProtocol::GeminiGenerateContent
                if self.google_mode == GoogleMode::CloudCodeAssist =>
            {
                let method = if streaming {
                    "streamGenerateContent"
                } else {
                    "generateContent"
                };
                format!("{base}/v1internal:{method}")
            }
            ProviderProtocol::GeminiGenerateContent if self.google_mode == GoogleMode::Vertex => {
                let method = if streaming {
                    "streamGenerateContent"
                } else {
                    "generateContent"
                };
                match (self.project.as_deref(), self.location.as_deref()) {
                    (Some(project), Some(location)) => {
                        let root = if base == "https://aiplatform.googleapis.com"
                            && location != "global"
                        {
                            format!("https://{location}-aiplatform.googleapis.com")
                        } else {
                            base.to_string()
                        };
                        format!(
                            "{root}/v1/projects/{project}/locations/{location}/publishers/google/models/{}:{method}",
                            model.replace('/', "%2F")
                        )
                    }
                    _ => format!(
                        "{base}/v1/publishers/google/models/{}:{method}",
                        model.replace('/', "%2F")
                    ),
                }
            }
            ProviderProtocol::GeminiGenerateContent if base.contains("{model}") => {
                base.replace("{model}", &model.replace('/', "%2F")).replace(
                    "{method}",
                    if streaming {
                        "streamGenerateContent"
                    } else {
                        "generateContent"
                    },
                )
            }
            ProviderProtocol::GeminiGenerateContent => {
                let method = if streaming {
                    "streamGenerateContent"
                } else {
                    "generateContent"
                };
                format!("{base}/models/{}:{method}", model.replace('/', "%2F"))
            }
        }
    }

    pub fn protocol_for_model(&self, model: &str) -> ProviderProtocol {
        self.model_protocols
            .get(model)
            .copied()
            .unwrap_or(self.protocol)
    }

    pub fn compact_endpoint(&self) -> String {
        let base = self.base_url.trim().trim_end_matches('/');
        if base.ends_with("/responses/compact") {
            base.to_string()
        } else if base.ends_with("/responses") {
            format!("{base}/compact")
        } else {
            format!("{base}/responses/compact")
        }
    }

    pub fn image_endpoint(&self, edits: bool) -> String {
        resource_endpoint(
            &self.base_url,
            if edits {
                "images/edits"
            } else {
                "images/generations"
            },
        )
    }

    pub fn search_endpoint(&self) -> String {
        resource_endpoint(&self.base_url, "alpha/search")
    }

    pub fn video_endpoint(&self) -> String {
        resource_endpoint(&self.base_url, "videos/generations")
    }
}
