-- Database setup script for platform-api
-- Run this to create the database

-- Create database (run as superuser)
CREATE DATABASE platform_api;

-- Connect to the database
\c platform_api;

-- Create UUID extension if not exists
CREATE EXTENSION IF NOT EXISTS "uuid-ossp";

-- Note: The application will run migrations automatically on startup
-- If you want to run migrations manually, use the refinery CLI or run the app once