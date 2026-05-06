class Lctr < Formula
  desc "Fast local file metadata search"
  homepage "https://github.com/NotTanJune/locator"
  url "https://github.com/NotTanJune/locator/archive/refs/tags/v0.1.58.tar.gz"
  sha256 "3180b8058a4d617de71dab4809dd1040cf401699c6834172416ce0b0fb90cb4f"
  license "GPL-3.0-only"
  head "https://github.com/NotTanJune/locator.git", branch: "main"

  depends_on "rust" => :build

  def install
    system "cargo", "install", *std_cargo_args
  end

  def caveats
    <<~EOS
      To refresh Homebrew metadata before install or upgrade, run:
        brew update

      To enable scan auto-cd shell integration, run:
        lctr setup-shell

      This lets `lctr scan <dir>` move your current shell into <dir> after a successful scan.
    EOS
  end

  test do
    assert_match "lctr 0.1.58", shell_output("#{bin}/lctr --version")
  end
end
