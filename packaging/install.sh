#!/bin/sh
# install.sh — install ss-gui and its desktop entry for one user (default) or a
# prefix such as /usr/local.
#
#   ./packaging/install.sh                  # ~/.local, builds release first
#   ./packaging/install.sh --no-build       # use target/release/ss-gui as is
#   ./packaging/install.sh --prefix /usr/local   # needs write access
#
# Files:
#   $PREFIX/bin/ss-gui
#   $PREFIX/bin/ss-clone-launch
#   $PREFIX/share/applications/ss-clone.desktop  (Exec uses the absolute path)
#   $PREFIX/share/icons/hicolor/scalable/apps/ss-clone.svg
#
# Uninstall by removing those four paths. No system service is installed and no
# extension/plugin is loaded: it is a normal Wayland client, started on demand.
set -eu

HERE=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
ROOT=$(cd "$HERE/.." && pwd)
PREFIX=${PREFIX:-$HOME/.local}
BUILD=1

while [ $# -gt 0 ]; do
  case $1 in
    --prefix) PREFIX=$2; shift 2;;
    --no-build) BUILD=0; shift;;
    --build) BUILD=1; shift;;
    -h|--help) sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'; exit 0;;
    *) echo "unknown argument: $1" >&2; exit 2;;
  esac
done

if [ "$BUILD" -eq 1 ]; then
  command -v cargo >/dev/null 2>&1 || {
    echo "cargo not found; export PATH=\"\$HOME/.cargo/bin:\$PATH\"" >&2
    exit 1
  }
  (cd "$ROOT" && cargo build --release -p ss-gui)
fi
BIN=$ROOT/target/release/ss-gui
[ -x "$BIN" ] || { echo "no built client at $BIN (drop --no-build?)" >&2; exit 1; }

BINDIR=$PREFIX/bin
APPDIR=$PREFIX/share/applications
ICONDIR=$PREFIX/share/icons/hicolor/scalable/apps
mkdir -p "$BINDIR" "$APPDIR" "$ICONDIR"

install -m 0755 "$BIN" "$BINDIR/ss-gui"
install -m 0755 "$HERE/ss-clone-launch.sh" "$BINDIR/ss-clone-launch"
install -m 0644 "$HERE/ss-clone.svg" "$ICONDIR/ss-clone.svg"

# The committed .desktop is valid for /usr/bin packaging; a user prefix needs an
# absolute launcher path because ~/.local/bin is not always on a GUI session's
# PATH.
sed "s|^Exec=ss-clone-launch$|Exec=$BINDIR/ss-clone-launch|" \
    "$HERE/ss-clone.desktop" >"$APPDIR/ss-clone.desktop"
chmod 0644 "$APPDIR/ss-clone.desktop"

if command -v update-desktop-database >/dev/null 2>&1; then
  update-desktop-database "$APPDIR" >/dev/null 2>&1 || true
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
  gtk-update-icon-cache -q -t "$PREFIX/share/icons/hicolor" >/dev/null 2>&1 || true
fi

echo "installed:"
echo "  $BINDIR/ss-gui"
echo "  $BINDIR/ss-clone-launch"
echo "  $APPDIR/ss-clone.desktop"
echo "  $ICONDIR/ss-clone.svg"
echo
echo "Launch \"ss-clone\" from the application menu, or run:"
echo "  $BINDIR/ss-clone-launch"
echo "Host: SS_HOST (default mm.local); see \${XDG_CONFIG_HOME:-\$HOME/.config}/ss-clone/config"
echo "Log:  \${XDG_STATE_HOME:-\$HOME/.local/state}/ss-clone/ss-gui.log"
