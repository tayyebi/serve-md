# Homebrew formula for serve-md.
#
# Lives here for reference; the tap that serves it is tayyebi/homebrew-tap,
# where this file belongs at Formula/serve-md.rb. Update `version` and both
# sha256 values on each release, or generate it from the release workflow.
class ServeMd < Formula
  desc "Minimal Markdown/HTML server that also speaks MCP for AI agents"
  homepage "https://github.com/tayyebi/serve-md"
  version "0.5.0"
  license "MIT"

  # ripgrep is not strictly required — serve-md falls back to ag, then grep,
  # and everything except search works without any of them — but search is
  # most of the point of the webmcp plugin, so it is a hard dependency here.
  depends_on "ripgrep"

  # The releases are bare binaries, not archives, so each needs its own name
  # on install.
  on_linux do
    on_intel do
      url "https://github.com/tayyebi/serve-md/releases/download/v0.5.0/serve-md-linux-x86_64"
      sha256 "REPLACE_WITH_SHA256"
    end
    on_arm do
      url "https://github.com/tayyebi/serve-md/releases/download/v0.5.0/serve-md-linux-aarch64"
      sha256 "REPLACE_WITH_SHA256"
    end
  end

  # No macOS binaries are published yet, so mac users build from source.
  on_macos do
    depends_on "rust" => :build
    url "https://github.com/tayyebi/serve-md/archive/refs/tags/v0.5.0.tar.gz"
    sha256 "REPLACE_WITH_SHA256"
  end

  def install
    if OS.mac?
      system "cargo", "install", *std_cargo_args
    else
      bin.install Dir["serve-md-linux-*"].first => "serve-md"
    end
  end

  test do
    (testpath/"README.md").write("# Hello\n\nFrom a formula test.\n")
    assert_match "serve-md", shell_output("#{bin}/serve-md --version")
    assert_match "--plugin", shell_output("#{bin}/serve-md --help")
  end
end
