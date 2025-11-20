-- V24: Add owned_by field to models table
-- This migration adds an owned_by field to track which entity owns/manages the model

-- Add owned_by column to models table
ALTER TABLE models
ADD COLUMN owned_by TEXT NOT NULL;

-- Backfill all existing models with 'nearai'
UPDATE models
SET owned_by = 'nearai';
