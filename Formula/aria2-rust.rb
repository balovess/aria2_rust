# Homebrew Formula for aria2-rust
#
# Installation:
#   brew tap balovess/aria2-rust https://github.com/balovess/aria2_rust.git
#   brew install balovess/aria2-rust/aria2-rust

class Aria2Rust < Formula
  desc "The ultra fast download utility - rewritten in Rust"
  homepage "https://github.com/balovess/aria2_rust"
  url "https://github.com/balovess/aria2_rust/archive/refs/tags/v0.3.11.tar.gz"
  sha256 "4e04d379cab546dfea72b51736d576dade8fda55f0b2b25a622459f694833602"
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
