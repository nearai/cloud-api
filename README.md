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

Tests use the same environment variables as the main application (from `env.example`).
If environment variables are not set, tests use sensible defaults for local testing.

#### Option 1: Using Docker (Recommended)
```bash
# Start test database
docker run --name test-postgres \
  -e POSTGRES_PASSWORD=postgres \
  -e POSTGRES_DB=platform_api \
  -p 5432:5432 \
  -d postgres:latest

# Run tests with default values (or override with env vars)
cargo test --test e2e_test

# Or with custom database settings
DATABASE_HOST=localhost \
DATABASE_PORT=5432 \
DATABASE_NAME=platform_api \
DATABASE_USERNAME=postgres \
DATABASE_PASSWORD=postgres \
cargo test --test e2e_test
```

#### Option 2: Using existing PostgreSQL
Set these environment variables before running tests:
```bash
export DATABASE_HOST=localhost
export DATABASE_PORT=5432
export DATABASE_NAME=platform_api_test  # Use a dedicated test database
export DATABASE_USERNAME=your_username
export DATABASE_PASSWORD=your_password
export DATABASE_MAX_CONNECTIONS=5
export DATABASE_TLS_ENABLED=false
```

#### Option 3: Using .env file
Copy `env.example` to `.env` and configure your test database:
```bash
cp env.example .env
# Edit .env with your database credentials
cargo test --test e2e_test
```

### vLLM Integration Tests

The vLLM integration tests require a running vLLM instance. Configure using environment variables:

```bash
# Configure vLLM endpoint
export VLLM_BASE_URL=http://localhost:8002
export VLLM_API_KEY=your_vllm_api_key_here  # Optional
export VLLM_TEST_TIMEOUT_SECS=30  # Optional

# Run vLLM integration tests
cargo test --test integration_tests
```

If not set, tests will use default values but may fail if vLLM is not running at the default URL.

## Configuration

The application uses YAML configuration files located in the `config/` directory.

## Contributing

1. Ensure all tests pass: `cargo test`
2. Check code formatting: `cargo fmt --check`
3. Run linting: `cargo clippy`
4. Ensure database migrations work with test setup


## License 

Licensed under the [PolyForm Strict License 1.0.0](LICENSE).