#!/usr/bin/env bash
#
# Installs (or removes) the headset battery bridge: the binary, the udev rule
# that lets the service reach the headset, and the systemd unit.
#
# Build first, as yourself, then install as root:
#
#     cargo build --release
#     sudo ./install.sh
#
set -euo pipefail

BIN_NAME=headset-battery-indicator
PREFIX=${PREFIX:-/usr/local}
BIN_DIR="$PREFIX/bin"
UNIT_DIR=${UNIT_DIR:-/etc/systemd/system}
UDEV_DIR=${UDEV_DIR:-/etc/udev/rules.d}
GROUP=${GROUP:-headset-battery}
# Must sort before 73-seat-late.rules: once uaccess has put an ACL on the node,
# MODE= only changes the ACL mask and the group never gets access.
UDEV_RULE="$UDEV_DIR/70-$BIN_NAME.rules"
LEGACY_UDEV_RULE="$UDEV_DIR/99-$BIN_NAME.rules"
UNIT="$UNIT_DIR/$BIN_NAME.service"
SOURCE_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)

say() { printf '\033[1m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[33m==> %s\033[0m\n' "$*" >&2; }
die() { printf '\033[31m==> %s\033[0m\n' "$*" >&2; exit 1; }

require_root() {
    [ "$(id -u)" -eq 0 ] || die "run this as root: sudo $0 ${1:-}"
}

# Nodes that already exist carry the ACL uaccess gave them, and re-running the
# rule on them is a chmod, which only moves the ACL mask. Give the owning group
# its entry back directly; nodes created from now on get it from the rule.
repair_existing_nodes() {
    local node
    for node in /dev/hidraw*; do
        [ -e "$node" ] || continue
        [ "$(stat -c %G "$node")" = "$GROUP" ] || continue
        if getfacl -p "$node" 2>/dev/null | grep -q '^group::rw'; then
            continue
        fi
        if command -v setfacl >/dev/null 2>&1; then
            say "repairing the group ACL entry on $node"
            setfacl -m g::rw "$node"
        else
            warn "$node still denies the $GROUP group: unplug and replug the dongle"
        fi
    done
}

install_all() {
    require_root

    local built="$SOURCE_DIR/target/release/$BIN_NAME"
    [ -x "$built" ] || die "no release build found at $built - run 'cargo build --release' first"

    command -v headsetcontrol >/dev/null 2>&1 ||
        warn "headsetcontrol is not on PATH; install it from https://github.com/Sapd/HeadsetControl"

    say "installing $BIN_NAME to $BIN_DIR"
    install -Dm0755 "$built" "$BIN_DIR/$BIN_NAME"

    if getent group "$GROUP" >/dev/null; then
        say "group $GROUP already exists"
    else
        say "creating system group $GROUP"
        groupadd --system "$GROUP"
    fi

    say "generating $UDEV_RULE"
    local rules
    rules=$("$BIN_DIR/$BIN_NAME" udev-rules --group "$GROUP")
    if printf '%s\n' "$rules" | grep -q '^KERNEL=='; then
        install -d "$UDEV_DIR"
        rm -f "$LEGACY_UDEV_RULE"
        printf '%s\n' "$rules" >"$UDEV_RULE"
        udevadm control --reload
        udevadm trigger --subsystem-match=hidraw
        udevadm settle
        repair_existing_nodes
    else
        warn "no headset detected: plug the dongle in and re-run '$BIN_NAME udev-rules --group $GROUP > $UDEV_RULE'"
    fi

    say "installing $UNIT"
    install -Dm0644 "$SOURCE_DIR/packaging/systemd/$BIN_NAME.service" "$UNIT"
    if [ "$GROUP" != "headset-battery" ]; then
        sed -i "s/^SupplementaryGroups=.*/SupplementaryGroups=$GROUP/" "$UNIT"
    fi
    if [ "$BIN_DIR/$BIN_NAME" != "/usr/local/bin/$BIN_NAME" ]; then
        sed -i "s|^ExecStart=/usr/local/bin/$BIN_NAME|ExecStart=$BIN_DIR/$BIN_NAME|" "$UNIT"
    fi

    systemctl daemon-reload
    systemctl enable "$BIN_NAME.service"
    # `enable --now` leaves an already running service alone, which on an
    # upgrade means the old binary keeps running.
    systemctl restart "$BIN_NAME.service"

    say "done. Check it with:"
    printf '    systemctl status %s.service\n' "$BIN_NAME"
    printf '    upower -d | grep -A6 headset\n'
}

uninstall_all() {
    require_root --uninstall

    say "stopping the service"
    systemctl disable --now "$BIN_NAME.service" 2>/dev/null || true
    rm -f "$UNIT"
    systemctl daemon-reload

    say "removing the udev rule"
    rm -f "$UDEV_RULE" "$LEGACY_UDEV_RULE"
    udevadm control --reload || true

    say "removing the binary"
    rm -f "$BIN_DIR/$BIN_NAME"

    if getent group "$GROUP" >/dev/null; then
        say "removing the group $GROUP"
        groupdel "$GROUP" || warn "could not remove the group $GROUP"
    fi

    say "done."
}

case "${1:-install}" in
    install) install_all ;;
    -u | --uninstall | uninstall) uninstall_all ;;
    -h | --help)
        sed -n '2,10p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
        ;;
    *) die "unknown argument: $1 (try --help)" ;;
esac
