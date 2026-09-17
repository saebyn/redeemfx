# RedeemFX

RedeemFX triggers managed effects from Twitch channel-point redemptions. The
initial provider activates Govee scenes, permits one effect at a time, refunds
conflicting redemptions, and restores a configured default scene afterward.

The project is currently being extracted and prepared for its first release.
Its command-line interface and NixOS module are not yet stable.

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

## License

RedeemFX is licensed under the GNU Affero General Public License v3.0 only.
