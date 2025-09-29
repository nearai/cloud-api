-- Database initialization script for platform-api
-- This script runs automatically when PostgreSQL starts in Docker

-- Create UUID extension if not exists
CREATE EXTENSION IF NOT EXISTS "uuid-ossp";

-- Note: The application will run migrations automatically on startup
-- If you want to run migrations manually, use the refinery CLI or run the app once