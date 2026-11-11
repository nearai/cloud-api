use super::{overflow, InferenceType, UsageError, UsageEstimate};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Counts computed locally where the request is already visible. No content crosses admission.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageFeatures {
    pub inference_type: InferenceType,
    pub text_bytes: u64,
    pub token_ids: u64,
    pub images: u64,
    pub audio: u64,
    pub video: u64,
    pub unknown_media: u64,
    pub batch_items: u64,
    pub choices: u64,
    pub executions: u64,
    pub requested_output_tokens: Option<u64>,
    pub model_output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
    pub media_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_duration_seconds: Option<u64>,
    pub requested_images: u64,
    pub questions: u64,
}

impl UsageFeatures {
    /// Walk borrowed chat content so estimation does not duplicate inline media payloads.
    pub fn from_chat(
        params: &inference_providers::ChatCompletionParams,
    ) -> Result<Self, UsageError> {
        let mut features = Self::from_json(
            InferenceType::ChatCompletion,
            &serde_json::json!({
                "n": params.n, "max_tokens":params.max_tokens, "service_tier":params.service_tier,
                "max_completion_tokens":params.max_completion_tokens,
                "best_of":params.extra.get("best_of"), "max_output_tokens":params.extra.get("max_output_tokens"),
            }),
        )?;
        for message in &params.messages {
            if let Some(content) = &message.content {
                features.count(content)?;
            }
            features.count(&serde_json::to_value(&message.role).map_err(|_| overflow())?)?;
            for text in [
                &message.name,
                &message.tool_call_id,
                &message.reasoning_content,
            ]
            .into_iter()
            .flatten()
            {
                features.text_bytes = features
                    .text_bytes
                    .checked_add(text.len() as u64)
                    .ok_or_else(overflow)?;
            }
            // Tool-call arguments are text; their protocol wrapper is small and has no media.
            if let Some(calls) = &message.tool_calls {
                features.count(&serde_json::to_value(calls).map_err(|_| overflow())?)?;
            }
        }
        for name in [
            "prompt",
            "input",
            "instructions",
            "system",
            "state",
            "questions",
            "assets",
        ] {
            if let Some(value) = params.extra.get(name) {
                features.count(value)?;
            }
        }
        if let Some(items) = params.extra.get("prompt").and_then(Value::as_array) {
            if !items.iter().all(Value::is_number) {
                features.batch_items = items.len().max(1) as u64;
            }
        }
        features.questions = match params.extra.get("questions") {
            Some(Value::Array(items)) => items.len().max(1) as u64,
            Some(Value::Object(items)) => items.len().max(1) as u64,
            _ => 1,
        };
        if let Some(tools) = &params.tools {
            features.count_schema(tools)?;
        }
        if let Some(choice) = &params.tool_choice {
            features.count_schema(choice)?;
        }
        for value in [
            params.extra.get("functions"),
            params.extra.get("response_format"),
            params.extra.get("text").and_then(|v| v.get("format")),
        ]
        .into_iter()
        .flatten()
        {
            features.count_schema(value)?;
        }
        Ok(features)
    }

