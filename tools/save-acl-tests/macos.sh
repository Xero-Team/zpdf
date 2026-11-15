#!/bin/sh
# Provision on the ephemeral runner; build/test inside a no-network sandbox.
set -eu
test "$(uname -s)" = Darwin
acl_source=$(pwd)
acl_root=$(mktemp -d "${RUNNER_TEMP:?}/zpdf-acl.XXXXXX")
acl_mount="$acl_root/volume"
acl_toolchain=$(dirname "$(rustup which cargo)")
mkdir "$acl_mount"
cleanup() {
    hdiutil detach "$acl_mount" -force >/dev/null 2>&1 || true
}
trap cleanup EXIT HUP INT TERM
hdiutil create -quiet -size 512m -fs APFS -volname ZPDF_ACL_CI "$acl_root/scratch.dmg"
hdiutil attach -quiet -nobrowse -owners on -mountpoint "$acl_mount" "$acl_root/scratch.dmg"
mkdir "$acl_mount/work" "$acl_mount/provision"
# Fetch only the locked libc crate. This phase executes no project build scripts.
env -i PATH="$acl_toolchain:/usr/bin:/bin" HOME="$acl_mount/provision" \
    CARGO_HOME="$acl_mount/provision/cargo" "$acl_toolchain/cargo" vendor --locked \
    --manifest-path "$acl_source/tools/save-acl-tests/Cargo.toml" "$acl_mount/vendor" >/dev/null
python3 - "$acl_root/profile.sb" "$acl_mount" "$acl_source" "$acl_toolchain" <<'PY'
import json, pathlib, sys
profile, mount, source, toolchain = sys.argv[1:]
q = json.dumps
pathlib.Path(profile).write_text('\n'.join([
    '(version 1)', '(allow default)', '(deny network*)', '(deny file-write*)',
    '(deny file-read* (subpath "/Users"))',
    f'(allow file-read* (subpath {q(source)}) (subpath {q(str(pathlib.Path(toolchain).parent))}) (subpath {q(mount)}))',
    f'(allow file-write* (subpath {q(mount + "/work")}) (literal "/dev/null"))',
]))
PY
python3 tools/save-acl-tests/run.py --source "$acl_source" --scratch "$acl_mount/work" \
    --vendor "$acl_mount/vendor" --cargo "$acl_toolchain/cargo" --sandbox "$acl_root/profile.sb"
