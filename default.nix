{ lib, rustPlatform }:

rustPlatform.buildRustPackage {
  pname = "redeemfx";
  version = "0.1.0";

  src = lib.cleanSource ./.;
  cargoLock.lockFile = ./Cargo.lock;

  meta = {
    description = "Trigger managed effects from Twitch channel-point redemptions";
    license = lib.licenses.agpl3Only;
    mainProgram = "redeemfx";
    platforms = lib.platforms.linux;
  };
}
