class Lctr < Formula
  desc "Fast local file metadata search"
  homepage "https://github.com/NotTanJune/locator"
  url "https://github.com/NotTanJune/locator/releases/download/v0.3.2/lctr-aarch64-apple-darwin.tar.gz"
  sha256 "ee15af4b424fdf4ec35b5f83792d5a885815e7a64147e65c4478e29f46a4ba6f"
  license "GPL-3.0-only"

  head do
    url "https://github.com/NotTanJune/locator.git", branch: "main"

    depends_on "rust" => :build
  end

  def install
    if build.head?
      system "cargo", "install", *std_cargo_args
    else
      unless OS.mac?
        odie <<~EOS
          Stable Homebrew install currently ships the Apple silicon macOS binary only.
          On Linux, use `brew install --HEAD lctr` to build from source.
        EOS
      end

      unless Hardware::CPU.arm?
        odie <<~EOS
          Stable Homebrew install currently ships the Apple silicon macOS binary only.
          On Intel macOS, use `brew install --HEAD lctr` to build from source.
        EOS
      end

      bin.install "lctr"
    end
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
    assert_match "lctr 0.3.2", shell_output("#{bin}/lctr --version")
  end
end
