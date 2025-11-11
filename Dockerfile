# Build stage
FROM rust:1.88.0-bookworm@sha256:af306cfa71d987911a781c37b59d7d67d934f49684058f96cf72079c3626bfe0 AS builder

# Install pinned apt dependencies
RUN --mount=type=bind,source=pinned-packages-builder.txt,target=/tmp/pinned-packages-builder.txt,ro \
    set -e; \
    # Create a sources.list file pointing to a specific snapshot
    echo 'deb [check-valid-until=no] https://snapshot.debian.org/archive/debian/20250411T024939Z bookworm main' > /etc/apt/sources.list && \
    echo 'deb [check-valid-until=no] https://snapshot.debian.org/archive/debian-security/20250411T024939Z bookworm-security main' >> /etc/apt/sources.list && \
    echo 'Acquire::Check-Valid-Until "false";' > /etc/apt/apt.conf.d/10no-check-valid-until && \
    # Create preferences file to pin all packages
    rm -rf /etc/apt/sources.list.d/debian.sources && \
    mkdir -p /etc/apt/preferences.d && \
    cat /tmp/pinned-packages-builder.txt | while read line; do \
        pkg=$(echo $line | cut -d= -f1); \
        ver=$(echo $line | cut -d= -f2); \
        if [ ! -z "$pkg" ] && [ ! -z "$ver" ]; then \
            printf "Package: %s\nPin: version %s\nPin-Priority: 1001\n\n" "$pkg" "$ver" >> /etc/apt/preferences.d/pinned-packages; \
        fi; \
    done && \
    apt-get update && \
    apt-get install -y --no-install-recommends \
        pkg-config \
        libssl-dev \
        && rm -rf /var/lib/apt/lists/* /var/log/* /var/cache/ldconfig/aux-cache

# Set the working directory
WORKDIR /app

# Fetch the latest pinned package list
RUN dpkg -l | grep '^ii' | awk '{print $2"="$3}' | sort > ./pinned-packages-builder.txt

# Copy workspace files
COPY Cargo.toml Cargo.lock ./
COPY crates/ ./crates/
COPY .cargo/ ./.cargo/

# Build the application in release mode
RUN cargo build --release --locked --bin api


# Runtime stage
FROM debian:bookworm-slim@sha256:78d2f66e0fec9e5a39fb2c72ea5e052b548df75602b5215ed01a17171529f706 AS runtime

# Bootstrap by installing ca-certificates which will be overridden by the pinned packages.
# Otherwise the source list cannot be fetched from the debian snapshot.
RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* /var/log/* /var/cache/ldconfig/aux-cache

# Install pinned apt dependencies
RUN --mount=type=bind,source=pinned-packages-runtime.txt,target=/tmp/pinned-packages-runtime.txt,ro \
    set -e; \
    # Create a sources.list file pointing to a specific snapshot
    echo 'deb [check-valid-until=no] https://snapshot.debian.org/archive/debian/20250411T024939Z bookworm main' > /etc/apt/sources.list && \
    echo 'deb [check-valid-until=no] https://snapshot.debian.org/archive/debian-security/20250411T024939Z bookworm-security main' >> /etc/apt/sources.list && \
    echo 'Acquire::Check-Valid-Until "false";' > /etc/apt/apt.conf.d/10no-check-valid-until && \
    # Create preferences file to pin all packages
    rm -rf /etc/apt/sources.list.d/debian.sources && \
    mkdir -p /etc/apt/preferences.d && \
    cat /tmp/pinned-packages-runtime.txt | while read line; do \
        pkg=$(echo $line | cut -d= -f1); \
        ver=$(echo $line | cut -d= -f2); \
        if [ ! -z "$pkg" ] && [ ! -z "$ver" ]; then \
            printf "Package: %s\nPin: version %s\nPin-Priority: 1001\n\n" "$pkg" "$ver" >> /etc/apt/preferences.d/pinned-packages; \
        fi; \
    done && \
    apt-get update && \
    apt-get install -y --no-install-recommends \
        ca-certificates \
        libssl3 \
        curl \
        && rm -rf /var/lib/apt/lists/* /var/log/* /var/cache/ldconfig/aux-cache

# Create app user
# Normalize /etc/shadow file last password change date to 0 for app user
RUN useradd -m -u 1000 app \
    && sed -i -r 's/^(app:[^:]*:)[0-9]+/\10/' /etc/shadow

# Create app directory
WORKDIR /app

# Copy the built binary
COPY --from=builder /app/target/release/api /app/api

# Copy the migration SQL files
RUN mkdir -p /app/crates/database/src/migrations/sql
COPY --from=builder --chmod=0664 /app/crates/database/src/migrations/sql/*.sql /app/crates/database/src/migrations/sql/

# Copy the pinned package list from builder stage
COPY --from=builder --chmod=0664 /app/pinned-packages-builder.txt /app/pinned-packages-builder.txt

# Change ownership to app user
RUN chown -R app:app /app

# Switch to app user
USER app

# Expose the port
EXPOSE 3000

# Run the application
CMD ["./api"]
