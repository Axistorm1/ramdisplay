#!/usr/bin/env bash
# Install the build dependencies for ramdisplay.
#
# gtk4 is available almost everywhere; gtk4-layer-shell is packaged on
# Arch and Fedora and on newer Debian/Ubuntu, but must be built from source on
# Ubuntu 24.04 and older. This script handles all of that.
set -euo pipefail

have() { command -v "$1" >/dev/null 2>&1; }

echo "==> Detecting distribution"
if [ -f /etc/os-release ]; then
    . /etc/os-release
    DISTRO="${ID:-unknown}"
else
    DISTRO="unknown"
fi
echo "    ID=$DISTRO (${PRETTY_NAME:-?})"

install_rust() {
    if have cargo; then
        echo "==> Rust toolchain already present: $(cargo --version)"
    else
        echo "==> Installing Rust via rustup"
        curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
        # shellcheck disable=SC1090
        source "$HOME/.cargo/env"
    fi
}

build_layer_shell_from_source() {
    echo "==> Building gtk4-layer-shell from source into /usr/local"
    local tmp
    tmp="$(mktemp -d)"
    git clone --depth 1 https://github.com/wmww/gtk4-layer-shell.git "$tmp"
    ( cd "$tmp"
      meson setup build --prefix=/usr/local -Dintrospection=false -Dvapi=false -Dexamples=false -Dtests=false
      ninja -C build
      sudo ninja -C build install )
    sudo ldconfig
    rm -rf "$tmp"
}

case "$DISTRO" in
    arch|manjaro|endeavouros|cachyos)
        sudo pacman -S --needed --noconfirm base-devel rust gtk4 gtk4-layer-shell pkgconf
        ;;
    fedora|rhel|centos)
        sudo dnf install -y gcc gcc-c++ rust cargo gtk4-devel gtk4-layer-shell-devel pkgconf-pkg-config meson ninja-build git
        ;;
    debian|ubuntu|pop|linuxmint|elementary)
        sudo apt-get update
        if apt-cache show libgtk4-layer-shell-dev >/dev/null 2>&1; then
            sudo apt-get install -y build-essential rustc cargo libgtk-4-dev \
                libgtk4-layer-shell-dev pkg-config meson ninja-build git
        else
            echo "!! libgtk4-layer-shell-dev not packaged here; building from source"
            sudo apt-get install -y build-essential libgtk-4-dev \
                pkg-config meson ninja-build git
            build_layer_shell_from_source
        fi
        ;;
    *)
        echo "!! Unknown distribution '$DISTRO'."
        echo "   Please install: a C toolchain, Rust, GTK4 development headers,"
        echo "   pkg-config and gtk4-layer-shell (library + headers) manually."
        ;;
esac

install_rust

echo
echo "==> Done. Build with:  cargo build --release"
echo "    Binary:            target/release/ramdisplay"
