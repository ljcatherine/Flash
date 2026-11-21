# Flash for Linux, built from this repository's Cargo workspace: the app and its polkit helper.
{
  lib,
  rustPlatform,
  makeWrapper,
  # tools Flash runs (the system's own copies come first on PATH)
  kmod,
  pciutils,
  usbutils,
  xdg-utils,
  dbus,
  # libraries winit and glow load with dlopen at runtime
  libxkbcommon,
  libGL,
  wayland,
  libx11,
  libxcursor,
  libxi,
  libxrandr,
  libxcb,
}:

let
  cargoToml = lib.importTOML ../Cargo.toml;
  runtimeLibs = [
    libxkbcommon
    libGL
    wayland
    libx11
    libxcursor
    libxi
    libxrandr
    libxcb
  ];
  tools = [
    kmod
    pciutils
    usbutils
    xdg-utils
    dbus
  ];
in
rustPlatform.buildRustPackage {
  pname = "flash";
  inherit (cargoToml.package) version;

  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../build.rs
      ../src
      ../helper
      ../assets
      ../packaging/linux
      ../tests
      ../README.md
      ../LICENSE
    ];
  };

  cargoLock.lockFile = ../Cargo.lock;

  nativeBuildInputs = [ makeWrapper ];

  cargoBuildFlags = [
    "--package"
    "flash"
    "--package"
    "flash-helper"
  ];
  # Run the whole workspace's tests, as `cargo test --workspace` does.
  cargoTestFlags = [ "--workspace" ];
  preCheck = ''
    export HOME=$TMPDIR
  '';

  postInstall = ''
    # The helper lives in libexec, and the polkit policy names its store path, so pkexec
    # runs exactly this build's helper (Flash finds it next to its own binary).
    install -Dm755 $out/bin/flash-helper $out/libexec/flash/flash-helper
    rm $out/bin/flash-helper
    mkdir -p $out/share/polkit-1/actions
    substitute packaging/linux/dev.liamjc.flash.policy.in $out/share/polkit-1/actions/dev.liamjc.flash.policy \
      --replace-fail @HELPER@ $out/libexec/flash/flash-helper
    install -Dm644 packaging/linux/flash.desktop $out/share/applications/flash.desktop
    install -Dm644 assets/flash.png $out/share/icons/hicolor/256x256/apps/flash.png
    install -Dm644 assets/flash.svg $out/share/icons/hicolor/scalable/apps/flash.svg
    wrapProgram $out/bin/flash \
      --prefix LD_LIBRARY_PATH : ${lib.makeLibraryPath runtimeLibs} \
      --suffix PATH : ${lib.makeBinPath tools}
  '';

  meta = {
    description = "Driver updater and PC health tool: driver and firmware updates, hardware health, benchmarks, cleanup and a system monitor";
    homepage = "https://flash.liamjc.dev";
    license = lib.licenses.gpl3Plus;
    mainProgram = "flash";
    platforms = lib.platforms.linux;
  };
}
