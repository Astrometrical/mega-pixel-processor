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

MAKE_ARGS=()
[ -n "$PCL_INC" ] && MAKE_ARGS+=("PCLINCDIR=$PCL_INC")
[ -n "$PCL_LIB" ] && MAKE_ARGS+=("PCLLIBDIR=$PCL_LIB")

echo "=== [1/6] make clean ==="
make -C "$MODULE_DIR" clean

echo "=== [2/6] build BigTIFF-pxm.dylib ==="
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

echo "=== [3/6] nm gates ==="
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

echo "=== [4/6] build mpp-ipc-worker ==="
( cd "$REPO_ROOT" && cargo build --release -p mpp-ipc-worker )
WORKER="$REPO_ROOT/target/release/mpp-ipc-worker"
[ -f "$WORKER" ] || { echo "mpp-ipc-worker not built" >&2; exit 1; }

echo "=== [5/6] host CTest golden suite ==="
CTEST_BUILD="$HOST_DIR/ci-build"
cmake -S "$HOST_DIR" -B "$CTEST_BUILD"
cmake --build "$CTEST_BUILD"
MPP_WORKER_EXE="$WORKER" ctest --test-dir "$CTEST_BUILD" --output-on-failure

echo "=== [6/6] stage artifacts ==="
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
