# Build stage (rust 1.90-bookworm)
ARG RUST_IMAGE_SHA
FROM rust:1.90-bookworm@${RUST_IMAGE_SHA} AS builder

# ARG to receive the timestamp from the CLI
ARG SOURCE_DATE_EPOCH
# ENV to set it for cargo/rustc
ENV SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH}

COPY pinned-packages.txt /tmp/

RUN set -e; \
    # Create a sources.list file pointing to a specific snapshot
    echo 'deb [check-valid-until=no] https://snapshot.debian.org/archive/debian/20250411T024939Z bookworm main' > /etc/apt/sources.list && \
    echo 'deb [check-valid-until=no] https://snapshot.debian.org/archive/debian-security/20250411T024939Z bookworm-security main' >> /etc/apt/sources.list && \
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
    apt-get install -y --no-install-recommends \
        openssl \
        bash \
        python3-pip \
        python3-requests \
        python3.11 \
        python3.11-venv \
        jq \
        ca-certificates \
        libssl3 \
        curl \
        coreutils && \
        rm -rf /var/lib/apt/lists/* /var/log/* /var/cache/ldconfig/aux-cache /tmp/pinned-packages.txt

# Set the working directory
WORKDIR /app

# Copy workspace files
COPY Cargo.toml Cargo.lock ./
COPY crates/ ./crates/

# Build the application in release mode
RUN cargo build --release --bin api

# Runtime stage
FROM debian:bookworm-slim

RUN set -e; \
    # Create a sources.list file pointing to a specific snapshot
    echo 'deb [check-valid-until=no] https://snapshot.debian.org/archive/debian/20250411T024939Z bookworm main' > /etc/apt/sources.list && \
    echo 'deb [check-valid-until=no] https://snapshot.debian.org/archive/debian-security/20250411T024939Z bookworm-security main' >> /etc/apt/sources.list && \
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
    apt-get install -y --no-install-recommends \
        ca-certificates \
        libssl3 \
        curl \
        && rm -rf /var/lib/apt/lists/* /var/log/* /var/cache/ldconfig/aux-cache /tmp/pinned-packages.txt

# Create app user
RUN useradd -m -u 1000 app

# Create app directory
WORKDIR /app

# Copy the built binary
COPY --from=builder /app/target/release/api /app/api

COPY .GIT_REV /etc/

# Change ownership to app user
RUN chown -R app:app /app

# Switch to app user
USER app

# Expose the port
EXPOSE 3000

# Run the application
CMD ["./api"]
