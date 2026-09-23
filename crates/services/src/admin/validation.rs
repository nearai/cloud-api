use super::*;

impl AdminServiceImpl {
    pub(super) async fn validate_model_request(
        model_name: &str,
        request: &UpdateModelAdminRequest,
        repository: Arc<dyn AdminRepository>,
    ) -> Result<(), AdminError> {
        // All costs use fixed scale 9 (nano-dollars) and USD - no scale/currency validation needed

        if let Some(Some(profile)) = &request.text_pricing {
            profile
                .validate()
                .map_err(|error| AdminError::InvalidPricing(error.to_string()))?;
        }

        // Validate model name
        if model_name.trim().is_empty() {
            return Err(AdminError::InvalidPricing(
                "Model name cannot be empty".to_string(),
            ));
        }

        // Validate display name if provided
        if let Some(ref display_name) = request.model_display_name {
            if display_name.trim().is_empty() {
                return Err(AdminError::InvalidPricing(
                    "Model display name cannot be empty".to_string(),
                ));
            }
        }

        // Validate description if provided
        if let Some(ref description) = request.model_description {
            if description.trim().is_empty() {
                return Err(AdminError::InvalidPricing(
                    "Model description cannot be empty".to_string(),
                ));
            }
        }

        // Activation pricing gate: reject requests that would result in an
        // active model with all-zero pricing unless allow_free is set.
        //
        // The gate fires when:
        //   - is_active is explicitly Some(true), OR
        //   - is_active is None AND the model does not exist yet (new model
        //     inserts default is_active to true, so omitting it on create is
        //     equivalent to passing is_active=true).
        //
        // The free-check covers all four cost fields: input, output, image,
        // and cache_read.  A model with non-zero cost on any field is treated
        // as priced and is not blocked.
        let existing = repository
            .get_model_validation_state(model_name)
            .await
            .map_err(|e| AdminError::InternalError(e.to_string()))?;

        let is_new_model = existing.is_none();

        // Evaluate whether this request will result in an active model.
        let existing = existing.unwrap_or(ModelValidationState {
            input_cost_per_token: 0,
            output_cost_per_token: 0,
            cost_per_image: 0,
            cache_read_cost_per_token: None,
            allow_free: false,
            provider_type: "vllm".to_string(),
            provider_config: None,
        });

        let effective_provider_type = request
            .provider_type
            .as_deref()
            .unwrap_or(&existing.provider_type);
        if effective_provider_type == "external" {
            let config = request
                .provider_config
                .as_ref()
                .or(existing.provider_config.as_ref())
                .ok_or_else(|| {
                    AdminError::InvalidPricing(format!(
                        "model '{model_name}': external models require providerConfig"
                    ))
                })?;
            inference_providers::non_attested::external::validate_external_provider_config(config)
                .map_err(|error| {
                    AdminError::InvalidPricing(format!("model '{model_name}': {error}"))
                })?;
        }

        let effective_is_active = request.is_active.unwrap_or(is_new_model);

        if effective_is_active {
            let effective_input = request
                .input_cost_per_token
                .unwrap_or(existing.input_cost_per_token);
            let effective_output = request
                .output_cost_per_token
                .unwrap_or(existing.output_cost_per_token);
            let effective_image = request.cost_per_image.unwrap_or(existing.cost_per_image);
            // Tri-state: absent = keep existing, explicit null = disabled (None),
            // value = that value. Disabled (None) and an explicit free price
            // (Some(0)) both contribute no revenue, so neither counts as
            // "priced" for the activation gate.
            let effective_cache_read = request
                .cache_read_cost_per_token
                .unwrap_or(existing.cache_read_cost_per_token);
            let effective_allow_free = request.allow_free.unwrap_or(existing.allow_free);

            if effective_input == 0
                && effective_output == 0
                && effective_image == 0
                && effective_cache_read.unwrap_or(0) == 0
                && !effective_allow_free
            {
                return Err(AdminError::InvalidPricing(format!(
                    "Cannot activate model '{}' with zero pricing. \
                     Set allowFree=true to explicitly allow free serving, \
                     or set a non-zero cost on at least one of: \
                     inputCostPerToken, outputCostPerToken, costPerImage, \
                     cacheReadCostPerToken.",
                    model_name
                )));
            }
        }

        Ok(())
    }

    pub(super) fn validate_organization_limits(
        limits: &OrganizationLimitsUpdate,
    ) -> Result<(), AdminError> {
        // All amounts use fixed scale 9 (nano-dollars) and USD - no scale/currency validation needed

        // With four API-supported credit types, this keeps every API-created
        // aggregate below i64::MAX and prevents oversized values from breaking
        // BIGINT sums used by admin listings. $1B remains effectively unlimited
        // for the contract-customer workflow while retaining a safety boundary.
        const MAX_SPEND_LIMIT_NANO_USD: i64 = 1_000_000_000_000_000_000;

        // Validate amount is non-negative
        if limits.spend_limit < 0 {
            return Err(AdminError::InvalidLimits(
                "Spend limit cannot be negative".to_string(),
            ));
        }

        if limits.spend_limit > MAX_SPEND_LIMIT_NANO_USD {
            return Err(AdminError::InvalidLimits(
                "Spend limit cannot exceed $1,000,000,000".to_string(),
            ));
        }

        Ok(())
    }
}
