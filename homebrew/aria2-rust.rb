# Homebrew Formula for aria2-rust
#
# Installation:
#   brew install aria2-rust
#
# Or from this repository:
#   brew install ./homebrew/aria2-rust.rb

class Aria2Rust < Formula
  desc "The ultra fast download utility - rewritten in Rust"
  homepage "https://github.com/balovess/aria2_rust"
  version "0.1.0"
  license "GPL-2.0-or-later"

  on_macos do
    on_intel do
      url "https://github.com/balovess/aria2_rust/releases/download/v#{version}/aria2-x86_64-macos.tar.gz"
      sha256 "PLACEHOLDER_SHA256" # TODO: Update with actual SHA256
    end
    on_arm do
      url "https://github.com/balovess/aria2_rust/releases/download/v#{version}/aria2-aarch64-macos.tar.gz"
      sha256 "PLACEHOLDER_SHA256" # TODO: Update with actual SHA256
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/balovess/aria2_rust/releases/download/v#{version}/aria2-x86_64-linux.tar.gz"
      sha256 "PLACEHOLDER_SHA256" # TODO: Update with actual SHA256
    end
    on_arm do
      url "https://github.com/balovess/aria2_rust/releases/download/v#{version}/aria2-aarch64-linux.tar.gz"
      sha256 "PLACEHOLDER_SHA256" # TODO: Update with actual SHA256
    end
  end

  def install
    bin.install "aria2c"
  end

  def caveats
    <<~EOS
      aria2-rust is now installed!

      Quick start:
        aria2c http://example.com/file.zip

      For more options:
        aria2c --help
    EOS
  end

  test do
    system "#{bin}/aria2c", "--version"
  end
end
