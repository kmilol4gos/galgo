# PROPOSAL ONLY: replace the checksum after downloading and verifying the actual
# arm64 release asset. Do not publish or install this cask with the placeholder.
cask "galgo" do
  version "0.1.0"
  sha256 "REPLACE_WITH_VERIFIED_ARM64_SHA256"

  url "https://github.com/kmilol4gos/galgo/releases/download/v#{version}/galgo-v#{version}-macos-arm64.tar.gz"
  name "galgo"
  desc "Terminal for macOS with optional agent orchestration"
  homepage "https://github.com/kmilol4gos/galgo"

  # The workflow publishes only the build runner's architecture. Use this
  # template only if the release actually contains the arm64-named asset.
  depends_on macos: ">= :sonoma"
  depends_on arch: :arm64

  app "galgo.app"
  binary "bin/galgo"
  binary "bin/galgod"
  binary "bin/galgo-mcp"
  binary "bin/galgo-gui"
end
