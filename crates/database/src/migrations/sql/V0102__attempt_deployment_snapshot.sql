ALTER TABLE inference_attempts ADD COLUMN account_id UUID REFERENCES provider_accounts(id);
ALTER TABLE inference_attempts ADD COLUMN deployment_revision BIGINT CHECK(deployment_revision > 0);
