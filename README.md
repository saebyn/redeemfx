# RedeemFX

RedeemFX triggers managed effects from Twitch channel-point redemptions. The
initial provider activates Govee scenes, permits one effect at a time, refunds
conflicting redemptions, and restores a configured default scene afterward.

The project is currently being prepared for its first release. Its
command-line interface and NixOS module are not yet stable.

## Current capabilities

- Twitch Device Code authorization
- Twitch reward creation and reconciliation
- EventSub redemption delivery over WebSockets
- Govee device and scene discovery
- Timed scene activation and restoration
- Redemption refunding, retry, and crash recovery

## Development

Run the Rust checks with:

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
```

Build the Nix package with:

```bash
nix-build -E 'with import <nixpkgs> {}; callPackage ./. {}'
```

## NixOS module

Import `nix/module.nix` and configure `services.redeemfx`. The module runs a
systemd user service named `redeemfx.service` and stores state under
`redeemfx` by default. Both names can be changed independently:

```nix
services.redeemfx = {
  enable = true;
  unitName = "stream-redeems";
  stateDirectory = "stream-redeems";
  user = "streamer";
  secretsFile = "/home/streamer/.config/redeemfx/secrets.env";

  goveeDevice = "SKU/device-id";
  defaultScene = "govee-scene:default-scene-data";
  rewards.red = {
    title = "Go Red";
    cost = 51;
    color = "#FF0000";
    scene = "govee-scene:red-scene-data";
  };
};
```

## License

RedeemFX is licensed under the GNU Affero General Public License v3.0 only.
