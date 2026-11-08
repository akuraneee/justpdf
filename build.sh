#!/usr/bin/env bash
# Builds JustPDF APKs without Gradle:
#   cargo (NDK clang as linker) -> libjustpdf.so
#   prebuilt PDFium             -> libpdfium.so
#   javac + d8                  -> classes.dex (the ~300 line Java shim)
#   aapt2                       -> manifest + resources
#   zipalign + apksigner        -> build/out/justpdf-<abi>.apk and justpdf-universal.apk
#
# Environment:
#   ANDROID_HOME (or ANDROID_SDK_ROOT)  Android SDK with build-tools and platforms
#   ANDROID_NDK_HOME                    NDK (defaults to the newest in $ANDROID_HOME/ndk)
#   ABIS                                default: "arm64-v8a armeabi-v7a x86_64"
#   PROFILE                             release (default) or debug
#   JUSTPDF_KEYSTORE, JUSTPDF_KEYSTORE_PASS, JUSTPDF_KEY_ALIAS, JUSTPDF_KEY_PASS
#                                       signing key; defaults to ~/.android/debug.keystore
set -euo pipefail
cd "$(dirname "$0")"
ROOT=$PWD

ABIS=${ABIS:-"arm64-v8a armeabi-v7a x86_64"}
PROFILE=${PROFILE:-release}
MIN_SDK=26
TARGET_SDK=36
BUILD_TOOLS=${BUILD_TOOLS:-36.0.0}
VERSION_NAME=${VERSION_NAME:-$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)}
VERSION_CODE=${VERSION_CODE:-$(git rev-list --count HEAD 2>/dev/null || echo 1)}

die() { echo "error: $*" >&2; exit 1; }

