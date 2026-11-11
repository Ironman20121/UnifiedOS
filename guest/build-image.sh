#!/usr/bin/env bash
# Builds a guest root filesystem image for UnifiedOS application VMs.
#
#   guest/build-image.sh linux     # Linux applications
#   guest/build-image.sh windows   # Windows applications (Wine)
#
# The image is written to $UOS_IMAGE_DIR (default ~/.local/share/uos/images).
# It carries kernel modules for the running host kernel, so rebuild it after a
# kernel update. Needs podman and e2fsprogs; does not need root.
set -euo pipefail

guest=${1:-linux}
case "$guest" in
linux | windows) ;;
*)
    echo "usage: $0 [linux|windows]" >&2
    exit 2
    ;;
esac

# Free space added on top of the files, for the guest to write to.
readonly SLACK_MIB=256
# Modules the guest init loads: the shared application directory, the
# display and the keyboard and pointer.
readonly GUEST_MODULES=(virtiofs virtio-gpu virtio_input)

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
out=${UOS_IMAGE_DIR:-${XDG_DATA_HOME:-$HOME/.local/share}/uos/images}
release=$(uname -r)
mkdir -p "$out"

podman build --quiet --tag uos-guest-linux --file "$here/Containerfile.linux" "$here"
if [ "$guest" = windows ]; then
    podman build --quiet --tag uos-guest-windows --file "$here/Containerfile.windows" "$here"
fi

work=$(mktemp -d -p "${TMPDIR:-/var/tmp}" uos-image.XXXXXX)
# Files extracted inside the user namespace belong to sub-uids; remove them there too.
trap 'podman unshare rm -rf "$work"' EXIT

container=$(podman create "uos-guest-$guest")
podman export --output "$work/rootfs.tar" "$container"
podman rm "$container" >/dev/null

mkdir "$work/modules"
for name in "${GUEST_MODULES[@]}"; do
    modprobe --show-depends "$name" | awk '$1 == "insmod" { print $2 }'
done | awk '!seen[$0]++' |
    while read -r module; do
        cp "$module" "$work/modules/"
        basename "$module" >>"$work/modules/order"
    done

# Run inside podman's user namespace so files keep root ownership in the image.
podman unshare bash -euo pipefail -c '
    work=$1 slack_mib=$2
    mkdir "$work/root"
    tar --extract --file "$work/rootfs.tar" --directory "$work/root" --exclude="dev/*"
    mkdir -p "$work/root/usr/lib/uos-modules" "$work/root/app"
    cp "$work"/modules/* "$work/root/usr/lib/uos-modules/"
    used_mib=$(du --summarize --block-size=1M "$work/root" | cut -f1)
    size_mib=$((used_mib + used_mib / 4 + slack_mib))
    mkfs.ext4 -q -F -L "uos-guest" -d "$work/root" "$work/image.ext4" "${size_mib}M"
' _ "$work" "$SLACK_MIB"

mv "$work/image.ext4" "$out/$guest.ext4"
echo "$release" >"$out/$guest.kernel"
echo "built $out/$guest.ext4 for kernel $release"
