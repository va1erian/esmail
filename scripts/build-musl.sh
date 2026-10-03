#!/bin/sh
# Builds the mail core for x86_64-unknown-linux-musl the way LazyOS links it:
# the `rustls` backend, with zig compiling the bundled SQLite (a static-PIE
# binary, so position-independent objects). Needs `pip install
# ziglang==0.16.0` (or zig on PATH) and the musl Rust target.
set -eu
zig="${ZIG:-python3 -m ziglang}"
wrappers="$(mktemp -d)"
trap 'rm -rf "$wrappers"' EXIT
printf '#!/bin/sh\nexec %s cc -target x86_64-linux-musl "$@"\n' "$zig" > "$wrappers/zcc"
printf '#!/bin/sh\nexec %s ar "$@"\n' "$zig" > "$wrappers/zar"
chmod +x "$wrappers/zcc" "$wrappers/zar"
# cc-rs would add a --target= that zig rejects.
export CRATE_CC_NO_DEFAULTS=1
export CC_x86_64_unknown_linux_musl="$wrappers/zcc"
export AR_x86_64_unknown_linux_musl="$wrappers/zar"
export CFLAGS_x86_64_unknown_linux_musl="-Os -fPIC -ffunction-sections -fdata-sections"
exec cargo build -p esmail --no-default-features --features rustls \
    --target x86_64-unknown-linux-musl "$@"
