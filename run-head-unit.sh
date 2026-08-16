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

# The head unit segfaults on startup on a PipeWire system. The core dump points
# at libpipewire rather than the head unit: it enumerates ALSA devices, ALSA
# loads its PipeWire plugin, and a thread blocked in read is then cancelled with
# an unwind that faults.
#
# Redirecting ALSA's default device is not enough — the stock alsa.conf pulls in
# alsa.conf.d, which is where the PipeWire plugin is wired up, so including it
# reintroduces exactly what needs avoiding. Pointing the plugin directory
# somewhere empty is what actually stops it loading. The audio sockets are
# hidden too, for the paths that reach PulseAudio directly.
#
# None of this costs anything here: audio is irrelevant to inspecting the
# interface, and the car plays through the phone in any case.
export ALSA_PLUGIN_DIR="${TMPDIR:-/tmp}/dhu-no-alsa-plugins"
export XDG_RUNTIME_DIR="${TMPDIR:-/tmp}/dhu-no-audio"
export PULSE_SERVER=none
mkdir -p "$ALSA_PLUGIN_DIR" "$XDG_RUNTIME_DIR"

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
