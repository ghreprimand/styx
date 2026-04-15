class StyxReceiver < Formula
  desc "Software KVM receiver for macOS -- receives keyboard/mouse from a Hyprland Linux machine"
  homepage "https://github.com/ghreprimand/styx"
  url "https://github.com/ghreprimand/styx/archive/refs/tags/v0.5.0.tar.gz"
  sha256 "21cae643cde50efe120181ab7d2822b0e90abba426760ced78a9c1e377a22bc3"
  license "MIT"

  depends_on "rust" => :build

  def install
    system "cargo", "build", "--release", "-p", "styx-receiver"
    bin.install "target/release/styx-receiver"
  end
end