SDK=${ANDROID_HOME:-${ANDROID_SDK_ROOT:-}}
[[ -n $SDK && -d $SDK ]] || die "set ANDROID_HOME to the Android SDK"
NDK=${ANDROID_NDK_HOME:-${ANDROID_NDK_ROOT:-}}
if [[ -z $NDK ]]; then
  NDK=$(ls -d "$SDK"/ndk/* 2>/dev/null | sort -V | tail -1 || true)
fi
[[ -n $NDK && -d $NDK ]] || die "no NDK found; set ANDROID_NDK_HOME"
BT=$SDK/build-tools/$BUILD_TOOLS
[[ -d $BT ]] || BT=$(ls -d "$SDK"/build-tools/* | sort -V | tail -1)
ANDROID_JAR=$SDK/platforms/android-$TARGET_SDK/android.jar
[[ -f $ANDROID_JAR ]] || die "missing $ANDROID_JAR (sdkmanager \"platforms;android-$TARGET_SDK\")"
case "$(uname -s)" in
  Linux) HOST=linux-x86_64 ;;
  Darwin) HOST=darwin-x86_64 ;;
  *) die "unsupported host" ;;
esac
LLVM=$NDK/toolchains/llvm/prebuilt/$HOST/bin

abi_info() {
  case $1 in
    arm64-v8a)   TRIPLE=aarch64-linux-android;   CLANG=aarch64-linux-android;    PDFIUM=android-arm64 ;;
    armeabi-v7a) TRIPLE=armv7-linux-androideabi; CLANG=armv7a-linux-androideabi; PDFIUM=android-arm ;;
    x86_64)      TRIPLE=x86_64-linux-android;    CLANG=x86_64-linux-android;     PDFIUM=android-x64 ;;
    x86)         TRIPLE=i686-linux-android;      CLANG=i686-linux-android;       PDFIUM=android-x86 ;;
    *) die "unknown ABI $1" ;;
  esac
}

OUT=$ROOT/build
rm -rf "$OUT/lib" "$OUT/classes" "$OUT/dex" "$OUT/apk"
mkdir -p "$OUT/lib" "$OUT/classes" "$OUT/dex" "$OUT/apk" "$OUT/out"

echo "==> PDFium"
plats=()
for abi in $ABIS; do abi_info "$abi"; plats+=("$PDFIUM"); done
scripts/fetch-pdfium.sh "${plats[@]}"

echo "==> Rust ($PROFILE)"
cargo_flags=(--lib)
[[ $PROFILE == release ]] && cargo_flags+=(--release)
for abi in $ABIS; do
  abi_info "$abi"
  up=$(echo "$TRIPLE" | tr 'a-z-' 'A-Z_')
  low=${TRIPLE//-/_}
  export "CARGO_TARGET_${up}_LINKER=$LLVM/${CLANG}${MIN_SDK}-clang"
  export "CC_${low}=$LLVM/${CLANG}${MIN_SDK}-clang"
  export "AR_${low}=$LLVM/llvm-ar"
  export "CARGO_TARGET_${up}_RUSTFLAGS=-C link-arg=-Wl,-z,max-page-size=16384"
  cargo build "${cargo_flags[@]}" --target "$TRIPLE"
  mkdir -p "$OUT/lib/$abi"
  cp "target/$TRIPLE/$PROFILE/libjustpdf.so" "$OUT/lib/$abi/"
  cp "third_party/pdfium/$PDFIUM/lib/libpdfium.so" "$OUT/lib/$abi/"
  "$LLVM/llvm-strip" --strip-unneeded "$OUT/lib/$abi/libjustpdf.so"
done

echo "==> Java"
javac -nowarn -Xlint:-options -source 8 -target 8 -encoding UTF-8 \
  -bootclasspath "$ANDROID_JAR:$BT/core-lambda-stubs.jar" -d "$OUT/classes" $(find android/java -name '*.java')
"$BT/d8" --release --min-api $MIN_SDK --lib "$ANDROID_JAR" --output "$OUT/dex" $(find "$OUT/classes" -name '*.class')

echo "==> Resources"
"$BT/aapt2" compile --dir android/res -o "$OUT/apk/res.zip"
"$BT/aapt2" link -o "$OUT/apk/base.apk" -I "$ANDROID_JAR" \
  --manifest android/AndroidManifest.xml \
  --min-sdk-version $MIN_SDK --target-sdk-version $TARGET_SDK \
  --version-code "$VERSION_CODE" --version-name "$VERSION_NAME" \
  "$OUT/apk/res.zip"

echo "==> Signing key"
KS=${JUSTPDF_KEYSTORE:-$HOME/.android/debug.keystore}
KS_PASS=${JUSTPDF_KEYSTORE_PASS:-android}
KEY_ALIAS=${JUSTPDF_KEY_ALIAS:-androiddebugkey}
KEY_PASS=${JUSTPDF_KEY_PASS:-$KS_PASS}
if [[ ! -f $KS ]]; then
  [[ -z ${JUSTPDF_KEYSTORE:-} ]] || die "keystore $KS not found"
  echo "creating debug keystore at $KS"
  mkdir -p "$(dirname "$KS")"
  keytool -genkeypair -keystore "$KS" -storepass android -keypass android -alias androiddebugkey \
    -keyalg RSA -keysize 2048 -validity 10000 -dname "CN=Android Debug,O=Android,C=US" >/dev/null
fi

package() {
  local name=$1; shift
  python3 scripts/package-apk.py "$OUT/apk/base.apk" "$OUT/apk/$name.unaligned.apk" "$OUT/dex/classes.dex" "$OUT/lib" "$@"
  "$BT/zipalign" -f -P 16 4 "$OUT/apk/$name.unaligned.apk" "$OUT/apk/$name.aligned.apk"
  "$BT/apksigner" sign --ks "$KS" --ks-pass "pass:$KS_PASS" --ks-key-alias "$KEY_ALIAS" --key-pass "pass:$KEY_PASS" \
    --out "$OUT/out/justpdf-$name.apk" "$OUT/apk/$name.aligned.apk"
  echo "    $(du -h "$OUT/out/justpdf-$name.apk" | cut -f1)  build/out/justpdf-$name.apk"
}

echo "==> APKs"
rm -f "$OUT"/out/*.apk "$OUT"/out/*.idsig
for abi in $ABIS; do package "$abi" "$abi"; done
set -- $ABIS
if [[ $# -gt 1 ]]; then package universal $ABIS; fi
rm -f "$OUT"/out/*.idsig
