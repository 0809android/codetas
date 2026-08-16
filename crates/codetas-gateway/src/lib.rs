mod anthropic;
mod auth;
mod catalog;
mod client_anthropic;
mod client_chat;
mod client_gemini;
mod compaction;
mod conformance;
mod compat;
mod config;
mod copilot;
mod debug;
mod discovery;
mod gemini;
mod kiro;
mod network;
mod oauth;
mod observability;
mod registry;
mod response_state;
mod routing;
mod server;
mod translate;
mod update;

pub use catalog::{build_codex_catalog, CodexCatalog};
pub use config::{
    parse_gateway_settings_json, validate_provider_id, AccountPoolSettings, AccountPoolStrategy,
    AccountReference, AgentSettings, AgentSurfaceMode, AuxiliaryInputMode, ClientIntegrationSettings,
    CodexIntegrationSettings, CredentialCommand, CredentialSource, CredentialTransport,
    ExternalAccessKey, GatewaySettings, GoogleMode, HelperInterceptSettings,
    ModelDiscoverySettings, ModelMetadata, ObservabilitySettings, ProviderCapabilities,
    ProviderCredential, ProviderDefinition, ProviderLimits, ProviderProtocol, ProviderTransport,
    CatalogSettings, ManagedClientSettings, ResponseItemIdRepairSettings, RouteDefinition,
    RoutePolicySettings, RouteStrategy, RouteTarget, RuntimeSettings,
    SecuritySettings, ShadowRule, SidecarSettings, UpdateChannel, UpdateSettings, SETTINGS_VERSION,
};
pub use conformance::{
    compatibility_lab_report, CompatibilityLabReport, CompatibilityResultRow,
    ConformanceExpectation, ConformanceFixture, ProtocolConformanceFixture,
    CONFORMANCE_FIXTURES, PROTOCOL_CONFORMANCE_FIXTURES,
};
pub use discovery::{
    discover_provider_models, test_provider_connection, ModelDiscoveryError,
    ProviderConnectionReport,
};
pub use oauth::{
    adopt_local_cli_sessions, auth_store_is_configured, auth_store_path, configure_auth_store_path,
    detect_local_cli_session, has_stored_session, login_provider_oauth, oauth_session_credential,
    oauth_provider_registry, provider_supports_native_oauth, OAuthLoginReport,
    OAuthProviderDescriptor,
};
pub use observability::{
    list_observability_trash, preview_observability_cleanup, read_observability_breakdown,
    read_observability_summary, read_recent_observability_events, restore_observability_trash,
    trash_observability_cleanup, ObservabilityBreakdown, ObservabilityBreakdownRow,
    ObservabilityCleanupPreview, ObservabilityStorageFile, ObservabilitySummary,
    ObservabilityTrashEntry, ObservabilityTrashReport, ObservationEvent, TokenUsage,
};
pub use registry::{provider_presets, ProviderPreset};
pub use routing::{dry_run_route, RouteDryRunCandidate, RouteDryRunReport};
pub use server::{
    start_gateway, start_gateway_with_options, GatewayHandle, GatewayRuntimeOptions,
    GatewayStartError, SharedSettings,
};
pub use update::{check_signed_update, SignedUpdateManifest, UpdateCheck};
