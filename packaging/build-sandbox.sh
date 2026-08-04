#!/usr/bin/env bash
# Build the image `run_python` runs in.
#
# Once per machine, and again only when Containerfile.sandbox changes. The tool
# refuses with this command in the message when the image is not there, so the
# user is never left guessing what to run.
set -euo pipefail

cd "$(dirname "$0")"

IMAGE="${FAMILIAR_SANDBOX_IMAGE:-localhost/familiar-sandbox:1}"

if ! command -v podman >/dev/null 2>&1; then
    echo "podman is not installed. The Python sandbox needs it." >&2
    exit 1
fi

echo "Building $IMAGE — this pulls a few hundred megabytes the first time."
podman build --tag "$IMAGE" --file Containerfile.sandbox .

echo
echo "Built $IMAGE:"
podman images --filter "reference=$IMAGE" --format '  {{.Repository}}:{{.Tag}}  {{.Size}}'
