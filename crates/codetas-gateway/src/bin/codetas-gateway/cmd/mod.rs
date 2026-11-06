pub(super) use super::super::{
    atomic_write_new_or_owned, backup_path, read_valid_settings, secure_existing_file,
};
pub(super) use super::{
    find_executable, parse_google_mode, parse_provider_transport, ACCESS_USAGE, ACCOUNT_USAGE,
    AGENT_USAGE, MODELS_USAGE, OBSERVE_USAGE, PROVIDER_USAGE, ROUTE_USAGE, SYSTEM_USAGE,
};
pub(super) use system::parse_credential_patch;
pub(super) use util::*;

pub(super) mod access;
pub(super) mod account;
pub(super) mod agent;
pub(super) mod models;
pub(super) mod observe;
pub(super) mod provider;
pub(super) mod route;
pub(super) mod system;
pub(super) mod util;

/// Assigning `models` directly skips what a new model needs from its preset and
/// older sibling, such as reasoning preservation, which nothing fills in later.
pub(crate) fn inherit_settings_for_new_models(
    provider: &mut codetas_gateway::ProviderDefinition,
    ids: &[String],
    discovered: &[codetas_gateway::ModelMetadata],
) {
    let new_models = ids
        .iter()
        .filter(|id| !provider.models.contains(id))
        .cloned()
        .collect::<Vec<_>>();
    for model in &new_models {
        let reported_window = discovered
            .iter()
            .find(|row| &row.model_id == model)
            .and_then(|row| row.context_window);
        codetas_gateway::inherit_settings_with_reported_window(provider, model, reported_window);
    }
}
