#!/usr/bin/env bash
# Build the PCL module (BigTIFF-pxm.dylib) + mpp-ipc-worker on macOS, run the
# module build gates, run the host CTest golden suite, and stage the
# unsigned artifact payload.
#
# Usage: build-module-macos.sh [--pcl-inc <dir>] [--pcl-lib <dir>] [--stage <dir>]
#   See build-module-linux.sh's header comment for the --pcl-inc/--pcl-lib
#   rationale (module/makefile-x64's PCLINCDIR/PCLLIBDIR variable names).
#
# Adapted (mechanical rename + retarget) from MergeMosaic's
# integration/pixinsight/ci/build-module-macos.sh; see
# build-module-linux.sh's header comment for the full rationale (doc
# staging, --pcl split, LICENSE-PCL double-staging) -- identical here.
# CANNOT be run/verified on this machine (Linux, no clang++/macOS SDK/AppKit).
set -euo pipefail
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"

PCL_INC="" ; PCL_LIB="" ; STAGE="$REPO_ROOT/stage"
while [ $# -gt 0 ]; do
  case "$1" in
    --pcl-inc) PCL_INC="$2"; shift 2 ;;
    --pcl-lib) PCL_LIB="$2"; shift 2 ;;
    --stage)   STAGE="$2"; shift 2 ;;
    --help)
      echo "usage: build-module-macos.sh [--pcl-inc <dir>] [--pcl-lib <dir>] [--stage <dir>]"
      exit 0 ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done

MODULE_DIR="$REPO_ROOT/integration/pixinsight/module"
HOST_DIR="$REPO_ROOT/integration/pixinsight/host"
CI_DIR="$REPO_ROOT/integration/pixinsight/ci"

MAKE_ARGS=()
[ -n "$PCL_INC" ] && MAKE_ARGS+=("PCLINCDIR=$PCL_INC")
[ -n "$PCL_LIB" ] && MAKE_ARGS+=("PCLLIBDIR=$PCL_LIB")

