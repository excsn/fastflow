cask "fastflow" do
  version "@VERSION@"
  sha256 "@SHA256@"

  url "https://github.com/excsn/fastflow/releases/download/v#{version}/fastflow-#{version}.dmg"
  name "fastflow"
  desc "Screen recorder that speeds through idle stretches"
  homepage "https://github.com/excsn/fastflow"

  depends_on arch: :arm64
  depends_on macos: ">= :monterey"
  depends_on formula: "ffmpeg"
  depends_on formula: "webp"

  app "fastflow.app"
  binary "#{appdir}/fastflow.app/Contents/MacOS/fastflow"

  uninstall quit: "com.excsn.mac.fastflow"

  zap trash: [
    "~/Library/Application Support/com.excsn.mac.fastflow",
    "~/Library/Logs/fastflow",
  ]

  caveats <<~EOS
    fastflow lives in the menu bar. On first launch it asks for Screen Recording
    and Input Monitoring. Recordings are kept in ~/Movies/fastflow.
  EOS
end
