#!/bin/sh
# Run only in the disposable CI VM. Source/toolchain mounts are read-only;
# the unprivileged jail has no IP networking, home, credentials or host sockets.
set -eu
test "$(uname -s)" = FreeBSD
test "$(id -u)" = 0
acl_source=$(pwd)
acl_root=$(mktemp -d /tmp/zpdf-acl.XXXXXX)
acl_jail="$acl_root/jail"
acl_name="zpdf_acl_$$"
mkdir -p "$acl_jail" "$acl_root/provision"
env -i PATH=/usr/local/bin:/usr/bin:/bin HOME="$acl_root/provision" \
    CARGO_HOME="$acl_root/provision/cargo" cargo vendor --locked \
    --manifest-path "$acl_source/tools/save-acl-tests/Cargo.toml" "$acl_root/vendor" >/dev/null
acl_mounts=""
acl_devices=""
cleanup() {
    jail -r "$acl_name" 2>/dev/null || true
    for acl_path in $acl_mounts; do umount "$acl_path" || true; done
    for acl_device in $acl_devices; do mdconfig -d -u "$acl_device" || true; done
}
trap cleanup EXIT HUP INT TERM
for acl_dir in bin sbin lib libexec usr; do
    mkdir "$acl_jail/$acl_dir"
    mount -t nullfs -o ro "/$acl_dir" "$acl_jail/$acl_dir"
    acl_mounts="$acl_jail/$acl_dir $acl_mounts"
done
mkdir -p "$acl_jail/etc" "$acl_jail/var/run" "$acl_jail/dev" "$acl_jail/src" "$acl_jail/deps" "$acl_jail/scratch"
# Only non-secret resolver/user metadata, never the VM's passwd database or keys.
printf 'root:*:0:0:root:/nonexistent:/bin/sh\nnobody:*:65534:65534:nobody:/scratch/home:/bin/sh\n' > "$acl_jail/etc/passwd"
printf 'wheel:*:0:root\nnogroup:*:65534:nobody\n' > "$acl_jail/etc/group"
cp /var/run/ld-elf.so.hints "$acl_jail/var/run/"
mount -t devfs -o ruleset=4 devfs "$acl_jail/dev"
acl_mounts="$acl_jail/dev $acl_mounts"
mount -t nullfs -o ro "$acl_source" "$acl_jail/src"
acl_mounts="$acl_jail/src $acl_mounts"
mount -t nullfs -o ro "$acl_root/vendor" "$acl_jail/deps"
acl_mounts="$acl_jail/deps $acl_mounts"
mount -t tmpfs -o size=512m tmpfs "$acl_jail/scratch"
acl_mounts="$acl_jail/scratch $acl_mounts"
mkdir "$acl_jail/scratch/fixtures"
for acl_kind in posix nfs4 disabled; do
    acl_path="$acl_jail/scratch/fixtures/$acl_kind"
    mkdir "$acl_path"
    acl_device=$(mdconfig -a -t swap -s 64m)
    acl_devices="$acl_device $acl_devices"
    newfs -U "/dev/$acl_device" >/dev/null
    case "$acl_kind" in
        posix) acl_options=acls ;;
        nfs4) acl_options=nfsv4acls ;;
        disabled) acl_options=noacls,nonfsv4acls ;;
    esac
    mount -o "$acl_options" "/dev/$acl_device" "$acl_path"
    acl_mounts="$acl_path $acl_mounts"
    chown 65534:65534 "$acl_path"
    chmod 700 "$acl_path"
done
chown 65534:65534 "$acl_jail/scratch"
chmod 700 "$acl_jail/scratch"
# Read-only parents must be traversable by the dummy user in the jail.
chmod 755 "$acl_root"
jail -c name="$acl_name" path="$acl_jail" host.hostname=zpdf-acl \
    ip4=disable ip6=disable allow.raw_sockets=0 persist
jexec -u nobody "$acl_name" /usr/bin/env -i PATH=/usr/local/bin:/usr/bin:/bin \
    /usr/local/bin/python3.11 /src/tools/save-acl-tests/run.py \
    --source /src --scratch /scratch --vendor /deps --cargo /usr/local/bin/cargo
