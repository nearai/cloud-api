CREATE TABLE api_key_model_affinity (
  api_key_id UUID NOT NULL,
  model_name TEXT NOT NULL,
  provider_url TEXT NOT NULL,
  expires_at TIMESTAMPTZ NOT NULL,
  updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  PRIMARY KEY (api_key_id, model_name)
);
