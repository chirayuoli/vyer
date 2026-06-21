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
  version "0.1.4"
  license any_of: ["MIT", "Apache-2.0"]

  on_macos do
    on_arm do
      url "https://github.com/chirayuoli/vyer/releases/download/v#{version}/vyer-v#{version}-aarch64-apple-darwin.tar.gz"
      sha256 "e6e24666cdbc05f750fb6c3cd7b5495cd90373e269918e8ca73f045033bda9ec"
    end
    # Intel macOS prebuilt is intentionally not shipped (flaky macos-13 runner);
    # on Intel Macs: cargo install --git https://github.com/chirayuoli/vyer vyer-server
  end

  on_linux do
    on_arm do
      url "https://github.com/chirayuoli/vyer/releases/download/v#{version}/vyer-v#{version}-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "f12c118e59582fc1d4efd40d6ee95aca59dcf936885585dc4f4114eac9487893"
    end
    on_intel do
      url "https://github.com/chirayuoli/vyer/releases/download/v#{version}/vyer-v#{version}-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "d92bbefdc550b864e0e49faa3bc1cbbf9ee4d7373e4e8d78aa4389ce162f600c"
    end
  end

  def install
    bin.install "vyer"
  end

  test do
    assert_match "vyer", shell_output("#{bin}/vyer version")
  end
end
