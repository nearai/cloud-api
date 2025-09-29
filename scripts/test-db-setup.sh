#!/bin/bash
# Test database setup script for CI/local testing
set -e

echo "Setting up test database..."

# Environment variables with defaults
DB_HOST=${TEST_DB_HOST:-localhost}
DB_PORT=${TEST_DB_PORT:-5432}
DB_NAME=${TEST_DB_NAME:-platform_api}
DB_USER=${TEST_DB_USER:-postgres}
DB_PASSWORD=${TEST_DB_PASSWORD:-postgres}

echo "Database connection info:"
echo "  Host: $DB_HOST"
echo "  Port: $DB_PORT"
echo "  Database: $DB_NAME"
echo "  User: $DB_USER"

# Wait for PostgreSQL to be ready
echo "Waiting for PostgreSQL to be ready..."
until PGPASSWORD="$DB_PASSWORD" pg_isready -h "$DB_HOST" -p "$DB_PORT" -U "$DB_USER"; do
  echo "PostgreSQL is not ready yet, waiting..."
  sleep 2
done

echo "PostgreSQL is ready!"

# Test database connection
echo "Testing database connection..."
PGPASSWORD="$DB_PASSWORD" psql -h "$DB_HOST" -U "$DB_USER" -d "$DB_NAME" -c "SELECT version();" > /dev/null

echo "Database connection successful!"

# Run setup script if it exists
if [ -f "scripts/setup_db.sql" ]; then
    echo "Running database setup script..."
    PGPASSWORD="$DB_PASSWORD" psql -h "$DB_HOST" -U "$DB_USER" -d "$DB_NAME" -f scripts/setup_db.sql
    echo "Database setup script completed!"
else
    echo "No setup script found at scripts/setup_db.sql"
fi

echo "Database setup verification completed successfully!"
