#!/usr/bin/env bash
# Construit le paquet .deb (Ubuntu 22.04 / 24.04, amd64) dans ./dist/ avec cargo-deb.
#   sudo apt install build-essential dpkg-dev          # + Rust : https://rustup.rs
#   ./packaging/build-deb.sh
# Construire sur Ubuntu 22.04 (glibc 2.35) donne un paquet qui s'installe aussi sur 24.04.
set -euo pipefail
cd "$(dirname "$0")/.."

command -v cargo >/dev/null || { echo "Rust (cargo) est requis : https://rustup.rs" >&2; exit 1; }
command -v dpkg-shlibdeps >/dev/null || { echo "Outil manquant : sudo apt install dpkg-dev" >&2; exit 1; }
command -v cargo-deb >/dev/null || cargo install cargo-deb --locked

rm -rf dist && mkdir -p dist
cargo deb --locked --output dist/
echo
ls -l dist/*.deb
echo "Installer : sudo apt install ./$(ls dist/*.deb | head -1)"
