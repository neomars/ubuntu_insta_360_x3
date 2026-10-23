#!/usr/bin/env bash
# Installe un .deb x3studio, vérifie qu'il fonctionne (CLI, export, interface locale), puis le désinstalle.
# Usage : packaging/smoke-test-deb.sh dist/x3studio_X_amd64.deb
set -euo pipefail

deb="$(readlink -f "${1:?usage : $0 fichier.deb}")"
SUDO=""; [ "$(id -u)" -eq 0 ] || SUDO="sudo"
work="$(mktemp -d)"
server_pid=""
cleanup() { [ -n "$server_pid" ] && kill "$server_pid" 2>/dev/null || true; rm -rf "$work"; }
trap cleanup EXIT
step() { echo "==> $*"; }
fail() { echo "ÉCHEC : $*" >&2; exit 1; }

step "installation (apt tire ffmpeg)"
$SUDO apt-get install -y --no-install-recommends -o APT::Sandbox::User=root "$deb"

step "fichiers installés"
for f in /usr/bin/x3studio /usr/share/applications/x3studio.desktop /usr/share/icons/hicolor/scalable/apps/x3studio.svg; do
    [ -e "$f" ] || fail "$f absent"
done
command -v desktop-file-validate >/dev/null && desktop-file-validate /usr/share/applications/x3studio.desktop
ldd /usr/bin/x3studio | grep -q "not found" && fail "bibliothèque manquante : $(ldd /usr/bin/x3studio | grep 'not found')"
ldd /usr/bin/x3studio | grep -qiE "libopencv|libpython" && fail "dépendance inattendue (opencv/python)"
echo "version : $(x3studio --version)"

step "démo, sonde, exports"
export HOME="$work/home" X3STUDIO_HOME="$work/cache" X3STUDIO_OUTPUT="$work/out"
mkdir -p "$HOME"; cd "$work"
x3studio demo "$work/essai.insv" --seconds 2 --lens 480 --motion shake
x3studio probe "$work/essai.insv" | tee "$work/probe.txt"
grep -q "deux flux" "$work/probe.txt" || fail "la sonde ne reconnaît pas le fichier de démonstration"
x3studio reframe "$work/essai.insv" -o "$work/plat.mp4" --size 640x360 --stabilize horizon --crf 28 --preset veryfast
x3studio equirect "$work/essai.insv" -o "$work/360.mp4" --width 960 --crf 30 --preset ultrafast
x3studio snapshot "$work/essai.insv" -o "$work/image.jpg" --t 1.0 --size 640x360
for f in plat.mp4 360.mp4 image.jpg; do [ -s "$work/$f" ] || fail "$f vide ou absent"; done
ffprobe -v error -select_streams v:0 -show_entries stream=width,height -of csv=p=0 "$work/plat.mp4" | grep -q "640,360" || fail "taille de plat.mp4"
grep -q "GSpherical:ProjectionType" "$work/360.mp4" || fail "métadonnées sphériques absentes de 360.mp4"

step "interface web locale"
x3studio gui --no-browser --port 0 > "$work/gui.log" 2>&1 &
server_pid=$!
url=""
for _ in $(seq 1 50); do
    url="$(grep -oE 'http://127\.0\.0\.1:[0-9]+/\?key=[A-Za-z0-9_-]+' "$work/gui.log" | head -1 || true)"
    [ -n "$url" ] && break
    sleep 0.2
done
[ -n "$url" ] || { cat "$work/gui.log"; fail "le serveur n'a pas affiché son adresse"; }
base="${url%%/\?*}"
[ "$(curl -s -o /dev/null -w '%{http_code}' "$base/")" = 403 ] || fail "la page d'accueil doit être refusée sans la clé"
[ "$(curl -s -o /dev/null -w '%{http_code}' "$url")" = 200 ] || fail "la page d'accueil doit être servie avec la clé"
[ "$(curl -s -o /dev/null -w '%{http_code}' "$base/app.js")" = 200 ] || fail "app.js"
[ "$(curl -s -o /dev/null -w '%{http_code}' "$base/api/info")" = 401 ] || fail "l'API doit exiger le jeton"
kill "$server_pid"; wait "$server_pid" 2>/dev/null || true; server_pid=""

step "désinstallation"
$SUDO apt-get remove -y x3studio
[ ! -e /usr/bin/x3studio ] || fail "/usr/bin/x3studio subsiste"
echo "OK"
