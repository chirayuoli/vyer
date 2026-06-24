# Homebrew formula for Vyer (prebuilt binary).
#
# This belongs in a TAP repo named `chirayuoli/homebrew-tap` at `Formula/vyer.rb`,
# so users can `brew install chirayuoli/tap/vyer`.
#
# Fill the three `sha256` values after a release: each is in the matching
# `*.tar.gz.sha256` asset the release workflow uploads (or run
# `shasum -a 256 <archive>`). The release workflow can also be extended to bump
# these automatically (see docs/RELEASING.md).

class Vyer < Formula
  desc "Warm, local-first code-context engine for AI coding agents (MCP server)"
  homepage "https://github.com/chirayuoli/vyer"
  version "0.2.0"
  license any_of: ["MIT", "Apache-2.0"]

  on_macos do
    on_arm do
      url "https://github.com/chirayuoli/vyer/releases/download/v#{version}/vyer-v#{version}-aarch64-apple-darwin.tar.gz"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000" # TODO: fill from v0.2.0 release .sha256 assets
    end
    # Intel macOS prebuilt is intentionally not shipped (flaky macos-13 runner);
    # on Intel Macs: cargo install --git https://github.com/chirayuoli/vyer vyer-server
  end

  on_linux do
    on_arm do
      url "https://github.com/chirayuoli/vyer/releases/download/v#{version}/vyer-v#{version}-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000" # TODO: fill from v0.2.0 release .sha256 assets
    end
    on_intel do
      url "https://github.com/chirayuoli/vyer/releases/download/v#{version}/vyer-v#{version}-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000" # TODO: fill from v0.2.0 release .sha256 assets
    end
  end

  def install
    bin.install "vyer"
  end

  test do
    assert_match "vyer", shell_output("#{bin}/vyer version")
  end
end
