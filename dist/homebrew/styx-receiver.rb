class StyxReceiver < Formula
  desc "Software KVM receiver for macOS -- receives keyboard/mouse from a Hyprland Linux machine"
  homepage "https://github.com/ghreprimand/styx"
  url "https://github.com/ghreprimand/styx/archive/refs/tags/v0.5.9.tar.gz"
  sha256 "ff4973161076cb55d952be7e88e134a77f14d665d434b37ec5a270f122c18391"
  license "GPL-3.0-or-later"

  depends_on "rust" => :build

  def install
    system "cargo", "build", "--release", "-p", "styx-receiver"
    bin.install "target/release/styx-receiver"
  end
end
