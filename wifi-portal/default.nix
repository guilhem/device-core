{ pkgs }:

pkgs.buildGoModule {
  pname = "device-core-wifi-portal";
  version = "0.1.0";
  src = pkgs.lib.cleanSource ./.;
  vendorHash = "sha256-S6VKiJYQ107w2huroM0ztW9eELfvfpbaiOE7Ey7OP8Q=";
  env.CGO_ENABLED = "0";
  doCheck = pkgs.stdenv.buildPlatform.canExecute pkgs.stdenv.hostPlatform;
  nativeCheckInputs = [ pkgs.buildPackages.dbus ];
  checkPhase = ''
    runHook preCheck
    DEVICE_CORE_PORTAL_TEST_DBUS=1 dbus-run-session \
      --dbus-daemon="${pkgs.buildPackages.dbus}/bin/dbus-daemon" \
      --config-file="${pkgs.buildPackages.dbus}/share/dbus-1/session.conf" \
      -- go test -count=1 ./...
    runHook postCheck
  '';
  postInstall = ''
    mv "$out/bin/wifi-portal" "$out/bin/device-core-wifi-portal"
  '';
  meta.mainProgram = "device-core-wifi-portal";
}
