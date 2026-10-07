#!/bin/bash -ex

PREFIX="${PREFIX:-$HOME/.local}"
DATA="${XDG_DATA_HOME:-$HOME/.local/share}"

cd "$(dirname "$0")"
cargo build --release --locked

install -Dm755 target/release/oxidance "$PREFIX/bin/oxidance"
install -Dm644 data/io.github.oxidance.Oxidance.desktop "$DATA/applications/io.github.oxidance.Oxidance.desktop"
install -Dm644 data/icons/scalable/apps/io.github.oxidance.Oxidance.svg "$DATA/icons/hicolor/scalable/apps/io.github.oxidance.Oxidance.svg"

# The app reads its settings schema from here when the build directory is gone.
install -Dm644 data/io.github.oxidance.gschema.xml "$DATA/oxidance/schemas/io.github.oxidance.gschema.xml"
glib-compile-schemas --strict "$DATA/oxidance/schemas"

gtk-update-icon-cache -qtf "$DATA/icons/hicolor" || true
update-desktop-database -q "$DATA/applications" || true
