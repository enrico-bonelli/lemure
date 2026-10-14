#!/bin/sh
# Compila in release e installa Lemure per l'utente corrente (senza pacchetto).
set -e
cd "$(dirname "$0")"
cargo build --release
install -Dm755 target/release/lemure "$HOME/.local/bin/lemure"
install -Dm644 data/it.lemure.Lemure.desktop "$HOME/.local/share/applications/it.lemure.Lemure.desktop"
install -Dm644 data/it.lemure.Lemure.svg "$HOME/.local/share/icons/hicolor/scalable/apps/it.lemure.Lemure.svg"
echo "Installato in $HOME/.local/bin/lemure"
