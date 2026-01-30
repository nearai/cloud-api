-- Composite indexes for cursor-based pagination on vector store list endpoints.
-- Pagination uses (created_at, id) ordering with after/before cursor parameters.

CREATE INDEX idx_vector_stores_pagination ON vector_stores(created_at, id);
CREATE INDEX idx_vsf_pagination ON vector_store_files(vector_store_id, created_at, id);
CREATE INDEX idx_vsfb_pagination ON vector_store_file_batches(vector_store_id, created_at, id);
