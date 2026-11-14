#!/bin/bash
# The author's setup: build on an Apple-silicon Mac (Lima VM with an aarch64 Arch build
# container) and fetch the binary to ./dist/. Usage: contrib/build-m5.sh [test]
set -euo pipefail
cd "$(dirname "$0")/.."
L="/opt/homebrew/bin/limactl shell --workdir / omm-test --"
# The build VM may be stopped (it is shared and stopped when idle).
ssh m5 "/opt/homebrew/bin/limactl list --format '{{.Status}}' omm-test | grep -q Running || /opt/homebrew/bin/limactl start omm-test" >/dev/null 2>&1
tar -cf - --exclude=dist --exclude=target --exclude=.git -C .. omacctv \
  | ssh m5 "$L sh -c 'tar -C ~/builds -xf -'"
run="~/builds/arm-build run omacctv env CARGO_TARGET_DIR=/cache/target/omacctv"
if [ "${1:-}" = test ]; then
  ssh m5 "$L sh -c '$run cargo test --release 2>&1'"
  exit
fi
ssh m5 "$L sh -c '$run cargo build --release 2>&1 && $run cp /cache/target/omacctv/release/omacctv /builds/omacctv/omacctv.bin'"
mkdir -p dist
ssh m5 "$L sh -c 'cat ~/builds/omacctv/omacctv.bin'" > dist/omacctv.new
[ -s dist/omacctv.new ] || { echo "build failed (empty binary)"; exit 1; }
chmod +x dist/omacctv.new && mv dist/omacctv.new dist/omacctv
ls -l dist/omacctv
