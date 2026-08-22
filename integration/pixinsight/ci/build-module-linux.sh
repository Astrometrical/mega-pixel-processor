#!/usr/bin/env bash
# Build the PCL module (BigTIFF-pxm.so) + mpp-ipc-worker on Linux, run the
# module build gates (PCL_API_Version gate, zero -Wall -Wextra warnings, no
# undefined mpp:: symbols, PCL entry points exported), run the host CTest
# golden suite, and stage the unsigned artifact payload.
#
# Usage: build-module-linux.sh [--pcl-inc <dir>] [--pcl-lib <dir>] [--stage <dir>]
#   --pcl-inc / --pcl-lib override the module makefile's PCLINCDIR/PCLLIBDIR
#   (defaults, from module/makefile-x64: /opt/PixInsight/include and
#   $HOME/.local/pcl-build/lib -- both already present on this dev machine,
#   so a plain `build-module-linux.sh --stage <dir>` with no PCL flags
#   builds end-to-end here). CI passes a freshly built/cached PCL prefix's
#   include/ and lib/ (see build-pcl.sh --out) via these two flags instead.
#
# Adapted (mechanical rename + retarget) from MergeMosaic's
# integration/pixinsight/ci/build-module-linux.sh:
#   mmm -> mpp, mmm-pxm.so -> BigTIFF-pxm.so, mmm-ipc-worker -> mpp-ipc-worker,
#   mmm-core -> N/A (mpp's module gates are cargo/ctest only, no separate
#   `cargo test` call here -- module.yml runs `cargo test -p mpp-ipc
#   -p mpp-ipc-worker` itself, matching the Task 6 brief).
#   MergeMosaic's --pcl <prefix> (a single dir with include/+lib/ under it)
#   is split into --pcl-inc/--pcl-lib here because this machine's two PCL
#   locations (/opt/PixInsight/include, ~/.local/pcl-build/lib) are NOT
#   under one common prefix -- matching the module makefile's own
#   PCLINCDIR/PCLLIBDIR variable names directly avoids inventing a prefix
#   layout mpp doesn't otherwise use.
#   MergeMosaic's doc staging (integration/pixinsight/doc/tools/<Name>/*.html,
#   a PixInsight-native in-app help topic) has no mpp equivalent (Task 5
#   only created integration/pixinsight/README.md + LICENSE-PCL, no
#   doc/tools/*.html topic); staged doc content here is README.md +
#   LICENSE-PCL instead. LICENSE-PCL is staged *twice* -- once beside the
#   module binary in stage/bin/ (Task 5's compliance note: "a distributed
#   package containing BigTIFF-pxm.so should include LICENSE-PCL alongside
#   it") and once under stage/doc/ (this task's brief: "stage/doc/...
#   incl. LICENSE-PCL") -- satisfying both call-outs rather than guessing
#   which one is authoritative.
set -euo pipefail
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"

PCL_INC="" ; PCL_LIB="" ; STAGE="$REPO_ROOT/stage"
while [ $# -gt 0 ]; do
  case "$1" in
    --pcl-inc) PCL_INC="$2"; shift 2 ;;
    --pcl-lib) PCL_LIB="$2"; shift 2 ;;
    --stage)   STAGE="$2"; shift 2 ;;
    --help)
      echo "usage: build-module-linux.sh [--pcl-inc <dir>] [--pcl-lib <dir>] [--stage <dir>]"
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
# build-pcl.sh/build-pcl-macos.sh/build-pcl-windows.ps1 hard-gate the API
# version of a *freshly built* PCL (equality against the pinned checkout's
# own header), but that gate never runs on this path: a prebuilt PCLINCDIR
# is either passed via --pcl-inc or (as on this dev machine) falls through
# to the makefile's own default (/opt/PixInsight/include), and no
# pinned-checkout header exists in either case. Re-verify here against
# whichever include dir is actually about to be linked against -- but with
# asymmetric semantics, not equality: PCL_API_Version becomes the module's
# declared API-version floor, so the DANGEROUS direction is the local
# header claiming something HIGHER than the pin (the module would silently
# lock out cores between the two versions -- the failure MMM saw live).
# The local header claiming something LOWER than the pin is comparatively
# safe (a locally-installed PixInsight app's bundled headers can legitimately
# lag the pinned GitLab checkout's tag -- exactly the case on this dev
# machine, /opt/PixInsight/include's PixInsight 1.9.4 bundle vs. the pin's
# PCL 2.10.4 tag; see pcl-pin.env's provenance comment) and only warrants a
# loud warning, not a build failure. Either way, failing to read a
# PCL_API_Version at all is a hard failure, never a silent skip.
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

echo "=== [3/7] build BigTIFF-pxm.so ==="
build_log="$(mktemp)"
trap 'rm -f "$build_log"' EXIT
make -C "$MODULE_DIR" "${MAKE_ARGS[@]}" 2>&1 | tee "$build_log"

# The module build must be warning-free under -Wall -Wextra (binding gate;
# see the plan's "Global Constraints").
if grep -q 'warning:' "$build_log"; then
  echo "ERROR: -Wall -Wextra warnings in module build" >&2
  grep 'warning:' "$build_log" >&2
  exit 1
fi

SO="$MODULE_DIR/BigTIFF-pxm.so"
[ -f "$SO" ] || { echo "BigTIFF-pxm.so not built" >&2; exit 1; }

echo "=== [4/7] nm gates ==="
# The host objects (mpp_shm/mpp_protocol/mpp_host) must be linked in fully:
# any undefined mpp:: symbol means they weren't (MMM's build-module-linux.sh
# phrasing, mechanically renamed mmm -> mpp).
if nm -D -u "$SO" | grep -E '_ZN3mpp|mpp::' ; then
  echo "ERROR: undefined mpp:: symbols in BigTIFF-pxm.so (host objects not linked)" >&2
  exit 1
fi
echo "no undefined mpp:: symbols in $SO"

# The module must export PixInsight's PMINS entry points (Task 4's gate;
# a static-init-only object with no exported entry points is a silently
# unloadable module).
ENTRY_POINTS="$(nm -D --defined-only -C "$SO" | grep -i PixInsightModule || true)"
for ep in IdentifyPixInsightModule InitializePixInsightModule InstallPixInsightModule; do
  echo "$ENTRY_POINTS" | grep -q "$ep" || { echo "ERROR: $ep not exported by $SO" >&2; exit 1; }
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
cp -f "$SO" "$STAGE/bin/BigTIFF-pxm.so"
cp -f "$WORKER" "$STAGE/bin/mpp-ipc-worker"
cp -f "$DOC_SRC/LICENSE-PCL" "$STAGE/bin/LICENSE-PCL"
cp -f "$DOC_SRC/README.md" "$STAGE/doc/README.md"
cp -f "$DOC_SRC/LICENSE-PCL" "$STAGE/doc/LICENSE-PCL"

echo "staged unsigned payload in $STAGE/bin: $(ls "$STAGE/bin")"
echo "staged documentation in $STAGE/doc: $(ls "$STAGE/doc")"
