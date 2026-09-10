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
  url "https://github.com/balovess/aria2_rust/archive/refs/tags/v0.3.6.tar.gz"
  sha256 "2fbcbc81a782e1985bee5060a85893bdc78204c94d33789dddfdcdd4cfe223eb"
  license "GPL-2.0-or-later"

  depends_on "rust" => :build

  def install
    system "cargo", "build", "--release", "--locked", "--manifest-path", "aria2/Cargo.toml",
      "--no-default-features", "--features", "full"
    bin.install "target/release/aria2c"
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
