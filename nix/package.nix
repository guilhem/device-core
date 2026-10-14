{ lib, stdenv, rustPlatform, buildPackages, src }:
let
  # Sandboxed tests need a session bus configuration outside /etc.
  dbusForTests = buildPackages.writeShellScriptBin "dbus-daemon" ''
    args=()
    for arg in "$@"; do
      if [ "$arg" = --session ]; then
        args+=(--config-file=${buildPackages.dbus}/share/dbus-1/session.conf)
      else
        args+=("$arg")
      fi
    done
    exec ${buildPackages.dbus}/bin/dbus-daemon "''${args[@]}"
  '';
in
rustPlatform.buildRustPackage {
  pname = "device-core";
  version = (builtins.fromTOML (builtins.readFile "${src}/Cargo.toml")).package.version;
  inherit src;
  cargoLock.lockFile = "${src}/Cargo.lock";
  postPatch = ''
    # This decoder fixture clears PATH, so its interpreter must be absolute.
    substituteInPlace tests/audio.rs --replace-fail '#!/usr/bin/python3' \
      '#!${buildPackages.python3}/bin/python3'
  '';
  doCheck = stdenv.buildPlatform.canExecute stdenv.hostPlatform;
  checkType = "debug";
  nativeCheckInputs = [ dbusForTests ] ++ (with buildPackages; [
    coreutils python3 openssh openssl rauc squashfsTools
  ]);
  postInstall = ''
    install -Dm644 LICENSE "$out/share/doc/device-core/LICENSE"
    install -m644 NOTICE "$out/share/doc/device-core/NOTICE"
  '';
  meta = {
    description = "Linux device services over D-Bus and optional HTTP";
    homepage = "https://github.com/guilhem/device-core";
    license = lib.licenses.gpl3Only;
    platforms = lib.platforms.linux;
    mainProgram = "device-core";
  };
}
