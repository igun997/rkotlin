#!/bin/sh
# Installs a prebuilt rkotlin binary (no Rust toolchain needed, nothing to compile on your laptop).
#   curl -fsSL https://raw.githubusercontent.com/igun997/rkotlin/main/install.sh | sh
set -eu
repo="igun997/rkotlin"
os=$(uname -s); arch=$(uname -m)
case "$os-$arch" in
  Linux-x86_64)  target=x86_64-unknown-linux-gnu ;;
  Linux-aarch64|Linux-arm64) target=aarch64-unknown-linux-gnu ;;
  Darwin-arm64)  target=aarch64-apple-darwin ;;
  Darwin-x86_64) target=x86_64-apple-darwin ;;
  *) echo "rkotlin: no prebuilt binary for $os $arch. Build from source: cargo install --git https://github.com/$repo" >&2; exit 1 ;;
esac
dest="${RKOTLIN_INSTALL_DIR:-$HOME/.local/bin}"
mkdir -p "$dest"
url="https://github.com/$repo/releases/latest/download/rkotlin-$target.tar.gz"
echo "Downloading $url"
curl -fsSL "$url" | tar -xz -C "$dest" rkotlin
chmod +x "$dest/rkotlin"
echo "Installed $dest/rkotlin"
case ":$PATH:" in *":$dest:"*) ;; *) echo "Add $dest to your PATH:  export PATH=\"$dest:\$PATH\"" ;; esac
echo "Next: go to your Kotlin/Gradle project and run:  rkotlin init"
