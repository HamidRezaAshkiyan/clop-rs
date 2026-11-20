cask "clop-rs" do
  arch arm: "arm64"

  version "0.4.1"
  sha256 "REPLACE_WITH_MACOS_PKG_SHA256"

  url "https://github.com/HamidRezaAshkiyan/clop-rs/releases/download/v#{version}/clop-rs-v#{version}-macos-#{arch}.pkg"
  name "clop-rs"
  desc "Minimal-footprint automatic clipboard, downloads, file, PDF & video optimizer"
  homepage "https://github.com/HamidRezaAshkiyan/clop-rs"

  depends_on macos: ">= :big_sur"

  pkg "clop-rs-v#{version}-macos-#{arch}.pkg"

  uninstall pkgutil: "com.hamidrezaashkiyan.clop-rs.pkg",
            delete:  [
              "/Applications/clop-rs.app",
              "/usr/local/bin/clop-rs",
              "/usr/local/bin/clop-cli",
            ]

  zap trash: [
    "~/Library/Application Support/clop-rs",
    "~/Library/LaunchAgents/com.hamidrezaashkiyan.clop-rs.plist",
  ]
end
