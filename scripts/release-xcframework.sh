#!/usr/bin/env bash
set -euo pipefail

# Build the XCFramework, zip it, and create a GitHub Release.
#
# Usage:
#   ./scripts/release-xcframework.sh <version>
#   ./scripts/release-xcframework.sh 0.1.0
#
# Prerequisites:
#   - gh CLI authenticated
#   - Rust cross-compilation targets installed (run `make -C zcash-voting-ffi install`)

VERSION="${1:?Usage: $0 <version>}"
REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
FFI_DIR="$REPO_ROOT/zcash-voting-ffi"
ZIP_NAME="zcash_voting_ffiFFI.xcframework.zip"
ZIP_PATH="/tmp/$ZIP_NAME"

echo "=== Building XCFramework (dev — arm64 only) ==="
make -C "$FFI_DIR" dev

echo ""
echo "=== Copying Swift bindings to repo root ==="
mkdir -p "$REPO_ROOT/Sources/ZcashVotingFFI"
cp "$FFI_DIR/Sources/ZcashVotingFFI/zcash_voting_ffi.swift" "$REPO_ROOT/Sources/ZcashVotingFFI/"

echo ""
echo "=== Zipping XCFramework ==="
cd "$FFI_DIR"
rm -f "$ZIP_PATH"
zip -r "$ZIP_PATH" releases/zcash_voting_ffiFFI.xcframework
echo "Zip size: $(du -h "$ZIP_PATH" | cut -f1)"

echo ""
echo "=== Computing checksum ==="
CHECKSUM=$(swift package compute-checksum "$ZIP_PATH")
echo "Checksum: $CHECKSUM"

echo ""
echo "=== Updating Package.swift ==="
cd "$REPO_ROOT"
# Update the URL version and checksum in Package.swift
sed -i '' "s|/releases/download/[^/]*/|/releases/download/$VERSION/|" Package.swift
sed -i '' "s|checksum: \"[a-f0-9]*\"|checksum: \"$CHECKSUM\"|" Package.swift

echo ""
echo "=== Committing ==="
cd "$REPO_ROOT"
git add Package.swift Sources/ZcashVotingFFI/zcash_voting_ffi.swift
git commit -m "Release $VERSION — update Swift bindings and Package.swift checksum"

echo ""
echo "=== Pushing ==="
git push

echo ""
echo "=== Creating GitHub Release ==="
gh release create "$VERSION" "$ZIP_PATH" \
    --repo z-cale/librustvoting \
    --title "v$VERSION" \
    --notes "XCFramework binary for SPM consumption.

Zip checksum: \`$CHECKSUM\`
Platforms: ios-arm64, ios-arm64-simulator, macos-arm64"

echo ""
echo "=== Done ==="
echo "Release: https://github.com/z-cale/librustvoting/releases/tag/$VERSION"
echo ""
echo "To use in Package.swift:"
echo '  .package(url: "https://github.com/z-cale/librustvoting.git", exact: "'"$VERSION"'")'
