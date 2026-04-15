class StyxReceiver < Formula
  desc "Software KVM receiver for macOS -- receives keyboard/mouse from a Hyprland Linux machine"
  homepage "https://github.com/ghreprimand/styx"
  url "https://github.com/ghreprimand/styx/archive/refs/tags/v0.5.1.tar.gz"
  sha256 "SKIP"
  license "MIT"

  depends_on "rust" => :build

  def install
    system "cargo", "build", "--release", "-p", "styx-receiver"
    bin.install "target/release/styx-receiver"
  end
end
