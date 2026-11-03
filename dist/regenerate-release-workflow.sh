#!/bin/sh
# Regenerates .github/workflows/release.yml from Cargo.toml (T-035).
#
# dist 0.34.0 writes local reusable workflows as `uses: $/.github/...`, which
# GitHub rejects. This script generates with `allow-dirty` lifted, restores
# `./`, and puts the config back, so dist treats the fixed file as intended.
set -eu
cd "$(dirname "$0")/.."
cp Cargo.toml Cargo.toml.orig
trap 'mv Cargo.toml.orig Cargo.toml' EXIT
grep -v '^allow-dirty' Cargo.toml.orig > Cargo.toml
dist generate --mode ci
sed 's#uses: \$/\.github/#uses: ./.github/#' .github/workflows/release.yml > .github/workflows/release.yml.tmp
mv .github/workflows/release.yml.tmp .github/workflows/release.yml
