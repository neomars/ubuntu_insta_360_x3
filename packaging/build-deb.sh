#!/usr/bin/env bash
# Construit le paquet .deb (Ubuntu 22.04 / 24.04) dans ./dist/
#   sudo apt install build-essential debhelper dh-python pybuild-plugin-pyproject python3-all python3-setuptools fakeroot
#   ./packaging/build-deb.sh
set -euo pipefail
cd "$(dirname "$0")/.."

for tool in dpkg-buildpackage dh dh_python3 fakeroot; do
    command -v "$tool" >/dev/null || {
        echo "Outil manquant : $tool" >&2
        echo "  sudo apt install build-essential debhelper dh-python pybuild-plugin-pyproject python3-all python3-setuptools fakeroot" >&2
        exit 1
    }
done

# l'horodatage de l'archive est fixé par le changelog (build reproductible)
dpkg-buildpackage -us -uc -b
mkdir -p dist
version="$(dpkg-parsechangelog -S Version)"
mv -f "../x3studio_${version}_all.deb" "../x3studio_${version}_"*.buildinfo "../x3studio_${version}_"*.changes dist/ 2>/dev/null || true
dpkg-buildpackage -T clean >/dev/null 2>&1 || true
echo
echo "Paquet : dist/x3studio_${version}_all.deb"
echo "Installer : sudo apt install ./dist/x3studio_${version}_all.deb"
