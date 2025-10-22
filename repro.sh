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
# Check if buildkit_20 already exists before creating it
if ! docker buildx inspect buildkit_20 &>/dev/null; then
    docker buildx create --use --driver-opt image=moby/buildkit:v0.20.2 --name buildkit_20
fi

# Create pinned-packages.txt if it doesn't exist (empty for first build)
if [ ! -f pinned-packages.txt ]; then
    touch pinned-packages.txt
fi

# Normalize timestamps on all source files before build
echo "Normalizing file timestamps for reproducibility..."
find . -type f \( -name "*.rs" -o -name "*.toml" -o -name "*.lock" -o -name "*.txt" -o -name "*.md" \) \
    -not -path "./target/*" -not -path "./.git/*" \
    -exec touch -t 197001010000.00 {} +

git rev-parse HEAD > .GIT_REV
touch -t 197001010000.00 .GIT_REV

TEMP_TAG="cloud-api:$(date +%s)"
docker buildx build --builder buildkit_20 --no-cache --build-arg SOURCE_DATE_EPOCH="0" \
    --build-arg RUST_IMAGE_SHA="sha256:8192a1c210289f3ebb95c62f0cd526427e9e28a5840d6362e95abe5a2e6831a5" \
    --output type=oci,dest=./oci.tar,rewrite-timestamp=true \
    --output type=docker,name="$TEMP_TAG" .

if [ "$?" -ne 0 ]; then
    echo "Build failed"
    rm .GIT_REV
    exit 1
fi

echo "Build completed, manifest digest:"
echo ""
docker image inspect "$TEMP_TAG" --format='{{ .Id }}'
echo ""

# Extract package information from the built image
echo "Extracting package information from built image: $TEMP_TAG"
docker run --rm --entrypoint bash "$TEMP_TAG" -c "dpkg -l | grep '^ii' | awk '{print \$2\"=\"\$3}' | sort" > pinned-packages.txt

echo "Package information extracted to pinned-packages.txt ($(wc -l < pinned-packages.txt) packages)"

rm .GIT_REV