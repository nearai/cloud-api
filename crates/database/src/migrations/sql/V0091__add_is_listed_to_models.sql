-- Add a catalog-visibility switch to the models table.
--
-- Some models are served only through a gateway lane that sits in front of
-- the inference fleet. They still need an active, priced row here: the lane
-- reads its pricing and limits from the catalog and reports usage through
-- `POST /v1/internal/usage`, which bills against that row. Until now an
-- active row was always published on the public catalog endpoints
-- (`GET /v1/models`, `GET /v1/model/list`, `GET /v1/model/{name}`), so such a
-- model could not exist without being advertised to every caller.
--
-- `is_listed = false` keeps an active row out of those public endpoints. It is
-- a catalog switch only: model resolution, routing and usage recording are
-- unchanged. Trusted services read the full catalog, unlisted rows included,
-- from `GET /v1/internal/models`. Existing rows stay listed.
ALTER TABLE models ADD COLUMN is_listed BOOLEAN NOT NULL DEFAULT TRUE;

-- Mirror the column onto model_history so admin edits are audited the same
-- way as the other catalog fields (V0057, V0065 precedent). Rows written
-- before this migration read as listed, which is what they were.
ALTER TABLE model_history ADD COLUMN is_listed BOOLEAN NOT NULL DEFAULT TRUE;
