# Build stage
FROM rust:1.88.0-bookworm@sha256:af306cfa71d987911a781c37b59d7d67d934f49684058f96cf72079c3626bfe0 AS builder

ARG SOURCE_DATE_EPOCH
ENV SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH}

# Install build dependencies
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/* /var/log/* /var/cache/ldconfig/aux-cache

# Set the working directory
WORKDIR /app

# Copy workspace files
COPY Cargo.toml Cargo.lock ./
COPY crates/ ./crates/
COPY .cargo/ ./.cargo/

# Build the application in release mode
RUN cargo build --release --locked --bin api


# Runtime stage
FROM debian:bookworm-slim@sha256:78d2f66e0fec9e5a39fb2c72ea5e052b548df75602b5215ed01a17171529f706 AS runtime

# Install runtime dependencies
RUN apt-get update && apt-get install -y \
    ca-certificates \
    libssl3 \
    curl \
    && rm -rf /var/lib/apt/lists/* /var/log/* /var/cache/ldconfig/aux-cache

# Create app user
RUN useradd -m -u 1000 app

# Create app directory
WORKDIR /app

# Copy the built binary
COPY --from=builder /app/target/release/api /app/api

# Copy the migration SQL files
RUN mkdir -p /app/crates/database/src/migrations/sql
COPY --chmod=0664 --from=builder /app/crates/database/src/migrations/sql/*.sql /app/crates/database/src/migrations/sql/

# Change ownership to app user
RUN chown -R app:app /app

# Switch to app user
USER app

# Expose the port
EXPOSE 3000

# Run the application
CMD ["./api"]
