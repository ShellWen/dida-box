{ pkgs ? import <nixpkgs> {} }:
let
  libraries = with pkgs; [
    fontconfig freetype libxkbcommon wayland libGL vulkan-loader
    libx11 libxcb libxcb-cursor libxcursor libxi libxrandr openssl zlib
  ];
in pkgs.mkShell {
  nativeBuildInputs = with pkgs; [ cargo rustc rustfmt clippy pkg-config clang cmake ];
  buildInputs = libraries;
  LD_LIBRARY_PATH = "${pkgs.lib.makeLibraryPath libraries}:/run/opengl-driver/lib";
  FONTCONFIG_FILE = "/etc/fonts/fonts.conf";
}
