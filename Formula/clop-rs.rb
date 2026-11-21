class ClopRs < Formula
  desc "Minimal-footprint clipboard, file, PDF & video optimizer (GUI + clop-cli)"
  homepage "https://github.com/HamidRezaAshkiyan/clop-rs"
  version "0.4.4"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/HamidRezaAshkiyan/clop-rs/releases/download/v#{version}/clop-rs-v#{version}-macos-arm64.tar.gz"
      sha256 "dc0eee0fcef7da27e205a47e194028c84184acb73f6543710d9cd47baa382618"
    end
  end

  head "https://github.com/HamidRezaAshkiyan/clop-rs.git", branch: "main"

  depends_on "rust" => :build if build.head?

  def install
    if build.head?
      system "cargo", "install", *std_cargo_args(path: ".")
    else
      bin.install "clop-rs"
      bin.install "clop-cli" if File.exist?("clop-cli")
    end
  end

  def caveats
    <<~EOS
      Installed binaries:
        • clop-rs   (Menu Bar app + Floating HUD + Dashboard UI: `clop-rs ui`)
        • clop-cli  (Standalone terminal CLI: `clop-cli --help`)

      To install the macOS .app bundle in /Applications as well, use the Cask:
        brew install --cask HamidRezaAshkiyan/clop-rs/clop-rs
    EOS
  end

  test do
    assert_match "clop-cli", shell_output("#{bin}/clop-cli --version")
    assert_match "Supported File Extensions", shell_output("#{bin}/clop-cli formats")
  end
end
