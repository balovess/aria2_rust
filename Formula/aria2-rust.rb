# Homebrew Formula for aria2-rust
#
# Installation:
#   brew tap balovess/aria2_rust https://github.com/balovess/aria2_rust.git
#   brew install balovess/aria2_rust/aria2-rust

class Aria2Rust < Formula
  desc "The ultra fast download utility - rewritten in Rust"
  homepage "https://github.com/balovess/aria2_rust"
  url "https://github.com/balovess/aria2_rust/archive/refs/tags/v0.3.8.tar.gz"
  sha256 "287d53bfb3125cefb5ca438d1bea3f35f028a94d302e1a920c626461426e0037"
  license "GPL-3.0-or-later"

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
