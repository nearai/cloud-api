//! Separate provider-monitor catalog for the initial GLM-only OpenRouter launch.
//! The ordinary OpenAI catalog remains available to Cloud clients.
use axum::{extract::State, http::StatusCode, response::Json};
use serde_json::{json, Map, Value};
use services::models::ModelsServiceTrait;
use std::sync::Arc;

use crate::models::{ErrorResponse, ModelInfo};

pub const LAUNCH_MODEL: &str = "z-ai/glm-5.3-flash";

#[derive(Clone)]
pub struct OpenRouterState {
    pub models_service: Arc<dyn ModelsServiceTrait>,
    pub ready: bool,
    pub zdr: Option<bool>,
}

impl OpenRouterState {
    pub fn from_env(models_service: Arc<dyn ModelsServiceTrait>) -> Self {
        Self {
            models_service,
            ready: std::env::var("OPENROUTER_GLM53_FLASH_READY").as_deref() == Ok("true"),
            zdr: match std::env::var("OPENROUTER_GLM53_FLASH_ZDR").as_deref() {
                Ok("true") => Some(true),
                Ok("false") => Some(false),
                _ => None,
            },
        }
    }
}

/// OpenRouter provider schema 2.4; only the explicitly approved launch model.
/// Readiness is independent of the general Cloud catalog and defaults to false.
#[utoipa::path(
    get,
    path = "/v1/openrouter/models",
    tag = "Models",
    responses(
        (status = 200, description = "GLM-only OpenRouter provider catalog", body = serde_json::Value),
        (status = 503, description = "Catalog unavailable or unsupported metadata", body = ErrorResponse)
    )
)]
pub async fn models(
    State(state): State<OpenRouterState>,
) -> Result<Json<Value>, (StatusCode, Json<ErrorResponse>)> {
    let unavailable = || {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse::new(
                "OpenRouter catalog is unavailable".into(),
                "catalog_unavailable".into(),
            )),
        )
    };
    let catalog = state
        .models_service
        .get_models_with_pricing()
        .await
        .map_err(|_| unavailable())?;
    let mut data = Vec::new();
    for model in catalog.into_iter().filter(|m| m.model_name == LAUNCH_MODEL) {
        // This launch contract covers our own flat-priced deployment. Never
        // project a tiered or external model as this verified configuration.
        if model.text_pricing.is_some()
            || model.provider_type != "vllm"
            || model.input_modalities.is_none()
            || model.output_modalities.is_none()
            || model.input_cost_per_token < 0
            || model.output_cost_per_token < 0
            || model
                .cache_read_cost_per_token
                .is_some_and(|price| price < 0)
            || model.cost_per_image != 0
        {
            return Err(unavailable());
        }
        let model = super::completions::model_with_pricing_to_info(model);
        data.push(document(model, state.ready, state.zdr).ok_or_else(unavailable)?);
    }
    Ok(Json(json!({"data": data})))
}