echo "=== [1/7] PCL API-version gate ==="
# See build-module-linux.sh's matching step for the full rationale: the
# build-pcl* hard gate never runs on this prebuilt-PCL path, so re-verify
# here against whichever include dir is actually about to be linked
# against -- asymmetric, not equality: a LOCAL value higher than the pin is
# a hard failure (silently locks out cores between the two versions); a
# LOCAL value lower than the pin is only a loud warning (locally installed
# headers can legitimately lag the pinned checkout's tag). Failing to read
# a PCL_API_Version at all is a hard failure either way, never a skip.
# shellcheck source=/dev/null
source "$CI_DIR/pcl-pin.env"
EFFECTIVE_PCL_INC="${PCL_INC:-/opt/PixInsight/include}"
API_HDR="$EFFECTIVE_PCL_INC/pcl/api/APIInterface.h"
[ -f "$API_HDR" ] || { echo "ERROR: cannot verify PCL_API_Version -- $API_HDR not found (PCLINCDIR misconfigured?)" >&2; exit 1; }
ACTUAL_HEX="$(grep -oE '^#define[[:space:]]+PCL_API_Version[[:space:]]+0x[0-9A-Fa-f]+' "$API_HDR" | grep -oE '0x[0-9A-Fa-f]+' | sed 's/^0x//')"
[ -n "$ACTUAL_HEX" ] || { echo "ERROR: no PCL_API_Version found in $API_HDR" >&2; exit 1; }
ACTUAL_DEC=$((16#$ACTUAL_HEX))
PINNED_DEC=$((16#$PCL_API_VERSION_HEX))
if [ "$ACTUAL_DEC" -gt "$PINNED_DEC" ]; then
  echo "ERROR: PCL_API_Version at $EFFECTIVE_PCL_INC is 0x${ACTUAL_HEX}, higher than the pinned 0x${PCL_API_VERSION_HEX}." >&2
  echo "This module would declare a higher API-version floor than intended, silently locking out cores between the two versions. Update PCL_API_VERSION_HEX in pcl-pin.env only deliberately, after re-verifying against the pinned checkout." >&2
  exit 1
elif [ "$ACTUAL_DEC" -lt "$PINNED_DEC" ]; then
  echo "WARNING: PCL_API_Version at $EFFECTIVE_PCL_INC is 0x${ACTUAL_HEX}, lower than the pinned 0x${PCL_API_VERSION_HEX} -- the locally installed headers likely lag the pinned GitLab checkout's tag. Continuing (the built module will declare whichever floor these headers actually carry; a manual PixInsight load/save smoke test settles which one)." >&2
else
  echo "PCL_API_Version confirmed 0x${PCL_API_VERSION_HEX} at $API_HDR"
fi

echo "=== [2/7] make clean ==="
make -C "$MODULE_DIR" clean

echo "=== [3/7] build BigTIFF-pxm.dylib ==="
build_log="$(mktemp)"
trap 'rm -f "$build_log"' EXIT
make -C "$MODULE_DIR" "${MAKE_ARGS[@]}" 2>&1 | tee "$build_log"

if grep -q 'warning:' "$build_log"; then
  echo "ERROR: warnings in module build" >&2
  grep 'warning:' "$build_log" >&2
  exit 1
fi

DYLIB="$MODULE_DIR/BigTIFF-pxm.dylib"
[ -f "$DYLIB" ] || { echo "BigTIFF-pxm.dylib not built" >&2; exit 1; }

echo "=== [4/7] nm gates ==="
# Host objects must be linked in: any undefined mpp:: symbol means they
# weren't (MMM's build-module-macos.sh phrasing, mechanically renamed).
if nm -u "$DYLIB" | grep -E ' _?_ZN3mpp' ; then
  echo "ERROR: undefined mpp:: symbols in BigTIFF-pxm.dylib (host objects not linked)" >&2
  exit 1
fi
echo "no undefined mpp:: symbols in $DYLIB"

# PCL entry points must be exported (Task 4's gate; BSD nm has no
# --defined-only, so filter on the "T"/"t" (text/code, defined) symbol type).
ENTRY_POINTS="$(nm "$DYLIB" | awk '$2 ~ /^[Tt]$/' | grep -i PixInsightModule || true)"
for ep in IdentifyPixInsightModule InitializePixInsightModule InstallPixInsightModule; do
  echo "$ENTRY_POINTS" | grep -q "$ep" || { echo "ERROR: $ep not exported by $DYLIB" >&2; exit 1; }
done
echo "PCL entry points exported:"
echo "$ENTRY_POINTS"

echo "=== [5/7] build mpp-ipc-worker ==="
( cd "$REPO_ROOT" && cargo build --release -p mpp-ipc-worker )
WORKER="$REPO_ROOT/target/release/mpp-ipc-worker"
[ -f "$WORKER" ] || { echo "mpp-ipc-worker not built" >&2; exit 1; }

echo "=== [6/7] host CTest golden suite ==="
CTEST_BUILD="$HOST_DIR/ci-build"
cmake -S "$HOST_DIR" -B "$CTEST_BUILD"
cmake --build "$CTEST_BUILD"
MPP_WORKER_EXE="$WORKER" ctest --test-dir "$CTEST_BUILD" --output-on-failure

echo "=== [7/7] stage artifacts ==="
DOC_SRC="$REPO_ROOT/integration/pixinsight"
[ -f "$DOC_SRC/README.md" ] || { echo "module documentation missing at $DOC_SRC/README.md" >&2; exit 1; }
[ -f "$DOC_SRC/LICENSE-PCL" ] || { echo "PCL license text missing at $DOC_SRC/LICENSE-PCL" >&2; exit 1; }

mkdir -p "$STAGE/bin" "$STAGE/doc"
cp -f "$DYLIB" "$STAGE/bin/BigTIFF-pxm.dylib"
cp -f "$WORKER" "$STAGE/bin/mpp-ipc-worker"
cp -f "$DOC_SRC/LICENSE-PCL" "$STAGE/bin/LICENSE-PCL"
cp -f "$DOC_SRC/README.md" "$STAGE/doc/README.md"
cp -f "$DOC_SRC/LICENSE-PCL" "$STAGE/doc/LICENSE-PCL"

echo "staged unsigned payload in $STAGE/bin: $(ls "$STAGE/bin")"
echo "staged documentation in $STAGE/doc: $(ls "$STAGE/doc")"
