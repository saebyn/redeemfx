# RedeemFX

`redeemfx` listens to Twitch EventSub over WebSockets and activates an
existing scene on one Govee device. It creates and reconciles the configured
channel-point rewards, permits one active effect, and pauses every managed
reward until the default scene has been restored. Redemptions that race with an
active effect are canceled so Twitch refunds the viewer's points.

## Accounts and secrets

1. In Govee Home, open **Profile > Settings > Apply for API Key** and complete
   the Developer API application. Govee emails the key after approval.
2. Register an application in the [Twitch Developer
   Console](https://dev.twitch.tv/console). Enable 2FA first if required. The
   Device Code flow supports a public client, which needs only the Client ID.
   A confidential client may also be used by providing its Client Secret.
3. Create an external file such as
   `~/.config/redeemfx/secrets.env`, set its mode to `0600`, and do not
   add it to this repository:

   ```text
   GOVEE_API_KEY=replace-with-govee-api-key
   TWITCH_CLIENT_ID=replace-with-twitch-client-id
   # Only for a Twitch application configured as a confidential client:
   TWITCH_CLIENT_SECRET=replace-with-twitch-client-secret
   ```

   ```bash
   chmod 0600 "$HOME/.config/redeemfx/secrets.env"
   ```

Load that file into the shell while running setup commands:

```bash
set -a
. "$HOME/.config/redeemfx/secrets.env"
set +a
```

Do not pass credentials on command lines. The service reads the file with
systemd `EnvironmentFile`; its contents never enter the Nix store.

## Discovery and testing

After installing the package through the NixOS module, discover the real device
identifier and capabilities. No model is assumed:

```bash
redeemfx list-devices
```

The canonical device reference is the printed `SKU/device` pair. Use it to list
all scene options reported by the device inventory, dynamic-scene endpoint, and
DIY-scene endpoint:

```bash
redeemfx list-scenes --device 'SKU/device-id'
```

Each result includes a canonical `govee-scene:...` reference. It encodes the
exact capability type, capability instance, and API value; scene names are only
labels and need not be unique. Test a reference before involving Twitch:

```bash
redeemfx activate-scene --device 'SKU/device-id' 'govee-scene:...'
```

Govee only exposes scenes supported by the selected device and account. Some
devices omit DIY scenes or snapshots, and unsupported scene endpoints are
reported as warnings. The service cannot invent or activate scenes absent from
Govee's official Developer Platform responses.

## Twitch authorization and rewards

Authorize once with the broadcaster account:

```bash
redeemfx auth
```

Open the displayed Twitch URL and enter its code. The application requests
`channel:manage:redemptions` so it can create and pause rewards and resolve
queued redemptions. Access and rotating refresh tokens are stored at
`$XDG_STATE_HOME/redeemfx/twitch-oauth.json`, or
`~/.local/state/redeemfx/twitch-oauth.json`, with mode `0600`. The service
validates the token at startup and hourly, refreshes it after authorization
failures, and atomically persists every replacement refresh token. A public
client's inactive refresh token expires after 30 days; rerun `auth` if Twitch
rejects refresh.

Rewards must be created by this Twitch application because Twitch does not let
an application manage rewards created manually or by another application. The
service creates missing configured rewards, adopts its own rewards by exact
title when local state is absent, and continuously applies their configured
titles, prices, colors, queue behavior, and per-stream limits. Resolved IDs are
stored in `twitch-rewards.json` beside the OAuth state. List visible reward IDs
with:

```bash
redeemfx list-rewards
```

## NixOS configuration

Add the discovered references and external secrets path to the host
configuration:

```nix
services.redeemfx = {
  enable = true;
  user = "user";
  goveeDevice = "SKU/device-id";
  defaultScene = "govee-scene:default-scene-data";

  rewards = {
    red = {
      title = "Go Red";
      cost = 51;
      color = "#FF0000";
      scene = "govee-scene:red-scene-data";
      maxPerStream = 99;
    };
    blue = {
      title = "Go Blue";
      cost = 52;
      color = "#0000FF";
      scene = "govee-scene:blue-scene-data";
      maxPerStream = 99;
    };
    green = {
      title = "Go Green";
      cost = 53;
      color = "#00FF00";
      scene = "govee-scene:green-scene-data";
      maxPerStream = 99;
    };
  };

  effectDurationSeconds = 60;
  secretsFile = "/home/user/.config/redeemfx/secrets.env";
};
```

Before the first test, remove any manually created rewards with the same titles;
the Twitch API cannot adopt or disable them. Rebuild NixOS, reauthorize if the
existing token lacks the management scope, then start or restart the service:

```bash
sudo nixos-rebuild switch
redeemfx auth
systemctl --user restart redeemfx.service
systemctl --user status redeemfx.service
journalctl --user -u redeemfx.service -f
```

Once enabled, `/etc/redeemfx.toml` is the non-secret generated
configuration used by CLI commands and the service. On startup, the service
pauses configured rewards, cancels stale queued redemptions, restores the
default scene, and then makes the rewards available. Rewards removed from the
configuration are disabled. A temporary restore or unpause failure keeps the
global lock active and retries after five seconds; ordinary Twitch disconnects
reconnect with bounded exponential backoff and create a new subscription.
