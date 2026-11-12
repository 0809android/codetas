use super::*;

#[test]
fn revision_23_settings_with_stale_argon_wire_ids_are_repaired() {
    let mut provider = provider_presets()
        .into_iter()
        .find(|preset| preset.id == "google-antigravity")
        .unwrap()
        .instantiate(None)
        .unwrap();
    for alias in [
        "gemini-4-argon",
        "gemini-4-argon-low",
        "gemini-4-argon-medium",
        "gemini-4-argon-high",
    ] {
        provider
            .model_wire_ids
            .insert(alias.into(), "gemini-4-argon-tiered".into());
    }
    let mut settings = GatewaySettings {
        registry_revision: 23,
        providers: vec![provider],
        ..GatewaySettings::default()
    };
    assert!(backfill_registry_input_limits(&mut settings));
    let wire = &settings.providers[0].model_wire_ids;
    // The model is addressed by its own id, so no argon alias keeps a mapping.
    for alias in [
        "gemini-4-argon",
        "gemini-4-argon-low",
        "gemini-4-argon-medium",
        "gemini-4-argon-high",
    ] {
        assert!(
            wire.get(alias).is_none(),
            "{alias} must not keep a wire mapping"
        );
    }
    // 3.8 Flash keeps its working -tiered id.
    assert_eq!(
        wire.get("gemini-3.8-flash").map(String::as_str),
        Some("gemini-3.8-flash-tiered")
    );
    assert_eq!(settings.registry_revision, REGISTRY_REVISION);
    // A wire mapping that points at itself fails validation and stops the gateway.
    settings.providers[0]
        .validate()
        .expect("migrated Antigravity provider must validate");
}

#[test]
fn antigravity_preset_validates_with_its_wire_aliases() {
    let provider = provider_presets()
        .into_iter()
        .find(|preset| preset.id == "google-antigravity")
        .unwrap()
        .instantiate(None)
        .unwrap();
    provider.validate().expect("preset must validate");
    assert!(provider
        .model_wire_ids
        .keys()
        .all(|alias| !alias.starts_with("gemini-4-argon")));
}
