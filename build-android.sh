#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"

export ANDROID_HOME="${ANDROID_HOME:-$HOME/Android/Sdk}"
NDK_VER="${NDK_VER:-30.0.14904198}"
export ANDROID_NDK_HOME="${ANDROID_NDK_HOME:-$ANDROID_HOME/ndk/$NDK_VER}"
NDK_BIN="$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/linux-x86_64/bin"
export PATH="$NDK_BIN:$PATH"

MODE="${1:-debug}"
CARGO_FLAGS=()
if [[ "$MODE" == "release" ]]; then CARGO_FLAGS+=(--release); fi

# target triple -> Android ABI dir
declare -A ABIS=(
  [aarch64-linux-android]=arm64-v8a
  [x86_64-linux-android]=x86_64
)

for TARGET in "${!ABIS[@]}"; do
  ABI="${ABIS[$TARGET]}"
  echo "==> cargo build -p ferrite-android --target $TARGET ($MODE)"
  cargo build -p ferrite-android --target "$TARGET" "${CARGO_FLAGS[@]}"
  JNI_DIR="android-app/app/src/main/jniLibs/$ABI"
  mkdir -p "$JNI_DIR"
  cp "target/$TARGET/$MODE/libferrite_android.so" "$JNI_DIR/"
  echo "==> copied .so -> $JNI_DIR"
done

echo "==> gradle assembleDebug"
(cd android-app && ./gradlew assembleDebug)

APK="android-app/app/build/outputs/apk/debug/app-debug.apk"
echo "==> APK at $APK"
echo "install: adb install -r $APK"
