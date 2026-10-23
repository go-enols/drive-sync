# mobile/ios/ffi — Rust 静态库放置目录（构建产物，不提交）

`libdrive_mobile.a` 由以下任一方式生成后放入对应子目录，Xcode 才能链接：

```bash
# 方式 A：仓库脚本（推荐，macOS）
bash mobile/ios/scripts/build-ffi.sh

# 方式 B：手动（CI 同款）
rustup target add aarch64-apple-ios aarch64-apple-ios-sim x86_64-apple-ios
cargo build -p drive-mobile --release --target aarch64-apple-ios
cargo build -p drive-mobile --release --target aarch64-apple-ios-sim
cargo build -p drive-mobile --release --target x86_64-apple-ios
mkdir -p mobile/ios/ffi/iphoneos mobile/ios/ffi/iphonesimulator
cp target/aarch64-apple-ios/release/libdrive_mobile.a mobile/ios/ffi/iphoneos/
lipo -create \
  target/aarch64-apple-ios-sim/release/libdrive_mobile.a \
  target/x86_64-apple-ios/release/libdrive_mobile.a \
  -output mobile/ios/ffi/iphonesimulator/libdrive_mobile.a
```

- `iphoneos/`：真机（arm64），Xcode 用个人 Apple ID 自动签名时使用；
- `iphonesimulator/`：模拟器（arm64 + x86_64 通用），CI 构建模拟器包使用；
- 头文件（`drive_mobile.h`）不在这里：来自 `crates/mobile-core/include/`，由 `project.yml` 的 `HEADER_SEARCH_PATHS` 指向。

> 这些 `.a` 被 `mobile/ios/.gitignore` 忽略；GitHub Actions 会构建并作为 `drive-mobile-ios-staticlibs` artifact 上传。
