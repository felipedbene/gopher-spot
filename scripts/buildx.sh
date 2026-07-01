#!/usr/bin/env bash
# Multi-arch build + push for both gopher-spot images. linux/amd64 + linux/arm64
# is mandatory: the debene cluster is mixed (intel*/zima/ultra2 amd64, orion
# arm64) and nothing is pinned, so a single-arch image would fail to schedule on
# half the nodes.
#
#   ./scripts/buildx.sh audio     # Fio A: just the audio-stream image
#   ./scripts/buildx.sh server    # Fio B: just the gopher-server image
#   ./scripts/buildx.sh           # both
#
# Requires: docker buildx + a logged-in ghcr.io (docker login ghcr.io).
set -euo pipefail

REGISTRY="ghcr.io/felipedbene"
PLATFORMS="linux/amd64,linux/arm64"
TAG="$(git rev-parse --short HEAD 2>/dev/null || echo dev)"

docker buildx create --use --name gopher-spot-builder 2>/dev/null || \
  docker buildx use gopher-spot-builder

build_audio() {
  echo ">> building ${REGISTRY}/gopher-spot-audio (${TAG}) for ${PLATFORMS}"
  docker buildx build \
    --platform "${PLATFORMS}" \
    -t "${REGISTRY}/gopher-spot-audio:${TAG}" \
    -t "${REGISTRY}/gopher-spot-audio:latest" \
    --push -f docker/audio-stream.Dockerfile .
}

build_server() {
  echo ">> building ${REGISTRY}/gopher-spot-server (${TAG}) for ${PLATFORMS}"
  docker buildx build \
    --platform "${PLATFORMS}" \
    -t "${REGISTRY}/gopher-spot-server:${TAG}" \
    -t "${REGISTRY}/gopher-spot-server:latest" \
    --push -f docker/gopher-server.Dockerfile .
}

case "${1:-all}" in
  audio)  build_audio ;;
  server) build_server ;;
  all)    build_audio; build_server ;;
  *) echo "usage: $0 [audio|server|all]" >&2; exit 64 ;;
esac
