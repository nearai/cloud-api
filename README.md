# NEAR AI Cloud API

A Rust-based cloud API for AI model inference, conversation management, and organization administration. Part of the NEAR AI platform alongside the Chat API.

## Quick Start

### Prerequisites

- **Rust** (latest stable version)
- **Docker & Docker Compose** (for local development)
- **PostgreSQL** (for production or local testing without Docker)

### Development Setup

1. **Clone the repository**:
   ```bash
   git clone <repository-url>
   cd cloud-api
   ```

2. **Start services with Docker Compose**:
   ```bash
   docker-compose up -d
   ```

   This starts:
   - PostgreSQL database on port 5432
   - NEAR AI Cloud API on port 3000

3. **Run without Docker**:
   ```bash
   # Set up PostgreSQL database first, then:
   cargo run --bin api
   ```

## Testing

### Prerequisites for Testing

- PostgreSQL database running
- Database must be accessible with the credentials specified in test configuration

### Run Tests

```bash
# Run unit tests
cargo test --lib --bins

# Run integration/e2e tests (requires database)
cargo test --test e2e_test
```

### Test Database Setup

#### Option 1: Using Docker (Recommended)
```bash
# Start test database
docker run --name test-postgres \
  -e POSTGRES_PASSWORD=postgres \
  -e POSTGRES_DB=platform_api \
  -p 5432:5432 \
  -d postgres:latest

# Run tests
TEST_DB_HOST=localhost \
TEST_DB_PORT=5432 \
TEST_DB_NAME=platform_api \
TEST_DB_USER=postgres \
TEST_DB_PASSWORD=postgres \
cargo test --test e2e_test
```

#### Option 2: Using existing PostgreSQL
Set these environment variables before running tests:
```bash
export TEST_DB_HOST=localhost
export TEST_DB_PORT=5432
export TEST_DB_NAME=platform_api_test  # Use a dedicated test database
export TEST_DB_USER=your_username
export TEST_DB_PASSWORD=your_password
```

### Test Database Verification

You can verify your test database setup using the provided script:
```bash
./scripts/test-db-setup.sh
```

## Configuration

The application uses YAML configuration files located in the `config/` directory.

## Contributing

1. Ensure all tests pass: `cargo test`
2. Check code formatting: `cargo fmt --check`
3. Run linting: `cargo clippy`
4. Ensure database migrations work with test setup