fn document(model: ModelInfo, ready: bool, zdr: Option<bool>) -> Option<Value> {
    let input = model.input_modalities.as_ref()?;
    if model.id != LAUNCH_MODEL
        || !input.iter().any(|m| m == "text")
        || input.iter().collect::<std::collections::HashSet<_>>().len() != input.len()
        || input.iter().any(|m| m != "text" && m != "image")
        || model.output_modalities.as_deref()? != ["text"]
        || model.text_pricing.is_some()
    {
        return None;
    }
    let context = model.context_length.filter(|n| *n > 0)?;
    let output = model
        .max_output_length
        .filter(|n| *n > 0 && *n <= context)?;
    let name = model.name.as_ref().filter(|s| !s.is_empty())?;
    let hf = model.hugging_face_id.as_ref().filter(|s| !s.is_empty())?;
    if model.quantization.as_ref().is_some_and(|q| {
        !matches!(
            q.as_str(),
            "int4"
                | "int8"
                | "fp4"
                | "mxfp4"
                | "nvfp4"
                | "fp6"
                | "fp8"
                | "mxfp8"
                | "fp16"
                | "bf16"
                | "fp32"
        )
    }) {
        return None;
    }
    let pricing = model.pricing.as_ref()?;
    let mut input_prices =
        vec![json!({"type": "prompt", "unit": "token", "cost_usd": pricing.prompt})];
    if let Some(cached) = &pricing.input_cache_read {
        input_prices.push(json!({"type": "cached_prompt", "unit": "token", "cost_usd": cached}));
    }
    // Image tokens share the prompt-token tariff. A zero legacy per-image
    // surcharge must not be advertised as free image inference.
    let inputs: Vec<Value> = input
        .iter()
        .map(|kind| {
            let mut entry = json!({"type": kind, "pricing": input_prices});
            if kind == "text" {
                entry["supported_inputs"] =
                    json!({"max_context_length": {"value": context, "unit": "token"}});
            }
            entry
        })
        .collect();
    let mut parameters = Map::new();
    for parameter in &model.supported_sampling_parameters {
        // The catalog knows support, not backend-specific bounds. Do not
        // manufacture numeric ranges or stop-list limits from examples.
        let descriptor = match parameter.as_str() {
            "temperature" | "top_p" | "min_p" | "frequency_penalty" | "presence_penalty"
            | "repetition_penalty" => json!({"type": "range"}),
            "top_k" | "seed" => json!({"type": "integer"}),
            "max_tokens" => json!({"type": "integer", "min": 1, "max": output, "unit": "token"}),
            "stop" => json!({"type": "array"}),
            // Variable token-ID keys cannot be represented by a closed object.
            "logit_bias" => json!({"type": "unknown"}),
            _ => continue,
        };
        parameters.insert(parameter.clone(), descriptor);
    }
    for feature in &model.supported_features {
        if matches!(
            feature.as_str(),
            "tools" | "structured_outputs" | "json_mode" | "reasoning"
        ) {
            parameters.insert(feature.clone(), json!({"type": "boolean"}));
        }
    }
    let has_datacenters = model.datacenters.as_ref().is_some_and(|d| !d.is_empty());
    let mut doc = json!({
        "schema_version": "2.4", "id": model.id, "name": name,
        "hugging_face_id": hf, "created": model.created,
        "input_modalities": inputs,
        "output_modalities": [{"type": "text", "streaming": true,
            "max_length": {"value": output, "unit": "token"},
            "supported_parameters": parameters,
            "pricing": [{"type": "completion", "unit": "token", "cost_usd": pricing.completion}]}],
        "is_ready": ready && model.is_ready == Some(true) && has_datacenters && zdr == Some(true)
    });
    if let Some(quantization) = model.quantization {
        doc["quantization"] = json!(quantization);
    }
    if let Some(description) = model.description {
        doc["description"] = json!(description);
    }
    if let Some(datacenters) = model.datacenters {
        doc["datacenters"] = json!(datacenters);
    }
    if let Some(openrouter) = model.openrouter {
        doc["openrouter"] = json!(openrouter);
    }
    if let Some(date) = model.deprecation_date {
        doc["deprecation_date"] = json!(date);
    }
    if let Some(zdr) = zdr {
        doc["compliance"] = json!({"zdr": zdr});
    }
    Some(doc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use services::models::{ModelWithPricing, ModelsError};

    struct Catalog(Vec<ModelWithPricing>);

    #[async_trait::async_trait]
    impl ModelsServiceTrait for Catalog {
        async fn get_models_with_pricing(&self) -> Result<Vec<ModelWithPricing>, ModelsError> {
            Ok(self.0.clone())
        }
        async fn get_models(&self) -> Result<Vec<services::models::ModelInfo>, ModelsError> {
            unreachable!()
        }
        async fn get_model_by_name(&self, _: &str) -> Result<ModelWithPricing, ModelsError> {
            unreachable!()
        }
        async fn resolve_and_get_model(&self, _: &str) -> Result<ModelWithPricing, ModelsError> {
            unreachable!()
        }
        async fn resolve_alias_cached(&self, _: &str) -> Option<String> {
            unreachable!()
        }
        async fn get_configured_model_names(&self) -> Result<Vec<String>, ModelsError> {
            unreachable!()
        }
        async fn invalidate_models_cache(&self) {}
    }

    fn service_model(name: &str) -> ModelWithPricing {
        ModelWithPricing {
            id: uuid::Uuid::nil(),
            model_name: name.into(),
            model_display_name: "GLM 5.3 Flash".into(),
            model_description: "Synthetic catalog fixture".into(),
            model_icon: None,
            input_cost_per_token: 150,
            output_cost_per_token: 500,
            cost_per_image: 0,
            cache_read_cost_per_token: Some(35),
            text_pricing: None,
            context_length: 1048576,
            verifiable: true,
            aliases: vec![],
            owned_by: "nearai".into(),
            provider_type: "vllm".into(),
            provider_config: None,
            attestation_supported: true,
            input_modalities: Some(vec!["text".into(), "image".into()]),
            output_modalities: Some(vec!["text".into()]),
            inference_url: None,
            hugging_face_id: Some("zai-org/GLM-5.3-Flash".into()),
            quantization: Some("fp8".into()),
            max_output_length: Some(131072),
            supported_sampling_parameters: vec!["max_tokens".into()],
            supported_features: vec!["tools".into()],
            datacenters: Some(vec!["US".into()]),
            is_ready: Some(true),
            deprecation_date: None,
            openrouter_slug: None,
            created_at: chrono::DateTime::from_timestamp(1, 0).unwrap(),
        }
    }

    #[tokio::test]
    async fn http_catalog_is_public_glm_only_and_hidden_by_default() {
        use tower::ServiceExt;
        let service = Arc::new(Catalog(vec![
            service_model("another/model"),
            service_model(LAUNCH_MODEL),
        ]));
        let router = crate::build_model_routes(service);
        let response = router
            .oneshot(
                axum::http::Request::builder()
                    .uri("/openrouter/models")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 65536)
            .await
            .unwrap();
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["data"].as_array().unwrap().len(), 1);
        assert_eq!(value["data"][0]["id"], LAUNCH_MODEL);
        // The two explicit readiness environment variables are absent in CI.
        assert_eq!(value["data"][0]["is_ready"], false);
    }

    #[tokio::test]
    async fn omitted_and_incompatible_deployments_do_not_get_advertised() {
        let state = |catalog| {
            State(OpenRouterState {
                models_service: Arc::new(Catalog(catalog)),
                ready: true,
                zdr: Some(true),
            })
        };
        assert_eq!(
            models(state(vec![service_model("another/model")]))
                .await
                .unwrap()
                .0["data"],
            json!([])
        );
        let mut model = service_model(LAUNCH_MODEL);
        model.provider_type = "external".into();
        assert_eq!(
            models(state(vec![model])).await.unwrap_err().0,
            StatusCode::SERVICE_UNAVAILABLE
        );
        let mut model = service_model(LAUNCH_MODEL);
        model.input_cost_per_token = -1;
        assert_eq!(
            models(state(vec![model])).await.unwrap_err().0,
            StatusCode::SERVICE_UNAVAILABLE
        );
        let mut model = service_model(LAUNCH_MODEL);
        model.input_modalities = None;
        assert_eq!(
            models(state(vec![model])).await.unwrap_err().0,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    fn fixture() -> ModelInfo {
        serde_json::from_value(json!({
            "id": LAUNCH_MODEL, "object": "model", "created": 1, "owned_by": "nearai",
            "name": "GLM 5.3 Flash", "hugging_face_id": "zai-org/GLM-5.3-Flash", "quantization": "fp8",
            "pricing": {"input": 0.15, "output": 0.5, "prompt": "0.00000015", "completion": "0.0000005",
                "image": "0", "request": "0", "input_cache_read": "0.000000035"},
            "context_length": 1048576, "max_output_length": 131072,
            "input_modalities": ["text", "image"], "output_modalities": ["text"],
            "supported_sampling_parameters": ["temperature", "max_tokens", "stop", "logit_bias"],
            "supported_features": ["tools", "reasoning"], "is_ready": true,
            "datacenters": [{"country_code": "US"}]
        })).unwrap()
    }

    #[test]
    fn readiness_requires_explicit_launch_zdr_and_location() {
        assert_eq!(
            document(fixture(), false, Some(true)).unwrap()["is_ready"],
            false
        );
        assert_eq!(document(fixture(), true, None).unwrap()["is_ready"], false);
        assert_eq!(
            document(fixture(), true, Some(false)).unwrap()["is_ready"],
            false
        );
        let mut model = fixture();
        model.datacenters = None;
        assert_eq!(
            document(model, true, Some(true)).unwrap()["is_ready"],
            false
        );
        let mut model = fixture();
        model.is_ready = None;
        assert_eq!(
            document(model, true, Some(true)).unwrap()["is_ready"],
            false
        );
        assert_eq!(
            document(fixture(), true, Some(true)).unwrap()["is_ready"],
            true
        );
    }

    #[test]
    fn rejects_unapproved_models_modalities_and_tiered_pricing() {
        let mut model = fixture();
        model.id = "another/model".into();
        assert!(document(model, true, Some(true)).is_none());
        let mut model = fixture();
        model.output_modalities = Some(vec!["image".into()]);
        assert!(document(model, true, Some(true)).is_none());
        let mut model = fixture();
        model.text_pricing = Some(json!({}));
        assert!(document(model, true, Some(true)).is_none());
    }

    #[test]
    fn owns_pricing_by_modality_and_preserves_cache_price() {
        let doc = document(fixture(), false, None).unwrap();
        assert_eq!(doc["schema_version"], "2.4");
        assert!(doc.get("pricing").is_none());
        assert!(doc.get("compliance").is_none());
        for modality in doc["input_modalities"].as_array().unwrap() {
            assert_eq!(modality["pricing"][0]["cost_usd"], "0.00000015");
            assert_eq!(modality["pricing"][1]["cost_usd"], "0.000000035");
            assert_eq!(modality["pricing"][0]["unit"], "token");
        }
        assert_eq!(
            doc["output_modalities"][0]["pricing"][0]["cost_usd"],
            "0.0000005"
        );
        assert_eq!(
            doc["output_modalities"][0]["supported_parameters"]["max_tokens"]["max"],
            131072
        );
        assert!(doc["output_modalities"][0]["supported_parameters"]
            .get("structured_outputs")
            .is_none());
        // Also usable by the independently downloaded schema validator.
        if let Ok(path) = std::env::var("OPENROUTER_TEST_DOCUMENT_PATH") {
            std::fs::write(path, serde_json::to_vec_pretty(&doc).unwrap()).unwrap();
        }
    }
}
