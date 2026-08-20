use serde::Deserialize;
use services::usage::TextPricingProfile;
use std::collections::HashSet;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Manifest {
    manifest_version: u32,
    source_url: String,
    models: Vec<Entry>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Entry {
    model_id: String,
    upstream_model: String,
    expected_active: bool,
    catalog: Option<serde_json::Value>,
    text_pricing: serde_json::Value,
}

#[test]
fn every_openai_manifest_profile_is_complete_and_valid() {
    let manifest: Manifest =
        serde_json::from_str(include_str!("../../../config/openai_text_pricing.v1.json"))
            .expect("OpenAI pricing manifest must be valid JSON");

    assert_eq!(manifest.manifest_version, 1);
    assert_eq!(
        manifest.source_url,
        "https://developers.openai.com/api/docs/pricing"
    );
    assert_eq!(manifest.models.len(), 18);

    let mut ids = HashSet::new();
    let mut active = 0;
    let mut inactive_gpt56 = 0;
    for entry in manifest.models {
        assert!(ids.insert(entry.model_id.clone()), "duplicate model ID");
        assert_eq!(
            entry.model_id.strip_prefix("openai/"),
            Some(entry.upstream_model.as_str()),
            "canonical and upstream names should differ only by the provider prefix"
        );
        let profile = TextPricingProfile::from_json(entry.text_pricing)
            .unwrap_or_else(|error| panic!("{} profile is invalid: {error}", entry.model_id));
        if entry.expected_active {
            active += 1;
            assert!(entry.catalog.is_none());
            for tier in [
                Some(&profile.tiers.default),
                profile.tiers.flex.as_ref(),
                profile.tiers.priority.as_ref(),
            ]
            .into_iter()
            .flatten()
            {
                assert!(tier.short.cache_write.is_none());
                assert!(tier
                    .long
                    .as_ref()
                    .is_none_or(|rates| rates.cache_write.is_none()));
            }
        } else {
            assert!(entry.model_id.starts_with("openai/gpt-5.6"));
            assert!(entry.catalog.is_some());
            for tier in [
                Some(&profile.tiers.default),
                profile.tiers.flex.as_ref(),
                profile.tiers.priority.as_ref(),
            ]
            .into_iter()
            .flatten()
            {
                assert!(tier.short.cache_write.is_some());
                assert!(tier
                    .long
                    .as_ref()
                    .is_some_and(|rates| rates.cache_write.is_some()));
            }
            inactive_gpt56 += 1;
        }
    }
    assert_eq!(active, 15);
    assert_eq!(inactive_gpt56, 3);
}