    fn count_schema(&mut self, value: &impl Serialize) -> Result<(), UsageError> {
        struct Size(u64);
        impl std::io::Write for Size {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0 = self
                    .0
                    .checked_add(bytes.len() as u64)
                    .ok_or_else(|| std::io::Error::other("schema too large"))?;
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut size = Size(0);
        serde_json::to_writer(&mut size, value).map_err(|_| overflow())?;
        self.text_bytes = self.text_bytes.checked_add(size.0).ok_or_else(overflow)?;
        Ok(())
    }
    pub fn from_json(kind: InferenceType, body: &Value) -> Result<Self, UsageError> {
        let number = |name: &str, allow_zero: bool| -> Result<Option<u64>, UsageError> {
            body.get(name)
                .filter(|v| !v.is_null())
                .map(|v| {
                    v.as_u64().filter(|n| allow_zero || *n > 0).ok_or_else(|| {
                        UsageError::ValidationError(
                            "invalid generation count or output limit".into(),
                        )
                    })
                })
                .transpose()
        };
        let mut features = Self {
            inference_type: kind,
            text_bytes: 0,
            token_ids: 0,
            images: 0,
            audio: 0,
            video: 0,
            unknown_media: 0,
            batch_items: 1,
            choices: number("n", false)?
                .unwrap_or(1)
                .max(number("best_of", false)?.unwrap_or(1)),
            executions: 1,
            requested_output_tokens: number("max_completion_tokens", true)?
                .or(number("max_output_tokens", true)?)
                .or(number("max_tokens", true)?),
            model_output_tokens: None,
            service_tier: body.get("service_tier").and_then(Value::as_str).map(
                |value| match value {
                    "default" | "auto" | "priority" | "fast" | "flex" => value.to_owned(),
                    _ => "unknown".into(),
                },
            ),
            media_bytes: 0,
            audio_duration_seconds: None,
            requested_images: number("n", false)?.unwrap_or(1),
            questions: 0,
        };
        for name in [
            "messages",
            "prompt",
            "input",
            "instructions",
            "system",
            "state",
            "questions",
            "assets",
        ] {
            if let Some(value) = body.get(name) {
                features.count(value)?;
            }
        }
        // Schemas are part of the model input. Count their serialized keys and values;
        // the media walker intentionally skips fields such as `data`, `id`, and URLs.
        for value in [
            body.get("tools"),
            body.get("functions"),
            body.get("tool_choice"),
            body.get("response_format"),
            body.pointer("/text/format"),
        ]
        .into_iter()
        .flatten()
        {
            features.count_schema(value)?;
        }
        if let Some(items) = body.get("prompt").and_then(Value::as_array) {
            if !items.iter().all(Value::is_number) {
                features.batch_items = items.len().max(1) as u64;
            }
        }
        features.questions = match body.get("questions") {
            Some(Value::Array(items)) => items.len().max(1) as u64,
            Some(Value::Object(items)) => items.len().max(1) as u64,
            _ => 1,
        };
        Ok(features)
    }

    pub fn with_model_output(mut self, limit: Option<i32>) -> Self {
        self.model_output_tokens = limit.filter(|n| *n > 0).map(|n| n as u64);
        self
    }

    fn count(&mut self, value: &Value) -> Result<(), UsageError> {
        let add = |target: &mut u64, n: u64| -> Result<(), UsageError> {
            *target = target.checked_add(n).ok_or_else(overflow)?;
            Ok(())
        };
        match value {
            Value::String(text) => add(&mut self.text_bytes, text.len() as u64)?,
            Value::Array(items) if !items.is_empty() && items.iter().all(Value::is_number) => {
                add(&mut self.token_ids, items.len() as u64)?
            }
            Value::Array(items) => {
                for item in items {
                    self.count(item)?;
                }
            }
            Value::Object(fields) => {
                let kind = fields.get("type").and_then(Value::as_str).unwrap_or("");
                let media = match kind {
                    "image" | "image_url" | "input_image" => Some(&mut self.images),
                    "audio" | "input_audio" | "audio_url" => Some(&mut self.audio),
                    "video" | "video_url" | "input_video" => Some(&mut self.video),
                    "file" | "input_file" | "document" => Some(&mut self.unknown_media),
                    _ => None,
                };
                if let Some(count) = media {
                    add(count, 1)?;
                } else {
                    for (name, item) in fields {
                        if item.is_null() {
                            continue;
                        }
                        match name.as_str() {
                            "image_url" | "input_image" | "image" => add(&mut self.images, 1)?,
                            "audio_url" | "input_audio" | "audio" => add(&mut self.audio, 1)?,
                            "video_url" | "input_video" | "video" => add(&mut self.video, 1)?,
                            "file_id" | "file_url" | "file_data" | "url" | "data" | "b64_json" => {
                                add(&mut self.unknown_media, 1)?
                            }
                            "id" => {}
                            _ => self.count(item)?,
                        }
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    pub fn estimate(&self) -> Result<UsageEstimate, UsageError> {
        if self.executions == 0
            || self.batch_items == 0
            || self.choices == 0
            || self.requested_images == 0
        {
            return Err(UsageError::ValidationError(
                "zero execution multiplier".into(),
            ));
        }
        let policy = super::policy::ESTIMATE_POLICY;
        let mul = |a: u64, b: u64| a.checked_mul(b).ok_or_else(overflow);
        let generations = mul(mul(self.batch_items, self.choices)?, self.executions)?;
        let mut input = self.text_bytes.div_ceil(u64::from(policy.bytes_per_token));
        for units in [
            self.token_ids,
            mul(self.images, policy.image_input_tokens as u64)?,
            mul(self.audio, policy.audio_input_tokens as u64)?,
            mul(self.video, policy.video_input_tokens as u64)?,
            mul(self.unknown_media, policy.unknown_input_tokens as u64)?,
            32,
        ] {
            input = input.checked_add(units).ok_or_else(overflow)?;
        }
        input = mul(input, mul(self.choices, self.executions)?)?;
        let fallback_output = self
            .model_output_tokens
            .unwrap_or(policy.default_output_tokens as u64);
        // This caps only the quote. The provider still receives the original request.
        let requested_output = self.requested_output_tokens.unwrap_or(fallback_output);
        let mut output = mul(
            requested_output.min(self.model_output_tokens.unwrap_or(requested_output)),
            generations,
        )?;
        let mut images = 0;
        match self.inference_type {
            InferenceType::ImageGeneration | InferenceType::ImageEdit => {
                input = 0;
                output = 0;
                images = mul(self.requested_images, self.executions)?;
            }
            InferenceType::AudioTranscription => {
                input = mul(
                    self.audio_duration_seconds
                        .filter(|seconds| *seconds > 0)
                        .unwrap_or(policy.default_audio_seconds as u64)
                        .max(
                            self.media_bytes
                                .div_ceil(u64::from(policy.audio_bytes_per_second)),
                        ),
                    self.executions,
                )?;
                output = 0;
            }
            InferenceType::Decisions => {
                output = mul(
                    mul(self.questions.max(1), policy.decision_output_tokens as u64)?,
                    self.executions,
                )?;
            }
            InferenceType::ChatCompletion | InferenceType::ChatCompletionStream => {}
            _ => {
                return Err(UsageError::ValidationError(
                    "admission is unavailable for this inference type".into(),
                ))
            }
        }
        Ok(UsageEstimate {
            features: self.clone(),
            input_units: i32::try_from(input).map_err(|_| overflow())?,
            output_tokens: i32::try_from(output).map_err(|_| overflow())?,
            images: i32::try_from(images).map_err(|_| overflow())?,
            policy,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_contract_fixtures_and_borrowed_chat_features_match() {
        let cases: Vec<Value> = serde_json::from_str(include_str!("features.json")).unwrap();
        for case in cases {
            let features =
                UsageFeatures::from_json(InferenceType::ChatCompletion, &case["body"]).unwrap();
            assert_eq!(
                serde_json::to_value(features).unwrap(),
                case["expected"],
                "{}",
                case["name"]
            );
            if case["body"].get("messages").is_some() {
                let mut body = case["body"].clone();
                body["model"] = json!("test");
                let params: inference_providers::ChatCompletionParams =
                    serde_json::from_value(body).unwrap();
                let serialized = serde_json::to_value(&params).unwrap();
                let expected =
                    UsageFeatures::from_json(InferenceType::ChatCompletion, &serialized).unwrap();
                assert_eq!(
                    serde_json::to_value(UsageFeatures::from_chat(&params).unwrap()).unwrap(),
                    serde_json::to_value(expected).unwrap()
                );
            }
        }
    }
    use serde_json::json;

    #[test]
    fn estimates_do_not_modify_requests_or_fetch_media() {
        for body in [
            json!({"messages":[{"role":"user","content":"hi"}]}),
            json!({"messages":[{"content":[{"type":"image_url","image_url":{"url":"https://example.invalid/image"}}]}],"max_tokens":42}),
            json!({"prompt":[[1,2],[3,4]],"best_of":3,"max_tokens":8}),
        ] {
            let before = body.clone();
            let features = UsageFeatures::from_json(InferenceType::ChatCompletion, &body).unwrap();
            let estimate = features.estimate().unwrap();
            assert!(estimate.input_units > 0 && estimate.output_tokens > 0);
            assert_eq!(body, before);
            if body.get("max_tokens").is_none() {
                assert_eq!(estimate.output_tokens, 4096);
            }
        }
    }

    #[test]
    fn media_and_decisions_use_their_existing_billing_units() {
        for (kind, body, bytes, expected) in [
            (InferenceType::ImageGeneration, json!({"n":3}), 0, (0, 0, 3)),
            (
                InferenceType::AudioTranscription,
                json!({}),
                1024 * 90,
                (90, 0, 0),
            ),
            (
                InferenceType::Decisions,
                json!({"state":"abc","questions":[{},{}]}),
                0,
                (33, 256, 0),
            ),
        ] {
            let mut features = UsageFeatures::from_json(kind, &body).unwrap();
            features.media_bytes = bytes;
            let e = features.estimate().unwrap();
            assert_eq!((e.input_units, e.output_tokens, e.images), expected);
        }
    }

    #[test]
    fn schema_keys_and_media_named_properties_count_as_model_input() {
        use serde_json::json;
        // Large keys and "data"/"id" properties used to be omitted by the media walker.
        let schema = json!({"type": "object", "properties": {
            "data": {"const": "x".repeat(120_000)},
            "id": {"enum": ["y".repeat(60_000)]},
            "z".repeat(10_000): {"type": "number"}
        }});
        for body in [
            json!({"response_format": {"type": "json_schema", "json_schema": {"schema": schema}}}),
            json!({"text": {"format": {"type": "json_schema", "schema": schema}}}),
            json!({"tools": [{"type": "function", "function": {"parameters": schema}}]}),
            json!({"tools": [{"name": "test", "input_schema": schema}]}),
            json!({"functions": [{"parameters": schema}]}),
        ] {
            let original = body.clone();
            let features = UsageFeatures::from_json(InferenceType::ChatCompletion, &body).unwrap();
            assert!(features.text_bytes >= 190_000);
            assert_eq!((features.images, features.unknown_media), (0, 0));
            assert_eq!(body, original, "estimation must not alter inference");
        }
    }

    #[test]
    fn measured_audio_duration_prevents_compressed_uploads_from_under_reserving() {
        let mut features =
            UsageFeatures::from_json(InferenceType::AudioTranscription, &json!({})).unwrap();
        features.media_bytes = 1024;
        for (duration, expected) in [(None, 60), (Some(2), 2), (Some(3600), 3600)] {
            features.audio_duration_seconds = duration;
            assert_eq!(features.estimate().unwrap().input_units, expected);
        }
        let mut legacy = serde_json::to_value(&features).unwrap();
        legacy
            .as_object_mut()
            .unwrap()
            .remove("audio_duration_seconds");
        assert!(serde_json::from_value::<UsageFeatures>(legacy)
            .unwrap()
            .audio_duration_seconds
            .is_none());
    }

    #[test]
    fn overflow_fails_closed() {
        let mut features =
            UsageFeatures::from_json(InferenceType::ChatCompletion, &json!({})).unwrap();
        features.executions = u64::MAX;
        assert!(features.estimate().is_err());
    }

    #[test]
    fn output_quote_respects_catalog_cap_without_changing_request_or_zero_limit() {
        for (request, cap, expected) in [
            (None, Some(128000), 128000),
            (Some(u64::MAX), Some(8192), 8192),
            (Some(0), Some(8192), 0),
            (None, Some(1000), 1000),
        ] {
            let body = json!({"max_tokens":request});
            let features = UsageFeatures::from_json(InferenceType::ChatCompletion, &body)
                .unwrap()
                .with_model_output(cap);
            assert_eq!(features.estimate().unwrap().output_tokens, expected);
            assert_eq!(features.requested_output_tokens, request);
            assert_eq!(body["max_tokens"], json!(request));
        }
    }

    #[test]
    fn transcription_measurement_never_reduces_the_byte_floor() {
        let mut features =
            UsageFeatures::from_json(InferenceType::AudioTranscription, &json!({})).unwrap();
        features.media_bytes = 120 * 1024 + 1;
        for duration in [None, Some(0), Some(2), Some(120)] {
            features.audio_duration_seconds = duration;
            assert_eq!(features.estimate().unwrap().input_units, 121);
        }
        features.audio_duration_seconds = Some(3600);
        assert_eq!(features.estimate().unwrap().input_units, 3600);
    }
}
