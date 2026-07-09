class StyxReceiver < Formula
  desc "Software KVM receiver for macOS -- receives keyboard/mouse from a Hyprland Linux machine"
  homepage "https://github.com/ghreprimand/styx"
  url "https://github.com/ghreprimand/styx/archive/refs/tags/v0.5.6.tar.gz"
  sha256 "b8660688c7e180fba64e3c2366fd2de0e12371863ffb1bedf441fb52c8ea6106"
  license "GPL-3.0-or-later"

  depends_on "rust" => :build

  def install
    system "cargo", "build", "--release", "-p", "styx-receiver"
    bin.install "target/release/styx-receiver"
  end
end
