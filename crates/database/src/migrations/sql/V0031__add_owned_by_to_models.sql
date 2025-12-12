-- Add owned_by field to models table
-- This migration adds an owned_by field to track which entity owns/manages the model

-- Add owned_by column to models table with default value
-- DEFAULT applies to existing rows during migration and to future direct SQL inserts
ALTER TABLE models
ADD COLUMN owned_by TEXT NOT NULL DEFAULT 'nearai';
