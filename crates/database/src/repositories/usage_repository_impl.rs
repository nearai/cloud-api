use crate::models::RecordUsageRequest;
use crate::repositories::OrganizationUsageRepository;
use chrono::{DateTime, Utc};
use services::usage::ports::{
    InferenceCost, InferenceUsageHistoryQuery, InferenceUsageReportQuery, InferenceUsageReportRow,
    OrganizationBalanceInfo, UsageByModelEntry, UsageLogEntry,
};
use uuid::Uuid;

/// Trait implementation adapter for UsageRepository
#[async_trait::async_trait]
impl services::usage::ports::UsageRepository for OrganizationUsageRepository {
    async fn release_budget(&self, organization_id: Uuid, id: Uuid) -> anyhow::Result<()> {
        let mut client = self.pool.get().await?;
        let tx = client.transaction().await?;
        tx.batch_execute("SET LOCAL statement_timeout = '10s'; SET LOCAL lock_timeout = '5s'")
            .await?;
        tx.query_one(
            "SELECT release_usage_budget($1,$2)",
            &[&organization_id, &id],
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }
    async fn expire_budgets(&self) -> anyhow::Result<u64> {
        let mut expired = 0;
        let organizations = self
            .pool
            .get()
            .await?
            .query(
                "SELECT organization_id FROM usage_budget_reservations
             WHERE state = 'active' AND expires_at <= clock_timestamp()
             GROUP BY organization_id ORDER BY MIN(expires_at),organization_id LIMIT 100",
                &[],
            )
            .await?;
        for row in organizations {
            let mut client = self.pool.get().await?;
            let tx = client.transaction().await?;
            tx.batch_execute("SET LOCAL statement_timeout = '10s'; SET LOCAL lock_timeout = '5s'")
                .await?;
            let released: i64 = tx
                .query_one("SELECT expire_usage_budgets($1)", &[&row.get::<_, Uuid>(0)])
                .await?
                .get(0);
            tx.commit().await?;
            expired += u64::try_from(released)?;
        }
        Ok(expired)
    }

    async fn admit_budget(
        &self,
        r: &services::usage::budget::BudgetReservation,
    ) -> Result<(), services::usage::UsageError> {
        use services::usage::UsageError;
        let result: anyhow::Result<()> = async {
            let mut client = self.pool.get().await?;
            let tx = client.transaction().await?;
            tx.batch_execute("SET LOCAL statement_timeout = '10s'; SET LOCAL lock_timeout = '5s'")
                .await?;
            tx.query_one(
                "SELECT admit_usage_budget($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13)",
                &[
                    &r.id,
                    &r.identity.organization_id,
                    &r.identity.workspace_id,
                    &r.identity.api_key_id,
                    &r.identity.model_id,
                    &r.amount,
                    &r.inference_type.as_str(),
                    &i64::from(r.estimator_version),
                    &self.allocation_policy.priority,
                    &self.allocation_policy.version,
                    &r.organization_max_unsettled,
                    &r.key_max_unsettled,
                    &r.reservation_ttl_seconds,
                ],
            )
            .await?;
            tx.commit().await?;
            Ok(())
        }
        .await;
        result.map_err(|error| {
            match error
                .downcast_ref::<tokio_postgres::Error>()
                .and_then(|e| e.code())
                .map(|c| c.code())
            {
                Some("NBKEX") => UsageError::LimitExceeded(
                    "API key spend limit exceeded. Insufficient available budget for this request."
                        .into(),
                ),
                Some("NBOEX") => UsageError::LimitExceeded(
                    "Credit limit exceeded. Insufficient available budget for this request.".into(),
                ),
                Some("NBBUS") => UsageError::CapacityExceeded,
                Some("NBORG" | "NBKEY") => {
                    UsageError::Unauthorized("Billing identity is unavailable".into())
                }
                _ => UsageError::InternalError(format!("Budget admission failed: {error}")),
            }
        })
    }

    async fn record_usage(
        &self,
        request: services::usage::ports::RecordUsageDbRequest,
    ) -> anyhow::Result<UsageLogEntry> {
        let db_request = RecordUsageRequest {
            organization_id: request.organization_id,
            workspace_id: request.workspace_id,
            api_key_id: request.api_key_id,
            model_id: request.model_id,
            model_name: request.model_name,
            input_tokens: request.input_tokens,
            output_tokens: request.output_tokens,
            cache_read_tokens: request.cache_read_tokens,
            cache_write_tokens: request.cache_write_tokens,
            billing_details: request.billing_details,
            service_tier: request.service_tier,
            context_band: request.context_band,
            input_cost: request.input_cost,
            output_cost: request.output_cost,
            total_cost: request.total_cost,
            inference_type: request.inference_type.to_string(),
            ttft_ms: request.ttft_ms,
            avg_itl_ms: request.avg_itl_ms,
            inference_id: request.inference_id,
            provider_request_id: request.provider_request_id,
            stop_reason: request.stop_reason,
            response_id: request.response_id,
            image_count: request.image_count,
            served_provider_tier: request.provider_attribution.served_provider_tier,
            served_provider_type: request.provider_attribution.served_provider_type,
            served_via_fallback: request.provider_attribution.served_via_fallback,
            dedupe_on_conflict: request.dedupe_on_conflict,
        };

        let log = self.record_usage(db_request).await?;

        Ok(UsageLogEntry {
            id: log.id,
            organization_id: log.organization_id,
            workspace_id: log.workspace_id,
            api_key_id: log.api_key_id,
            model_id: log.model_id,
            model: log.model,
            input_tokens: log.input_tokens,
            output_tokens: log.output_tokens,
            cache_read_tokens: log.cache_read_tokens,
            cache_write_tokens: log.cache_write_tokens,
            billing_details: log.billing_details,
            service_tier: log.service_tier,
            context_band: log.context_band,
            total_tokens: log.total_tokens,
            input_cost: log.input_cost,
            output_cost: log.output_cost,
            total_cost: log.total_cost,
            inference_type: log
                .inference_type
                .parse()
                .unwrap_or(services::usage::ports::InferenceType::ChatCompletion),
            created_at: log.created_at,
            ttft_ms: log.ttft_ms,
            avg_itl_ms: log.avg_itl_ms,
            inference_id: log.inference_id,
            provider_request_id: log.provider_request_id,
            stop_reason: log.stop_reason,
            response_id: log.response_id,
            image_count: log.image_count,
            was_inserted: log.was_inserted,
            credit_allocations: log.credit_allocations,
            funded_amount: log.funded_amount,
            unfunded_amount: log.unfunded_amount,
            allocation_policy_version: log.allocation_policy_version,
            provider_attribution: services::usage::ProviderAttribution {
                served_provider_tier: log.served_provider_tier,
                served_provider_type: log.served_provider_type,
                served_via_fallback: log.served_via_fallback,
            },
        })
    }

    async fn get_balance(
        &self,
        organization_id: Uuid,
    ) -> anyhow::Result<Option<OrganizationBalanceInfo>> {
        let balance = self.get_balance(organization_id).await?;

        Ok(balance.map(|b| OrganizationBalanceInfo {
            organization_id: b.organization_id,
            total_spent: b.total_spent,
            legacy_unattributed_amount: b.legacy_unattributed_amount,
            last_usage_at: b.last_usage_at,
            total_requests: b.total_requests,
            total_tokens: b.total_tokens,
            updated_at: b.updated_at,
        }))
    }

    async fn get_usage_history(
        &self,
        organization_id: Uuid,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> anyhow::Result<(Vec<UsageLogEntry>, i64)> {
        let (logs, total) = self
            .get_usage_history(organization_id, limit, offset)
            .await?;

        let entries = logs
            .into_iter()
            .map(|log| UsageLogEntry {
                id: log.id,
                organization_id: log.organization_id,
                workspace_id: log.workspace_id,
                api_key_id: log.api_key_id,
                model_id: log.model_id,
                model: log.model,
                input_tokens: log.input_tokens,
                output_tokens: log.output_tokens,
                cache_read_tokens: log.cache_read_tokens,
                cache_write_tokens: log.cache_write_tokens,
                billing_details: log.billing_details,
                service_tier: log.service_tier,
                context_band: log.context_band,
                total_tokens: log.total_tokens,
                input_cost: log.input_cost,
                output_cost: log.output_cost,
                total_cost: log.total_cost,
                inference_type: log
                    .inference_type
                    .parse()
                    .unwrap_or(services::usage::ports::InferenceType::ChatCompletion),
                created_at: log.created_at,
                ttft_ms: log.ttft_ms,
                avg_itl_ms: log.avg_itl_ms,
                inference_id: log.inference_id,
                provider_request_id: log.provider_request_id,
                stop_reason: log.stop_reason,
                response_id: log.response_id,
                image_count: log.image_count,
                was_inserted: true,
                credit_allocations: log.credit_allocations,
                funded_amount: log.funded_amount,
                unfunded_amount: log.unfunded_amount,
                allocation_policy_version: log.allocation_policy_version,
                provider_attribution: services::usage::ProviderAttribution {
                    served_provider_tier: log.served_provider_tier,
                    served_provider_type: log.served_provider_type,
                    served_via_fallback: log.served_via_fallback,
                },
            })
            .collect();

        Ok((entries, total))
    }

    async fn get_usage_history_by_api_key(
        &self,
        api_key_id: Uuid,
        credit_type: Option<&str>,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> anyhow::Result<(Vec<UsageLogEntry>, i64)> {
        let logs = self
            .get_usage_history_by_api_key(api_key_id, credit_type, limit, offset)
            .await?;

        let total = self
            .count_usage_history_by_api_key(api_key_id, credit_type)
            .await?;

        let entries = logs
            .into_iter()
            .map(|log| UsageLogEntry {
                id: log.id,
                organization_id: log.organization_id,
                workspace_id: log.workspace_id,
                api_key_id: log.api_key_id,
                model_id: log.model_id,
                model: log.model,
                input_tokens: log.input_tokens,
                output_tokens: log.output_tokens,
                cache_read_tokens: log.cache_read_tokens,
                cache_write_tokens: log.cache_write_tokens,
                billing_details: log.billing_details,
                service_tier: log.service_tier,
                context_band: log.context_band,
                total_tokens: log.total_tokens,
                input_cost: log.input_cost,
                output_cost: log.output_cost,
                total_cost: log.total_cost,
                inference_type: log
                    .inference_type
                    .parse()
                    .unwrap_or(services::usage::ports::InferenceType::ChatCompletion),
                created_at: log.created_at,
                ttft_ms: log.ttft_ms,
                avg_itl_ms: log.avg_itl_ms,
                inference_id: log.inference_id,
                provider_request_id: log.provider_request_id,
                stop_reason: log.stop_reason,
                response_id: log.response_id,
                image_count: log.image_count,
                was_inserted: true,
                credit_allocations: log.credit_allocations,
                funded_amount: log.funded_amount,
                unfunded_amount: log.unfunded_amount,
                allocation_policy_version: log.allocation_policy_version,
                provider_attribution: services::usage::ProviderAttribution {
                    served_provider_tier: log.served_provider_tier,
                    served_provider_type: log.served_provider_type,
                    served_via_fallback: log.served_via_fallback,
                },
            })
            .collect();

        Ok((entries, total))
    }

    async fn get_api_key_spend(&self, api_key_id: Uuid) -> anyhow::Result<i64> {
        self.get_api_key_spend(api_key_id).await
    }

    async fn get_costs_by_inference_ids(
        &self,
        organization_id: Uuid,
        inference_ids: Vec<Uuid>,
    ) -> anyhow::Result<Vec<InferenceCost>> {
        self.get_costs_by_inference_ids(organization_id, inference_ids)
            .await
    }

    async fn get_stop_reason_by_response_id(
        &self,
        response_id: Uuid,
    ) -> anyhow::Result<Option<services::usage::StopReason>> {
        self.get_stop_reason_by_response_id(response_id).await
    }

    async fn get_stop_reason_by_provider_request_id(
        &self,
        provider_request_id: &str,
    ) -> anyhow::Result<Option<services::usage::StopReason>> {
        self.get_stop_reason_by_provider_request_id(provider_request_id)
            .await
    }

    async fn get_usage_by_model(
        &self,
        organization_id: Uuid,
        start_date: DateTime<Utc>,
        end_date: Option<DateTime<Utc>>,
    ) -> anyhow::Result<Vec<UsageByModelEntry>> {
        let rows = self
            .get_usage_by_model_since(organization_id, start_date, end_date)
            .await?;

        Ok(rows
            .into_iter()
            .map(|r| UsageByModelEntry {
                model: r.model,
                input_tokens: r.input_tokens,
                output_tokens: r.output_tokens,
                total_tokens: r.total_tokens,
                total_cost: r.total_cost,
                request_count: r.request_count,
            })
            .collect())
    }

    async fn list_inference_usage_report(
        &self,
        query: InferenceUsageReportQuery,
    ) -> anyhow::Result<Vec<InferenceUsageReportRow>> {
        self.list_inference_usage_report(query).await
    }

    async fn list_inference_usage_history(
        &self,
        query: InferenceUsageHistoryQuery,
    ) -> anyhow::Result<(Vec<InferenceUsageReportRow>, i64)> {
        self.list_inference_usage_history(query).await
    }
}
