class Lctr < Formula
  desc "Fast local file metadata search"
  homepage "https://github.com/NotTanJune/locator"
  url "https://github.com/NotTanJune/locator.git",
      tag: "v0.1.39"
  license "GPL-3.0-only"
  head "https://github.com/NotTanJune/locator.git", branch: "main"

  depends_on "rust" => :build

  def install
    system "cargo", "install", *std_cargo_args
  end

  test do
    assert_match "lctr 0.1.39", shell_output("#{bin}/lctr --version")
  end
end
