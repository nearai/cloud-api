#!/usr/bin/env bash
# Self-hosted runners have Docker but no sudo. Run the production decoder through
# stdin/stdout in the same pinned runtime layer without modifying the runner OS.
set -euo pipefail
if command -v ffmpeg >/dev/null 2>&1; then
  exit 0
fi
: "${RUNNER_TEMP:?RUNNER_TEMP must be set}"
: "${GITHUB_PATH:?GITHUB_PATH must be set}"
image="cloud-api-ci-audio:${GITHUB_RUN_ID:-local}-${GITHUB_JOB:-test}"
bindir="$RUNNER_TEMP/cloud-api-audio-bin"
docker build --target runtime-deps -t "$image" .
mkdir -p "$bindir"
cat > "$bindir/ffmpeg" <<WRAPPER
#!/usr/bin/env bash
exec docker run --rm -i --network none --read-only --cap-drop ALL --security-opt no-new-privileges --entrypoint /usr/bin/ffmpeg "$image" "\$@"
WRAPPER
chmod +x "$bindir/ffmpeg"
"$bindir/ffmpeg" -version
printf '%s\n' "$bindir" >> "$GITHUB_PATH"
