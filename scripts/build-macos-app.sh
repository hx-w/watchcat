#!/bin/sh
set -eu

project_dir=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
app_dir="$project_dir/dist/Watchcat.app"
contents="$app_dir/Contents"
iconset="$project_dir/dist/Watchcat.iconset"

cargo build --manifest-path "$project_dir/Cargo.toml" --release --bin watchcat --bin watchcatd
swift build --package-path "$project_dir/clients/macos" -c release --product Watchcat

rm -rf "$app_dir" "$iconset"
mkdir -p "$contents/MacOS" "$contents/Resources" "$contents/Library/LaunchAgents"
cp "$project_dir/clients/macos/Info.plist" "$contents/Info.plist"
cp "$project_dir/clients/macos/.build/release/Watchcat" "$contents/MacOS/Watchcat"
cp "$project_dir/clients/macos/Sources/WatchcatApp/Resources/WatchcatLogo.png" "$contents/Resources/WatchcatLogo.png"
mkdir -p "$iconset"
for size in 16 32 128 256 512; do
  double=$((size * 2))
  sips -z "$size" "$size" "$project_dir/clients/macos/Sources/WatchcatApp/Resources/WatchcatLogo.png" --out "$iconset/icon_${size}x${size}.png" >/dev/null
  sips -z "$double" "$double" "$project_dir/clients/macos/Sources/WatchcatApp/Resources/WatchcatLogo.png" --out "$iconset/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$iconset" -o "$contents/Resources/Watchcat.icns"
rm -rf "$iconset"
cp "$project_dir/target/release/watchcatd" "$contents/Resources/watchcatd"
cp "$project_dir/target/release/watchcat" "$contents/Resources/watchcat"
cp "$project_dir/clients/macos/ai.watchcat.watchcatd.plist" "$contents/Library/LaunchAgents/ai.watchcat.watchcatd.plist"

chmod 755 "$contents/MacOS/Watchcat" "$contents/Resources/watchcatd" "$contents/Resources/watchcat"
codesign --force --deep --sign - "$app_dir"
printf 'Built %s\n' "$app_dir"
