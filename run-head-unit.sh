#!/usr/bin/env bash
# Render Android Auto on this machine instead of in the car.
#
# The phone stays on USB, so adb — and therefore logcat — keeps working while
# the car interface is on screen. That is the only practical way to debug
# something that only misbehaves when projected.
#
# One-time setup on the phone:
#   1. Open Android Auto settings
#   2. Scroll to the bottom and tap "Version" about ten times, accept the prompt
#   3. ⋮ menu -> Developer settings -> enable "Unknown sources"
#   4. ⋮ menu -> "Start head unit server"
set -euo pipefail

ANDROID_HOME="${ANDROID_HOME:-$HOME/Android/Sdk}"
ADB="$ANDROID_HOME/platform-tools/adb"
DHU_DIR="$ANDROID_HOME/extras/google/auto"

# The Desktop Head Unit wants LLVM's libc++, which many distributions do not
# install. The SDK ships a copy, so borrow that rather than adding a package.
LIBS="${TMPDIR:-/tmp}/dhu-libs"
mkdir -p "$LIBS"
for lib in libc++.so.1 libc++abi.so.1; do
  [[ -f "$LIBS/$lib" ]] && continue
  for candidate in \
      "$ANDROID_HOME/emulator/lib64/$lib" \
      "/opt/android-studio/plugins/android-ndk/resources/lldb/lib64/$lib"; do
    [[ -f "$candidate" ]] && cp -f "$candidate" "$LIBS/" && break
  done
done

DEVICE="${ANDROID_SERIAL:-$("$ADB" devices | awk 'NR==2 {print $1}')}"
if [[ -z "${DEVICE:-}" ]]; then
  echo "No device on adb. Plug the phone in with USB debugging enabled." >&2
  exit 1
fi
echo "device: $DEVICE"

# The head unit server on the phone listens here; forward it over USB.
"$ADB" -s "$DEVICE" forward tcp:5277 tcp:5277 >/dev/null
echo "forwarded tcp:5277"

cd "$DHU_DIR"
echo "starting the head unit — if it exits immediately, the phone's head unit"
echo "server is not running (Android Auto -> ⋮ -> Start head unit server)"
exec env LD_LIBRARY_PATH="$LIBS:." ./desktop-head-unit "$@"
