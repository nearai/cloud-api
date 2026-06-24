#!/bin/bash

# Parse command line arguments
PUSH=false
REPO=""

while [[ $# -gt 0 ]]; do
    case $1 in
        --push)
            PUSH=true
            REPO="$2"
            if [ -z "$REPO" ]; then
                echo "Error: --push requires a repository argument"
                echo "Usage: $0 [--push <repo>[:<tag>]]"
                exit 1
            fi
            shift 2
            ;;
        *)
            echo "Usage: $0 [--push <repo>[:<tag>]]"
            exit 1
            ;;
    esac
done
# Use a unique, freshly-created BuildKit builder pinned to a specific version.
# Two reasons:
#   1. Reproducibility: OCI layer serialization and compression differ between
#      BuildKit versions, so the builder image must be pinned. Creating it fresh
#      each run guarantees the pinned version -- a long-lived, reused builder
#      only honored the pin on first creation, so a host whose builder predated
#      the pin silently drifted (e.g. gpu11 ran v0.27.1 while the rest of the
#      fleet ran the pinned v0.20.2, producing a different digest for identical
#      inputs).
#   2. Parallelism: the CI fleet runs builds on shared Docker daemons. A unique
#      per-invocation builder name means concurrent builds never reuse or tear
#      down each other's builder.
# The builder is removed on exit so buildkitd containers do not accumulate on
# shared hosts.
BUILDKIT_IMAGE="moby/buildkit:v0.20.2"
BUILDER_NAME="cloud-api-buildkit-$$-${RANDOM}"
docker buildx create --driver-opt image="${BUILDKIT_IMAGE}" --name "${BUILDER_NAME}" >/dev/null
trap 'docker buildx rm "${BUILDER_NAME}" >/dev/null 2>&1 || true' EXIT
touch pinned-packages-builder.txt pinned-packages-runtime.txt
git rev-parse HEAD > .GIT_REV
# Unique tag so two builds co-located on one daemon cannot collide (and one's
# `docker rmi` cannot yank an image the other is still reading).
TEMP_TAG="cloud-api-temp:$(date +%s)-$$-${RANDOM}"
# --ulimit nofile: raise the open-file limit for RUN steps. The default soft
# limit on the self-hosted runners is low enough that the parallel `cargo build`
# exhausts file descriptors ("Too many open files (os error 24)"). This only
# affects build-time resource limits, never the output bytes, so it is safe for
# reproducibility.
docker buildx build --builder "${BUILDER_NAME}" --no-cache --platform linux/amd64 \
    --ulimit nofile=1048576:1048576 \
    --build-arg SOURCE_DATE_EPOCH="0" \
    --output type=oci,dest=./oci.tar,rewrite-timestamp=true \
    --output type=docker,name="$TEMP_TAG",rewrite-timestamp=true .

if [ "$?" -ne 0 ]; then
    echo "Build failed"
    rm .GIT_REV
    exit 1
fi

echo "Build completed, manifest digest:"
echo ""
skopeo inspect oci-archive:./oci.tar | jq .Digest
echo ""

if [ "$PUSH" = true ]; then
    echo "Pushing image to $REPO..."
    skopeo copy --insecure-policy oci-archive:./oci.tar docker://"$REPO"
    echo "Image pushed successfully to $REPO"
else
    echo "To push the image to a registry, run:"
    echo ""
    echo " $0 --push <repo>[:<tag>]"
    echo ""
    echo "Or use skopeo directly:"
    echo ""
    echo " skopeo copy --insecure-policy oci-archive:./oci.tar docker://<repo>[:<tag>]"
    echo "" 
fi
echo ""

# Extract package information from the built image
echo "Extracting package information from built image: $TEMP_TAG"
# Extract builder stage package information (resolved = what apt actually installed)
docker run --rm "$TEMP_TAG" cat /app/pinned-packages-builder.txt > pinned-packages-builder.resolved.txt
echo "Package information extracted to pinned-packages-builder.resolved.txt ($(wc -l < pinned-packages-builder.resolved.txt) packages)"
# Extract runtime stage package information (resolved = what apt actually installed)
docker run --rm --entrypoint bash "$TEMP_TAG" -c "dpkg -l | grep '^ii' | awk '{print \$2\"=\"\$3}' | sort" > pinned-packages-runtime.resolved.txt
echo "Package information extracted to pinned-packages-runtime.resolved.txt ($(wc -l < pinned-packages-runtime.resolved.txt) packages)"

# Clean up the temporary image from Docker daemon
docker rmi "$TEMP_TAG" 2>/dev/null || true

rm .GIT_REV
