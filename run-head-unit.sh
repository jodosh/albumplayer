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

# The Desktop Head Unit links against LLVM's libc++, which many distributions do
# not install by default. Rather than requiring root, fetch the package and
# unpack it here.
#
# It must be the real thing: the copy shipped with the Android emulator loads
# but segfaults, being a different build against a different ABI. A near-miss on
# a C++ standard library is worse than an outright missing one, because it fails
# at run time instead of at link time.
LIBS="${TMPDIR:-/tmp}/dhu-libs"
if [[ ! -f "$LIBS/libc++.so.1" ]]; then
  mkdir -p "$LIBS"
  if command -v pacman >/dev/null; then
    work="$(mktemp -d)"
    trap 'rm -rf "$work"' EXIT
    ( cd "$work"
      for url in $(pacman -Sp libc++ libc++abi 2>/dev/null); do curl -sSLO "$url"; done
      for pkg in *.pkg.tar.zst; do tar --use-compress-program=unzstd -xf "$pkg"; done
      cp -a usr/lib/libc++*.so* "$LIBS/" )
  else
    echo "Install libc++ and libc++abi, then rerun." >&2
    exit 1
  fi
fi

# The head unit opens ALSA, which on a PipeWire system reaches PipeWire and then
# segfaults during thread cleanup — the core dump lands in libpipewire, not in
# the head unit binary. Redirecting ALSA to a null device is not enough on its
# own; the audio server has to be out of reach entirely. Audio is not needed to
# look at the interface.
ALSA_CONF="${TMPDIR:-/tmp}/dhu-alsa-null.conf"
cat > "$ALSA_CONF" <<'ALSA'
</usr/share/alsa/alsa.conf>
pcm.!default { type null }
ctl.!default { type null }
ALSA
export ALSA_CONFIG_PATH="$ALSA_CONF"
export PULSE_SERVER=none
# An empty runtime directory hides the PipeWire and PulseAudio sockets.
export XDG_RUNTIME_DIR="${TMPDIR:-/tmp}/dhu-no-audio"
mkdir -p "$XDG_RUNTIME_DIR"

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
echo "starting the head unit."
echo "The phone's head unit server is one-shot: start it again from"
echo "Android Auto -> ⋮ -> Start head unit server before each run."
exec env LD_LIBRARY_PATH="$LIBS:." ./desktop-head-unit "$@"
