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
  version "0.3.5"
  license "GPL-2.0-or-later"

  on_macos do
    on_intel do
      url "https://github.com/balovess/aria2_rust/releases/download/v#{version}/aria2-x86_64-macos.tar.gz"
      sha256 "9da55d787a3a44b604d17e8448a2ced775dca961728391742eff2626db198f80"
    end
    on_arm do
      url "https://github.com/balovess/aria2_rust/releases/download/v#{version}/aria2-aarch64-macos.tar.gz"
      sha256 "3c4a68781f07660d65f624a68af6051ffcc52bca2353dfd41261048a4b34c137"
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/balovess/aria2_rust/releases/download/v#{version}/aria2-x86_64-linux.tar.gz"
      sha256 "56583e8abc3a753cf7eeb903caac0c4dec658874de10103b40b09d513406356d"
    end
    on_arm do
      url "https://github.com/balovess/aria2_rust/releases/download/v#{version}/aria2-aarch64-linux.tar.gz"
      sha256 "PLACEHOLDER_SHA256"
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
