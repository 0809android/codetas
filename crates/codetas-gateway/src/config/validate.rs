use super::*;

impl GatewaySettings {
    pub fn prune_stale_client_integrations(&mut self) {
        if !self.integrations.claude_desktop {
            self.integrations.claude_desktop_aliases.clear();
            self.integrations.claude_desktop_families.clear();
            self.integrations.claude_desktop_defaults.clear();
            return;
        }
        let provider_ids = self
            .providers
            .iter()
            .filter(|provider| provider.enabled)
            .map(|provider| provider.id.as_str())
            .collect::<HashSet<_>>();
        let route_ids = self
            .routes
            .iter()
            .filter(|route| route.enabled)
            .flat_map(|route| [Some(route.id.as_str()), route.alias.as_deref()])
            .flatten()
            .collect::<HashSet<_>>();
        self.integrations
            .claude_desktop_aliases
            .retain(|_, target| {
                validate_routing_reference(target, &provider_ids, &route_ids).is_ok()
            });
        let targets = self
            .integrations
            .claude_desktop_aliases
            .values()
            .cloned()
            .collect::<HashSet<_>>();
        self.integrations
            .claude_desktop_families
            .retain(|target, family| {
                targets.contains(target)
                    && matches!(family.as_str(), "opus" | "fable" | "sonnet" | "haiku")
            });
        let families = self.integrations.claude_desktop_families.clone();
        self.integrations
            .claude_desktop_defaults
            .retain(|family, target| {
                families
                    .get(target)
                    .is_some_and(|assigned| assigned == family)
            });
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.version != SETTINGS_VERSION {
            return Err(format!("unsupported settings version: {}", self.version));
        }
        if self.registry_revision > REGISTRY_REVISION {
            return Err(format!(
                "unsupported registry revision: {}",
                self.registry_revision
            ));
        }
        let mut ids = HashSet::new();
        for provider in &self.providers {
            provider.validate()?;
            if !ids.insert(provider.id.as_str()) {
                return Err(format!("duplicate provider id: {}", provider.id));
            }
        }
        if let Some(default_provider) = self.default_provider.as_deref() {
            if !self
                .providers
                .iter()
                .any(|provider| provider.id == default_provider && provider.enabled)
            {
                return Err("defaultProvider must reference an enabled provider".into());
            }
        }
        if self.runtime.port == 0 {
            return Err("runtime port must be greater than zero".into());
        }
        if !(100..=300_000).contains(&self.runtime.shutdown_timeout_ms) {
            return Err("runtime shutdown timeout must be between 100 ms and 5 minutes".into());
        }
        validate_single_line("runtime host", &self.runtime.host, 255)?;
        let runtime_ip = if self.runtime.host == "localhost" {
            Some(IpAddr::V4(Ipv4Addr::LOCALHOST))
        } else {
            Some(
                self.runtime
                    .host
                    .parse::<IpAddr>()
                    .map_err(|_| "runtime host must be localhost or an IP address")?,
            )
        };
        let loopback_host = runtime_ip.is_some_and(|ip| ip.is_loopback());
        if !loopback_host && !self.security.allow_remote {
            return Err("non-loopback runtime host requires allowRemote".into());
        }
        let external_auth_enabled = self
            .security
            .external_access_keys
            .iter()
            .any(|key| key.enabled);
        if self.security.allow_remote
            && !self.security.require_local_token
            && !external_auth_enabled
        {
            return Err("remote access requires local or scoped external authentication".into());
        }
        for origin in &self.security.cors_allow_origins {
            validate_cors_origin(origin)?;
        }
        match (
            self.updates.manifest_url.as_deref(),
            self.updates.public_key_base64.as_deref(),
        ) {
            (None, None) if !self.updates.auto_check => {}
            (Some(url), Some(key)) => {
                let url = Url::parse(url).map_err(|_| "update manifest URL is invalid")?;
                if url.scheme() != "https"
                    || url.host_str().is_none()
                    || url.username() != ""
                    || url.password().is_some()
                    || url.fragment().is_some()
                {
                    return Err("update manifest URL must be a credential-free HTTPS URL".into());
                }
                validate_single_line("update public key", key, 256)?;
                if key.trim().is_empty() {
                    return Err("update public key cannot be empty".into());
                }
            }
            _ => {
                return Err(
                    "update manifest URL and public key must be configured together; autoCheck requires both"
                        .into(),
                )
            }
        }
        match (
            self.updates.installer_endpoint.as_deref(),
            self.updates.installer_public_key.as_deref(),
        ) {
            (None, None) => {}
            (Some(endpoint), Some(key)) => {
                let url =
                    Url::parse(endpoint).map_err(|_| "update installer endpoint is invalid")?;
                if url.scheme() != "https"
                    || url.host_str().is_none()
                    || url.username() != ""
                    || url.password().is_some()
                    || url.fragment().is_some()
                {
                    return Err(
                        "update installer endpoint must be a credential-free HTTPS URL".into(),
                    );
                }
                validate_single_line("update installer public key", key, 4_096)?;
                if key.trim().is_empty() {
                    return Err("update installer public key cannot be empty".into());
                }
            }
            _ => return Err(
                "update installer endpoint and installer public key must be configured together"
                    .into(),
            ),
        }
        let mut access_key_ids = HashSet::new();
        for key in &self.security.external_access_keys {
            validate_provider_id(&key.id)?;
            validate_single_line("external access key label", &key.label, 120)?;
            validate_env_key(&key.env_var)?;
            if key.label.trim().is_empty() {
                return Err(format!("external access key {} requires a label", key.id));
            }
            if !access_key_ids.insert(key.id.as_str()) {
                return Err(format!("duplicate external access key id: {}", key.id));
            }
            if key.scopes.is_empty() {
                return Err(format!(
                    "external access key {} requires at least one scope",
                    key.id
                ));
            }
            let mut scopes = HashSet::new();
            for scope in &key.scopes {
                if !matches!(
                    scope.as_str(),
                    "gateway:*"
                        | "health:read"
                        | "models:read"
                        | "responses:write"
                        | "chat:write"
                        | "messages:write"
                        | "gemini:write"
                        | "tokens:count"
                        | "images:write"
                        | "search:write"
                        | "videos:write"
                        | "realtime:write"
                        | "sidecars:write"
                ) {
                    return Err(format!("unsupported external access scope: {scope}"));
                }
                if !scopes.insert(scope.as_str()) {
                    return Err(format!(
                        "duplicate scope on external access key {}: {scope}",
                        key.id
                    ));
                }
            }
        }
        if self.account_pool.auto_switch_threshold_percent > 100 {
            return Err("account auto-switch threshold must be between 0 and 100".into());
        }
        if self.agents.max_threads == 0 || self.agents.max_threads > 128 {
            return Err("agent maxThreads must be between 1 and 128".into());
        }
        if !self.observability.redact_content {
            return Err("observability redactContent must remain enabled".into());
        }
        if self.observability.retention_days == 0 || self.observability.retention_days > 3_650 {
            return Err("observability retentionDays must be between 1 and 3650".into());
        }
        if !(64_u64 * 1024..=10_u64 * 1024 * 1024 * 1024)
            .contains(&self.observability.max_storage_bytes)
        {
            return Err("observability maxStorageBytes must be between 64 KiB and 10 GiB".into());
        }
        if self.observability.trash_retention_days == 0
            || self.observability.trash_retention_days > 365
        {
            return Err("observability trashRetentionDays must be between 1 and 365".into());
        }
        if !(64_u64 * 1024..=10_u64 * 1024 * 1024 * 1024)
            .contains(&self.observability.max_trash_bytes)
        {
            return Err("observability maxTrashBytes must be between 64 KiB and 10 GiB".into());
        }

        let mut model_keys = HashSet::new();
        for model in &self.model_catalog {
            if !ids.contains(model.provider_id.as_str()) {
                return Err(format!(
                    "model references unknown provider: {}",
                    model.provider_id
                ));
            }
            validate_model_id(&model.model_id)?;
            let key = format!("{}/{}", model.provider_id, model.model_id);
            if !model_keys.insert(key.clone()) {
                return Err(format!("duplicate model metadata: {key}"));
            }
            for price in [
                model.input_price_per_million,
                model.output_price_per_million,
            ]
            .into_iter()
            .flatten()
            {
                if !price.is_finite() || price < 0.0 {
                    return Err(format!(
                        "model pricing must be finite and non-negative: {key}"
                    ));
                }
            }
            for modality in &model.input_modalities {
                if !matches!(modality.as_str(), "text" | "image" | "audio" | "video") {
                    return Err(format!("unsupported model input modality: {modality}"));
                }
            }
            for (name, limit) in [
                ("contextWindow", model.context_window),
                ("maxInputTokens", model.max_input_tokens),
                ("maxOutputTokens", model.max_output_tokens),
            ] {
                if limit == Some(0) {
                    return Err(format!("model {name} must be greater than zero: {key}"));
                }
            }
            if let Some(context) = model.context_window {
                if model.max_input_tokens.is_some_and(|limit| limit > context)
                    || model.max_output_tokens.is_some_and(|limit| limit > context)
                {
                    return Err(format!(
                        "model token limits cannot exceed contextWindow: {key}"
                    ));
                }
            }
            let mut reasoning_efforts = HashSet::new();
            for effort in &model.reasoning_efforts {
                if !is_reasoning_effort(effort) {
                    return Err(format!("unsupported model reasoning effort: {effort}"));
                }
                if !reasoning_efforts.insert(effort.as_str()) {
                    return Err(format!(
                        "duplicate model reasoning effort on {key}: {effort}"
                    ));
                }
            }
            if let Some(default) = model.default_reasoning_effort.as_deref() {
                if !is_reasoning_effort(default)
                    || (!model.reasoning_efforts.is_empty() && !reasoning_efforts.contains(default))
                {
                    return Err(format!(
                        "invalid default reasoning effort on {key}: {default}"
                    ));
                }
            }
        }

        let mut route_ids = HashSet::new();
        for route in &self.routes {
            validate_provider_id(&route.id)?;
            validate_single_line("route name", &route.name, 120)?;
            if let Some(description) = route.description.as_deref() {
                if description.len() > 1_000 {
                    return Err("route description is too long".into());
                }
                if description
                    .chars()
                    .any(|character| {
                        character.is_control() && !matches!(character, '\n' | '\r' | '\t')
                    })
                {
                    return Err("route description contains unsupported control characters".into());
                }
            }
            if !route_ids.insert(route.id.as_str()) || ids.contains(route.id.as_str()) {
                return Err(format!("duplicate or colliding route id: {}", route.id));
            }
            if let Some(alias) = route.alias.as_deref() {
                validate_model_id(alias)?;
                if alias.contains('/') || !route_ids.insert(alias) || ids.contains(alias) {
                    return Err(format!("duplicate or colliding route alias: {alias}"));
                }
            }
            if route
                .default_reasoning_effort
                .as_deref()
                .is_some_and(|effort| !is_reasoning_effort(effort))
            {
                return Err(format!(
                    "route {} has an invalid default reasoning effort",
                    route.id
                ));
            }
            if route.targets.is_empty() {
                return Err(format!("route {} requires at least one target", route.id));
            }
            if route.sticky_requests == 0 || route.failure_threshold == 0 {
                return Err(format!(
                    "route {} requires positive stickyRequests and failureThreshold values",
                    route.id
                ));
            }
            for target in &route.targets {
                let (provider_id, model_id) = target.model.split_once('/').ok_or_else(|| {
                    format!("route target must use provider/model: {}", target.model)
                })?;
                if !ids.contains(provider_id) {
                    return Err(format!(
                        "route target references unknown provider: {provider_id}"
                    ));
                }
                validate_model_id(model_id)?;
                if target.weight == 0 {
                    return Err("route target weight must be greater than zero".into());
                }
            }
        }

        if self.helper_intercept.enabled {
            let target = self
                .helper_intercept
                .target_model
                .as_deref()
                .ok_or("enabled helper intercept requires targetModel")?;
            validate_routing_reference(target, &ids, &route_ids)?;
            if self.helper_intercept.source_models.is_empty()
                || self.helper_intercept.source_models.len() > 32
            {
                return Err("helper intercept requires 1-32 source models".into());
            }
            let mut source_models = HashSet::new();
            for source in &self.helper_intercept.source_models {
                validate_model_id(source)?;
                if source == target {
                    return Err("helper intercept target cannot equal a source model".into());
                }
                if !source_models.insert(source.as_str()) {
                    return Err(format!("duplicate helper intercept source model: {source}"));
                }
            }
        } else if self.helper_intercept.target_model.is_some() {
            if let Some(target) = self.helper_intercept.target_model.as_deref() {
                validate_routing_reference(target, &ids, &route_ids)?;
            }
        }

        if self.agents.subagent_models.len() > 32 || self.agents.subagent_fallback.len() > 32 {
            return Err("agent model rosters may contain at most 32 entries".into());
        }
        for effort in [
            self.agents.effort_cap.as_deref(),
            self.agents.subagent_effort_cap.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            if !is_reasoning_effort(effort) {
                return Err(format!("unsupported agent reasoning effort cap: {effort}"));
            }
        }
        for target in self
            .agents
            .subagent_models
            .iter()
            .chain(self.agents.subagent_fallback.iter())
        {
            validate_routing_reference(target, &ids, &route_ids)?;
        }
        if !(1_000..=600_000).contains(&self.agents.auxiliary_timeout_ms) {
            return Err("agent auxiliaryTimeoutMs must be between 1 second and 10 minutes".into());
        }
        if !(1..=64).contains(&self.agents.video_sample_frames) {
            return Err("agent videoSampleFrames must be between 1 and 64".into());
        }
        if !(1..=100).contains(&self.agents.document_max_pages) {
            return Err("agent documentMaxPages must be between 1 and 100".into());
        }

        let mut shadow_ids = HashSet::new();
        for rule in &self.shadows {
            validate_provider_id(&rule.id)?;
            if !shadow_ids.insert(rule.id.as_str()) {
                return Err(format!("duplicate shadow rule id: {}", rule.id));
            }
            validate_routing_reference(&rule.source_model, &ids, &route_ids)?;
            if rule.targets.is_empty() || rule.targets.len() > 4 {
                return Err(format!("shadow rule {} requires 1-4 targets", rule.id));
            }
            if rule.sample_percent == 0 || rule.sample_percent > 100 {
                return Err(format!(
                    "shadow rule {} samplePercent must be 1-100",
                    rule.id
                ));
            }
            if !(100..=60_000).contains(&rule.timeout_ms) {
                return Err(format!(
                    "shadow rule {} timeoutMs must be 100-60000",
                    rule.id
                ));
            }
            if !(1_024..=16 * 1024 * 1024).contains(&rule.max_response_bytes) {
                return Err(format!(
                    "shadow rule {} maxResponseBytes must be 1 KiB-16 MiB",
                    rule.id
                ));
            }
            let mut targets = HashSet::new();
            for target in &rule.targets {
                validate_routing_reference(target, &ids, &route_ids)?;
                if target == &rule.source_model {
                    return Err(format!(
                        "shadow rule {} cannot target its own source",
                        rule.id
                    ));
                }
                if !targets.insert(target.as_str()) {
                    return Err(format!(
                        "duplicate target in shadow rule {}: {target}",
                        rule.id
                    ));
                }
            }
        }

        for (kind, target, capability) in [
            (
                "webSearchModel",
                self.sidecars.web_search_model.as_deref(),
                SidecarCapability::WebSearch,
            ),
            (
                "visionModel",
                self.sidecars.vision_model.as_deref(),
                SidecarCapability::Vision,
            ),
            (
                "videoInputModel",
                self.sidecars.video_input_model.as_deref(),
                SidecarCapability::Vision,
            ),
            (
                "documentModel",
                self.sidecars.document_model.as_deref(),
                SidecarCapability::Vision,
            ),
            (
                "imageModel",
                self.sidecars.image_model.as_deref(),
                SidecarCapability::ImageGeneration,
            ),
            (
                "videoModel",
                self.sidecars.video_model.as_deref(),
                SidecarCapability::VideoGeneration,
            ),
            (
                "liveModel",
                self.sidecars.live_model.as_deref(),
                SidecarCapability::Realtime,
            ),
        ] {
            let Some(target) = target else { continue };
            validate_routing_reference(target, &ids, &route_ids)?;
            if !routing_reference_supports(self, target, capability) {
                return Err(format!(
                    "sidecar {kind} target does not advertise the required capability: {target}"
                ));
            }
        }

        if self.integrations.claude_desktop_aliases.len() > 365
            || self.integrations.claude_desktop_families.len() > 365
            || self.integrations.claude_desktop_defaults.len() > 4
        {
            return Err("Claude Desktop profile exceeds the CODETAS limits".into());
        }
        let desktop_routes = self
            .integrations
            .claude_desktop_aliases
            .values()
            .map(String::as_str)
            .collect::<HashSet<_>>();
        for (alias, target) in &self.integrations.claude_desktop_aliases {
            validate_model_id(alias)?;
            if alias.contains('/')
                || ids.contains(alias.as_str())
                || route_ids.contains(alias.as_str())
            {
                return Err(format!(
                    "Claude Desktop alias collides with a provider or route: {alias}"
                ));
            }
            validate_routing_reference(target, &ids, &route_ids)?;
        }
        for (target, family) in &self.integrations.claude_desktop_families {
            if !desktop_routes.contains(target.as_str())
                || !matches!(family.as_str(), "opus" | "fable" | "sonnet" | "haiku")
            {
                return Err(format!(
                    "invalid Claude Desktop family assignment: {target}"
                ));
            }
        }
        for (family, target) in &self.integrations.claude_desktop_defaults {
            if !matches!(family.as_str(), "opus" | "fable" | "sonnet" | "haiku")
                || !desktop_routes.contains(target.as_str())
                || self
                    .integrations
                    .claude_desktop_families
                    .get(target)
                    .is_none_or(|assigned| assigned != family)
            {
                return Err(format!(
                    "invalid Claude Desktop default for family {family}"
                ));
            }
        }

        let mut account_ids = HashSet::new();
        for account in &self.account_pool.accounts {
            validate_provider_id(&account.id)?;
            validate_single_line("account label", &account.label, 120)?;
            if account.label.trim().is_empty() {
                return Err(format!("account {} requires a label", account.id));
            }
            if !account_ids.insert(account.id.as_str()) {
                return Err(format!("duplicate account id: {}", account.id));
            }
            if !ids.contains(account.provider_id.as_str()) {
                return Err(format!(
                    "account references unknown provider: {}",
                    account.provider_id
                ));
            }
            account.credential.validate()?;
        }
        for (provider_id, account_id) in &self.account_pool.active_accounts {
            if !ids.contains(provider_id.as_str()) {
                return Err(format!(
                    "active account references unknown provider: {provider_id}"
                ));
            }
            if !self.account_pool.accounts.iter().any(|account| {
                account.enabled && account.provider_id == *provider_id && account.id == *account_id
            }) {
                return Err(format!(
                    "active account {account_id} is not enabled for provider {provider_id}"
                ));
            }
        }
        Ok(())
    }
}
