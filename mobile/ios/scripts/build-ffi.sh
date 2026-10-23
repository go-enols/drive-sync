#!/usr/bin/env bash
# 构建 drive-mobile 的 iOS 静态库（device + simulator）并放到 mobile/ios/ffi/。
#
#   bash mobile/ios/scripts/build-ffi.sh
#
# 前置：macOS + Xcode CLT + rustup；首次会执行 rustup target add。
# 产物（已被 mobile/ios/.gitignore 忽略，不提交）：
#   mobile/ios/ffi/iphoneos/libdrive_mobile.a        aarch64-apple-ios（真机）
#   mobile/ios/ffi/iphonesimulator/libdrive_mobile.a aarch64-apple-ios-sim + x86_64-apple-ios（通用模拟器）
#
# CI（.github/workflows/ios.yml）复用本脚本，macos-14 runner 上无需签名。
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
IOS_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
ROOT_DIR="$(cd "${IOS_DIR}/../.." && pwd)"

DEVICE_TARGET="aarch64-apple-ios"
SIM_ARM_TARGET="aarch64-apple-ios-sim"
SIM_X86_TARGET="x86_64-apple-ios"

HEADER="${ROOT_DIR}/crates/mobile-core/include/drive_mobile.h"
if [[ ! -f "${HEADER}" ]]; then
  echo "错误：缺少 C ABI 头文件 ${HEADER}" >&2
  echo "      （由移动核心线程提供；没有头文件无法用桥接头编译 Swift）" >&2
  exit 1
fi

# iOS 最低部署版本必须与 Xcode 工程一致（否则 zstd/blake3 等 C 依赖会按新 SDK 编译而链接到旧 min version）。
export IPHONEOS_DEPLOYMENT_TARGET="${IPHONEOS_DEPLOYMENT_TARGET:-16.0}"
export MACOSX_DEPLOYMENT_TARGET="${MACOSX_DEPLOYMENT_TARGET:-16.0}"

CARGO_FLAGS="${CARGO_FLAGS:-}"

echo "==> rustup target add ${DEVICE_TARGET} ${SIM_ARM_TARGET} ${SIM_X86_TARGET}"
rustup target add "${DEVICE_TARGET}" "${SIM_ARM_TARGET}" "${SIM_X86_TARGET}"

echo "==> cargo build -p drive-mobile --release（device: ${DEVICE_TARGET}）"
cargo rustc -p drive-mobile --release --target "${DEVICE_TARGET}" --lib --crate-type staticlib ${CARGO_FLAGS}

echo "==> cargo build -p drive-mobile --release（sim: ${SIM_ARM_TARGET}）"
cargo rustc -p drive-mobile --release --target "${SIM_ARM_TARGET}" --lib --crate-type staticlib ${CARGO_FLAGS}

echo "==> cargo build -p drive-mobile --release（sim: ${SIM_X86_TARGET}）"
cargo rustc -p drive-mobile --release --target "${SIM_X86_TARGET}" --lib --crate-type staticlib ${CARGO_FLAGS}

DEVICE_LIB="${ROOT_DIR}/target/${DEVICE_TARGET}/release/libdrive_mobile.a"
SIM_ARM_LIB="${ROOT_DIR}/target/${SIM_ARM_TARGET}/release/libdrive_mobile.a"
SIM_X86_LIB="${ROOT_DIR}/target/${SIM_X86_TARGET}/release/libdrive_mobile.a"

for lib in "${DEVICE_LIB}" "${SIM_ARM_LIB}" "${SIM_X86_LIB}"; do
  if [[ ! -f "${lib}" ]]; then
    echo "错误：未找到静态库 ${lib}" >&2
    exit 1
  fi
done

mkdir -p "${IOS_DIR}/ffi/iphoneos" "${IOS_DIR}/ffi/iphonesimulator"
cp "${DEVICE_LIB}" "${IOS_DIR}/ffi/iphoneos/libdrive_mobile.a"
lipo -create "${SIM_ARM_LIB}" "${SIM_X86_LIB}" -output "${IOS_DIR}/ffi/iphonesimulator/libdrive_mobile.a"

echo "==> 静态库架构："
lipo -info "${IOS_DIR}/ffi/iphoneos/libdrive_mobile.a"
lipo -info "${IOS_DIR}/ffi/iphonesimulator/libdrive_mobile.a"

echo ""
echo "完成。下一步："
echo "  cd mobile/ios && xcodegen generate && open DriveSync.xcodeproj"
echo "  或命令行构建模拟器 App："
echo "  xcodebuild -project mobile/ios/DriveSync.xcodeproj -scheme DriveSync \\"
echo "    -configuration Debug -sdk iphonesimulator -destination 'generic/platform=iOS Simulator' \\"
echo "    -derivedDataPath mobile/ios/build CODE_SIGNING_ALLOWED=NO build"
