#!/usr/bin/env bash
# Self-hosted runners have Docker but no sudo. Run FFmpeg through
# stdin/stdout without modifying the runner OS. Production keeps its snapshot
# pin; CI uses the signed Debian mirror so tests do not wait on snapshot downloads.
set -euo pipefail
if command -v ffmpeg >/dev/null 2>&1; then
  exit 0
fi
: "${RUNNER_TEMP:?RUNNER_TEMP must be set}"
: "${GITHUB_PATH:?GITHUB_PATH must be set}"
image="cloud-api-ci-audio:${GITHUB_RUN_ID:-local}-${GITHUB_JOB:-test}"
bindir="$RUNNER_TEMP/cloud-api-audio-bin"
docker build -t "$image" - <<'DOCKERFILE'
FROM debian:bookworm-slim@sha256:78d2f66e0fec9e5a39fb2c72ea5e052b548df75602b5215ed01a17171529f706
RUN apt-get update && apt-get install -y --no-install-recommends ffmpeg && rm -rf /var/lib/apt/lists/*
DOCKERFILE
mkdir -p "$bindir"
cat > "$bindir/ffmpeg" <<WRAPPER
#!/usr/bin/env bash
exec docker run --rm -i --network none --read-only --cap-drop ALL --security-opt no-new-privileges --entrypoint /usr/bin/ffmpeg "$image" "\$@"
WRAPPER
chmod +x "$bindir/ffmpeg"
"$bindir/ffmpeg" -version
printf '%s\n' "$bindir" >> "$GITHUB_PATH"
