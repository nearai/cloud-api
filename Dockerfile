# Build stage (rust 1.90-bookworm)
ARG RUST_IMAGE_SHA
FROM rust:1.90-bookworm@${RUST_IMAGE_SHA} AS builder

# ARG to receive the timestamp from the CLI
ARG SOURCE_DATE_EPOCH
# ENV to set it for cargo/rustc and reproducibility
ENV SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH} \
    CARGO_HOME=/cargo \
    PYTHONDONTWRITEBYTECODE=1 \
    CARGO_BUILD_JOBS=1

COPY pinned-packages.txt /tmp/

RUN set -e; \
    # Create a sources.list file pointing to a specific snapshot
    echo 'deb [check-valid-until=no] http://snapshot.debian.org/archive/debian/20251022T143047Z bookworm main' > /etc/apt/sources.list && \
    echo 'deb [check-valid-until=no] http://snapshot.debian.org/archive/debian-security/20251022T143047Z bookworm-security main' >> /etc/apt/sources.list && \
    echo 'Acquire::Check-Valid-Until "false";' > /etc/apt/apt.conf.d/10no-check-valid-until && \
    # Create preferences file to pin all packages
    rm -rf /etc/apt/sources.list.d/debian.sources && \
    mkdir -p /etc/apt/preferences.d && \
    cat /tmp/pinned-packages.txt | while read line; do \
        pkg=$(echo $line | cut -d= -f1); \
        ver=$(echo $line | cut -d= -f2); \
        if [ ! -z "$pkg" ] && [ ! -z "$ver" ]; then \
            echo "Package: $pkg\nPin: version $ver\nPin-Priority: 1001\n" >> /etc/apt/preferences.d/pinned-packages; \
        fi; \
    done && \
    apt-get update && \
    DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        bash \
        ca-certificates \
        coreutils \
        curl \
        jq \
        libssl3 \
        llvm \
        openssl \
        python3-pip \
        python3-requests \
        python3.11 \
        python3.11-venv \
        && rm -rf /var/lib/apt/lists/* /var/log/* /var/cache/ldconfig/aux-cache /tmp/pinned-packages.txt && \
    # Normalize timestamps in system directories for reproducibility
    find /usr /lib /etc /var -type f -exec touch -t 197001010000.00 {} + 2>/dev/null || true && \
    find /usr /lib /etc /var -type d -exec touch -t 197001010000.00 {} + 2>/dev/null || true

# Set the working directory
WORKDIR /app

# Copy workspace files
COPY Cargo.toml Cargo.lock ./
COPY crates/ ./crates/
COPY .cargo/ ./.cargo/

# Normalize timestamps on all copied files for reproducibility
RUN find /app -exec touch -t 197001010000.00 {} +

# Normalize Rust toolchain timestamps
RUN find /usr/local/cargo /usr/local/rustup -type f -exec touch -t 197001010000.00 {} + 2>/dev/null || true

# Build the application in release mode
RUN cargo build --release --locked --bin api && \
    llvm-strip --strip-all /app/target/release/api && \
    # Normalize cargo registry and build artifacts
    find /cargo -type f -exec touch -t 197001010000.00 {} + 2>/dev/null || true && \
    find /app/target -type f -exec touch -t 197001010000.00 {} + 2>/dev/null || true

# Runtime stage
FROM debian:bookworm@sha256:26f2a7cab45014541c65f9d140ccfa6aaefbb49686c6759bea9c6f7f5bb3d72f

COPY pinned-packages.txt /tmp/

RUN set -e; \
    # Create a sources.list file pointing to a specific snapshot
    echo 'deb [check-valid-until=no] http://snapshot.debian.org/archive/debian/20251022T143047Z bookworm main' > /etc/apt/sources.list && \
    echo 'deb [check-valid-until=no] http://snapshot.debian.org/archive/debian-security/20251022T143047Z bookworm-security main' >> /etc/apt/sources.list && \
    echo 'Acquire::Check-Valid-Until "false";' > /etc/apt/apt.conf.d/10no-check-valid-until && \
    # Create preferences file to pin all packages
    rm -rf /etc/apt/sources.list.d/debian.sources && \
    mkdir -p /etc/apt/preferences.d && \
    cat /tmp/pinned-packages.txt | while read line; do \
        pkg=$(echo $line | cut -d= -f1); \
        ver=$(echo $line | cut -d= -f2); \
        if [ ! -z "$pkg" ] && [ ! -z "$ver" ]; then \
            echo "Package: $pkg\nPin: version $ver\nPin-Priority: 1001\n" >> /etc/apt/preferences.d/pinned-packages; \
        fi; \
    done && \
    apt-get update && \
    DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
        libssl3 \
        && rm -rf /var/lib/apt/lists/* /var/log/* /var/cache/ldconfig/aux-cache /tmp/pinned-packages.txt && \
    # Normalize timestamps in system directories
    find /usr /lib /etc /var -type f -exec touch -t 197001010000.00 {} + 2>/dev/null || true && \
    find /usr /lib /etc /var -type d -exec touch -t 197001010000.00 {} + 2>/dev/null || true

# Create app user deterministically with manual entries
RUN echo 'app:x:10000:10000:Application User:/home/app:/bin/bash' >> /etc/passwd && \
    echo 'app:x:10000:' >> /etc/group && \
    echo 'app:!:19000:0:99999:7:::' >> /etc/shadow && \
    mkdir -p /home/app && \
    chown 10000:10000 /home/app && \
    chmod 755 /home/app && \
    touch -t 197001010000.00 /etc/passwd /etc/group /etc/shadow /home/app

# Create app directory
WORKDIR /app

# Copy the built binary
COPY --from=builder /app/target/release/api /app/api

COPY .GIT_REV /etc/

# Normalize timestamps on copied files
RUN find /app /etc/.GIT_REV -exec touch -t 197001010000.00 {} + && \
    chmod 755 /app/api && \
    chown -R 10000:10000 /app

# Switch to app user
USER app

# Expose the port
EXPOSE 3000

# Run the application
CMD ["./api"]
