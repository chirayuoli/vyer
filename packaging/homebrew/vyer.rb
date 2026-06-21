# Homebrew formula for Vyer (prebuilt binary).
#
# This belongs in a TAP repo named `chirayuoli/homebrew-tap` at `Formula/vyer.rb`,
# so users can `brew install chirayuoli/tap/vyer`.
#
# Fill the four `sha256` values after a release: each is in the matching
# `*.tar.gz.sha256` asset the release workflow uploads (or run
# `shasum -a 256 <archive>`). The release workflow can also be extended to bump
# these automatically (see docs/RELEASING.md).

class Vyer < Formula
  desc "Warm, local-first code-context engine for AI coding agents (MCP server)"
  homepage "https://github.com/chirayuoli/vyer"
  version "0.1.2"
  license any_of: ["MIT", "Apache-2.0"]

  on_macos do
    on_arm do
      url "https://github.com/chirayuoli/vyer/releases/download/v#{version}/vyer-v#{version}-aarch64-apple-darwin.tar.gz"
      sha256 "REPLACE_WITH_AARCH64_APPLE_DARWIN_SHA256"
    end
    on_intel do
      url "https://github.com/chirayuoli/vyer/releases/download/v#{version}/vyer-v#{version}-x86_64-apple-darwin.tar.gz"
      sha256 "REPLACE_WITH_X86_64_APPLE_DARWIN_SHA256"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/chirayuoli/vyer/releases/download/v#{version}/vyer-v#{version}-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "REPLACE_WITH_AARCH64_LINUX_SHA256"
    end
    on_intel do
      url "https://github.com/chirayuoli/vyer/releases/download/v#{version}/vyer-v#{version}-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "REPLACE_WITH_X86_64_LINUX_SHA256"
    end
  end

  def install
    bin.install "vyer"
  end

  test do
    assert_match "vyer", shell_output("#{bin}/vyer version")
  end
end
