# Locally assembled digest; publication and anonymous pull verification pending.
FROM nearaidev/apt-bundle-cloud-api@sha256:ecfed6536d176857f0c7f3199fac4b6cdba449a9155e8820817b8479351d522e AS apt-bundle
FROM scratch AS apt-bundle-files
COPY --from=apt-bundle /apt-bundle /apt-bundle

# Build stage
FROM rust:1.92.0-bookworm@sha256:9676d0547a259997add8f5924eb6b959c589ed39055338e23b99aba7958d6d31 AS builder-base

FROM builder-base AS builder

# Install pinned apt dependencies
RUN --network=none \
    --mount=type=bind,from=apt-bundle-files,source=/apt-bundle,target=/opt/apt-bundle,ro \
    --mount=type=bind,source=build/apt-bundle/prepare-apt.sh,target=/run/prepare-apt.sh,ro \
    --mount=type=bind,source=pinned-packages-builder.txt,target=/tmp/pinned-packages-builder.txt,ro \
    set -e; \
    bash /run/prepare-apt.sh --bundle /opt/apt-bundle --snapshot 20260928T000000Z \
        --config /tmp/apt-bundle.conf --debian-keyring /usr/share/keyrings/debian-archive-keyring.gpg \
        --status /var/lib/dpkg/status --pins /tmp/pinned-packages-builder.txt && \
    if [ -f /var/lib/apt/extended_states ]; then \
        cp /var/lib/apt/extended_states /tmp/apt-bundle.conf.d/state/extended_states; \
    fi && \
    export APT_CONFIG=/tmp/apt-bundle.conf && \
    apt-get update && \
    apt-get install -y --no-install-recommends \
        pkg-config \
        libssl-dev \
        && cp /tmp/apt-bundle.conf.d/state/extended_states /var/lib/apt/extended_states \
        && rm -rf /tmp/apt-bundle.conf /tmp/apt-bundle.conf.d \
            /var/lib/apt/lists/* /var/log/* /var/cache/ldconfig/aux-cache

# Set the working directory
WORKDIR /app

# Fetch the latest pinned package list
RUN dpkg -l | grep '^ii' | awk '{print $2"="$3}' | sort > ./pinned-packages-builder.txt

# Set the source date epoch to 0 to avoid timestamp changes
ARG SOURCE_DATE_EPOCH=0
ENV SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH}

# Copy workspace files
COPY Cargo.toml Cargo.lock ./
COPY crates/ ./crates/
COPY vendor/ ./vendor/
COPY .cargo/ ./.cargo/

# Build the application in release mode
RUN cargo build --release --locked --bin api


# Runtime stage
FROM debian:bookworm-slim@sha256:78d2f66e0fec9e5a39fb2c72ea5e052b548df75602b5215ed01a17171529f706 AS runtime

# Install pinned apt dependencies
# eMudhra intermediate CA key compromise (2026-10-20): bookworm has no fixed
# ca-certificates yet, so distrust the two roots the compromised intermediates
# chain to. Remove once the pinned ca-certificates no longer ships them.
RUN --network=none \
    --mount=type=bind,from=apt-bundle-files,source=/apt-bundle,target=/opt/apt-bundle,ro \
    --mount=type=bind,source=build/apt-bundle/prepare-apt.sh,target=/run/prepare-apt.sh,ro \
    --mount=type=bind,source=pinned-packages-runtime.txt,target=/tmp/pinned-packages-runtime.txt,ro \
    set -e; \
    bash /run/prepare-apt.sh --bundle /opt/apt-bundle --snapshot 20260928T000000Z \
        --config /tmp/apt-bundle.conf --debian-keyring /usr/share/keyrings/debian-archive-keyring.gpg \
        --status /var/lib/dpkg/status --pins /tmp/pinned-packages-runtime.txt && \
    if [ -f /var/lib/apt/extended_states ]; then \
        cp /var/lib/apt/extended_states /tmp/apt-bundle.conf.d/state/extended_states; \
    fi && \
    export APT_CONFIG=/tmp/apt-bundle.conf && \
    apt-get update && \
    apt-get install -y --no-install-recommends \
        ca-certificates \
        libssl3 \
        curl \
        && sed -i -e 's|^mozilla/emSign_ECC_Root_CA_-_G3.crt$|!&|' \
            -e 's|^mozilla/Comodo_AAA_Services_root.crt$|!&|' /etc/ca-certificates.conf && \
    update-ca-certificates && \
    test ! -e /etc/ssl/certs/emSign_ECC_Root_CA_-_G3.pem && \
    test ! -e /etc/ssl/certs/Comodo_AAA_Services_root.pem && \
    cp /tmp/apt-bundle.conf.d/state/extended_states /var/lib/apt/extended_states && \
    rm -rf /tmp/apt-bundle.conf /tmp/apt-bundle.conf.d \
        /var/lib/apt/lists/* /var/log/* /var/cache/ldconfig/aux-cache

# Create app user
# Normalize /etc/shadow file last password change date to 0 for app user
RUN useradd -m -u 1000 app \
    && sed -i -r 's/^(app:[^:]*:)[0-9]+/\10/' /etc/shadow

# Create app directory
WORKDIR /app

# Copy the built binary
COPY --from=builder /app/target/release/api /app/api

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
