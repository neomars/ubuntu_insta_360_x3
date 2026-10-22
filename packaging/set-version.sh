#!/usr/bin/env bash
# Reporte une version dans Cargo.toml, Cargo.lock et la page de manuel.
#
#   packaging/set-version.sh 1.2.3           # version finale
#   packaging/set-version.sh 1.2.3-beta.1    # préversion (alpha | beta | rc) ; cargo-deb la convertit en 1.2.3~beta.1
#
# Utilisé par le workflow « Build » : la version vient du tag `v1.2.3`.
# Deuxième argument facultatif : la racine du dépôt (pour les tests).
set -euo pipefail

version="${1:?usage : $0 VERSION [RACINE]}"
root="${2:-$(cd "$(dirname "$0")/.." && pwd)}"

re='^[0-9]+\.[0-9]+\.[0-9]+(-(alpha|beta|rc)\.[0-9]+)?$'
if ! [[ "$version" =~ $re ]]; then
    echo "Erreur : version « $version » invalide : attendu 1.2.3 (ou 1.2.3-beta.1, -alpha.N, -rc.N)." >&2
    exit 1
fi

# Cargo.toml : la première ligne « version = "..." » est celle de la section [package].
awk -v v="$version" '!done && /^version = "/ { print "version = \"" v "\""; done = 1; next } { print }' \
    "$root/Cargo.toml" > "$root/Cargo.toml.new"
mv "$root/Cargo.toml.new" "$root/Cargo.toml"

# Cargo.lock : la version de notre propre paquet (sinon `cargo build --locked` échouerait).
if [ -f "$root/Cargo.lock" ]; then
    awk -v v="$version" '
        prev == "name = \"x3studio\"" && /^version = "/ { print "version = \"" v "\""; prev = ""; next }
        { prev = $0; print }' "$root/Cargo.lock" > "$root/Cargo.lock.new"
    mv "$root/Cargo.lock.new" "$root/Cargo.lock"
fi

# Page de manuel : .TH X3STUDIO 1 "date" "x3studio X.Y.Z" "..."
if [ -f "$root/packaging/x3studio.1" ]; then
    sed -i -E "s/\"x3studio [^\"]+\"/\"x3studio $version\"/" "$root/packaging/x3studio.1"
fi

# Journal des modifications du .deb : une entrée en tête (forme Debian : 1.2.3~beta.1), l'historique est conservé.
if [ -f "$root/packaging/changelog" ]; then
    deb_version="${version/-/\~}-1"   # révision Debian 1 (cargo-deb l'ajoute au nom du paquet)
    if ! grep -q "^x3studio ($deb_version) " "$root/packaging/changelog"; then
        maint="$(sed -n 's/^ -- \(.*\)  [A-Z][a-z][a-z],.*$/\1/p' "$root/packaging/changelog" | head -1)"
        {
            echo "x3studio ($deb_version) noble; urgency=medium"
            echo
            echo "  * Version $version."
            echo
            echo " -- ${maint:-x3studio <noreply@localhost>}  $(date -u -R)"
            echo
            cat "$root/packaging/changelog"
        } > "$root/packaging/changelog.new"
        mv "$root/packaging/changelog.new" "$root/packaging/changelog"
    fi
fi

echo "Version $version"
