cask "clop-rs" do
  arch arm: "arm64"

  version "1.0.0"
  sha256 "bfa1b292f23f3f1c1401fcf2a650db0b2edb079746fcfd4b0a4404ad8a4b87a9"

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
