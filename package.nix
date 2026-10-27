{
  lib,
  rustPlatform,
  makeWrapper,
  pkg-config,
  clang,
  cmake,
  fontconfig,
  freetype,
  libxkbcommon,
  wayland,
  libGL,
  vulkan-loader,
  libx11,
  libxcb,
  libxcb-cursor,
  libxcursor,
  libxi,
  libxrandr,
  openssl,
  zlib,
}:
rustPlatform.buildRustPackage {
  pname = "dida-box";
  version = "0.2.0";

  src = ./.;
  cargoLock = {
    lockFile = ./Cargo.lock;
    outputHashes = {
      "async-task-4.7.1" = "sha256-503Yewi444XtYodEWIggMmVN3NKqCgtMBNegiMyJcCI=";
      "collections-0.1.0" = "sha256-Hu659NqGMWLolNs4hAmB1HuX981tgnGXEocmRR3tDD0=";
      "derive_refineable-0.1.0" = "sha256-Hu659NqGMWLolNs4hAmB1HuX981tgnGXEocmRR3tDD0=";
      "gpui-0.2.2" = "sha256-Hu659NqGMWLolNs4hAmB1HuX981tgnGXEocmRR3tDD0=";
      "gpui_apple-0.1.0" = "sha256-Hu659NqGMWLolNs4hAmB1HuX981tgnGXEocmRR3tDD0=";
      "gpui_linux-0.1.0" = "sha256-Hu659NqGMWLolNs4hAmB1HuX981tgnGXEocmRR3tDD0=";
      "gpui_macos-0.1.0" = "sha256-Hu659NqGMWLolNs4hAmB1HuX981tgnGXEocmRR3tDD0=";
      "gpui_macros-0.1.0" = "sha256-Hu659NqGMWLolNs4hAmB1HuX981tgnGXEocmRR3tDD0=";
      "gpui_platform-0.1.0" = "sha256-Hu659NqGMWLolNs4hAmB1HuX981tgnGXEocmRR3tDD0=";
      "gpui_shared_string-0.1.0" = "sha256-Hu659NqGMWLolNs4hAmB1HuX981tgnGXEocmRR3tDD0=";
      "gpui_util-0.1.0" = "sha256-Hu659NqGMWLolNs4hAmB1HuX981tgnGXEocmRR3tDD0=";
      "gpui_web-0.1.0" = "sha256-Hu659NqGMWLolNs4hAmB1HuX981tgnGXEocmRR3tDD0=";
      "gpui_wgpu-0.1.0" = "sha256-Hu659NqGMWLolNs4hAmB1HuX981tgnGXEocmRR3tDD0=";
      "gpui_windows-0.1.0" = "sha256-Hu659NqGMWLolNs4hAmB1HuX981tgnGXEocmRR3tDD0=";
      "http_client-0.1.0" = "sha256-Hu659NqGMWLolNs4hAmB1HuX981tgnGXEocmRR3tDD0=";
      "proptest-1.10.0" = "sha256-p5NTcHhruI8QQvANACg8AMRVNmuvGxs2NLit+/8PaWo=";
      "proptest-macro-0.5.0" = "sha256-p5NTcHhruI8QQvANACg8AMRVNmuvGxs2NLit+/8PaWo=";
      "refineable-0.1.0" = "sha256-Hu659NqGMWLolNs4hAmB1HuX981tgnGXEocmRR3tDD0=";
      "scheduler-0.1.0" = "sha256-Hu659NqGMWLolNs4hAmB1HuX981tgnGXEocmRR3tDD0=";
      "sum_tree-0.1.0" = "sha256-Hu659NqGMWLolNs4hAmB1HuX981tgnGXEocmRR3tDD0=";
      "xim-ctext-0.3.0" = "sha256-pRT4Sz1JU9ros47/7pmIW9kosWOGMOItcnNd+VrvnpE=";
      "xim-parser-0.2.1" = "sha256-pRT4Sz1JU9ros47/7pmIW9kosWOGMOItcnNd+VrvnpE=";
      "zed-font-kit-0.14.1-zed" = "sha256-KXygi0olNQi5yM8eaJVykNDtbPMDjT+cWPBF8UrtXR4=";
      "zed-scap-0.0.8-zed" = "sha256-BihiQHlal/eRsktyf0GI3aSWsUCW7WcICMsC2Xvb7kw=";
      "zed-xim-0.4.0-zed" = "sha256-pRT4Sz1JU9ros47/7pmIW9kosWOGMOItcnNd+VrvnpE=";
      "ztracing-0.1.0" = "sha256-Hu659NqGMWLolNs4hAmB1HuX981tgnGXEocmRR3tDD0=";
      "ztracing_macro-0.1.0" = "sha256-Hu659NqGMWLolNs4hAmB1HuX981tgnGXEocmRR3tDD0=";
    };
  };

  nativeBuildInputs = [
    pkg-config
    clang
    cmake
    makeWrapper
  ];

  buildInputs = [
    fontconfig
    freetype
    libxkbcommon
    wayland
    libGL
    vulkan-loader
    libx11
    libxcb
    libxcb-cursor
    libxcursor
    libxi
    libxrandr
    openssl
    zlib
  ];

  postInstall = ''
    wrapProgram $out/bin/dida-box \
      --prefix LD_LIBRARY_PATH : "/run/opengl-driver/lib:${lib.makeLibraryPath [ vulkan-loader libGL ]}"
  '';

  meta = {
    description = "GPUI offscreen display for the Soeyi CH1L USB screen, fed by Spotify/Spicetify";
    license = lib.licenses.mit;
    mainProgram = "dida-box";
    platforms = lib.platforms.linux;
  };
}
