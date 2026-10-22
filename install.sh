#!/usr/bin/env bash
# Installation sur Ubuntu / Debian : dépendances système, environnement Python, commande `x3studio`.
set -euo pipefail
cd "$(dirname "$0")"

echo "==> Dépendances système (sudo requis)"
sudo apt-get update
sudo apt-get install -y ffmpeg python3 python3-venv python3-pip python3-numpy python3-opencv

PREFIX="${X3STUDIO_PREFIX:-$HOME/.local/share/x3studio}"
echo "==> Environnement Python dans $PREFIX"
python3 -m venv --system-site-packages "$PREFIX/venv"
"$PREFIX/venv/bin/pip" install --upgrade pip >/dev/null
"$PREFIX/venv/bin/pip" install .

mkdir -p "$HOME/.local/bin" "$HOME/.local/share/applications"
ln -sf "$PREFIX/venv/bin/x3studio" "$HOME/.local/bin/x3studio"
install -m 644 packaging/x3studio.desktop "$HOME/.local/share/applications/x3studio.desktop"

echo
echo "Installé. Lancez :  x3studio            (interface web locale)"
echo "                    x3studio --help     (ligne de commande)"
case ":$PATH:" in *":$HOME/.local/bin:"*) ;; *) echo "Ajoutez ~/.local/bin à votre PATH si la commande est introuvable." ;; esac
