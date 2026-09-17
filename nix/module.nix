{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.services.redeemfx;
  configFile = (pkgs.formats.toml { }).generate "redeemfx.toml" {
    govee_device = cfg.goveeDevice;
    default_scene = cfg.defaultScene;
    rewards = lib.mapAttrs (_: reward: {
      inherit (reward)
        title
        cost
        color
        scene
        ;
      max_per_stream = reward.maxPerStream;
    }) cfg.rewards;
    effect_duration_seconds = cfg.effectDurationSeconds;
  };
in
{
  options.services.redeemfx = {
    enable = lib.mkEnableOption "Twitch redemption effects";

    package = lib.mkOption {
      type = lib.types.package;
      default = pkgs.callPackage ../. { };
      defaultText = lib.literalExpression "pkgs.callPackage ../. { }";
      description = "RedeemFX package to run.";
    };

    unitName = lib.mkOption {
      type = lib.types.strMatching "^[A-Za-z0-9][A-Za-z0-9_.@-]*$";
      default = "redeemfx";
      description = "Name of the systemd user service, without the .service suffix.";
    };

    stateDirectory = lib.mkOption {
      type = lib.types.strMatching "^[A-Za-z0-9][A-Za-z0-9_.-]*$";
      default = "redeemfx";
      description = "Name of the systemd user state directory.";
    };

    user = lib.mkOption {
      type = lib.types.nonEmptyStr;
      example = "streamer";
      description = "User whose systemd session runs RedeemFX.";
    };

    goveeDevice = lib.mkOption {
      type = lib.types.nonEmptyStr;
      example = "H1234/AA:BB:CC:DD:EE:FF:00:11";
      description = "Canonical SKU/device reference printed by list-devices.";
    };

    defaultScene = lib.mkOption {
      type = lib.types.nonEmptyStr;
      example = "govee-scene:base64url-data-from-list-scenes";
      description = "Scene reference restored after the redemption effect expires.";
    };

    rewards = lib.mkOption {
      type = lib.types.attrsOf (
        lib.types.submodule {
          options = {
            title = lib.mkOption {
              type = lib.types.nonEmptyStr;
              description = "Title shown for the Twitch channel-point reward.";
            };
            cost = lib.mkOption {
              type = lib.types.ints.positive;
              description = "Channel-point cost of the reward.";
            };
            color = lib.mkOption {
              type = lib.types.strMatching "^#[0-9A-Fa-f]{6}$";
              description = "Reward background color in #RRGGBB format.";
            };
            scene = lib.mkOption {
              type = lib.types.nonEmptyStr;
              description = "Canonical Govee scene reference activated by the reward.";
            };
            maxPerStream = lib.mkOption {
              type = lib.types.ints.positive;
              default = 99;
              description = "Maximum times Twitch permits this reward per stream.";
            };
          };
        }
      );
      default = { };
      description = "Twitch rewards managed by RedeemFX.";
    };

    effectDurationSeconds = lib.mkOption {
      type = lib.types.ints.positive;
      default = 60;
      description = "Seconds before restoring the default scene.";
    };

    secretsFile = lib.mkOption {
      type = lib.types.nonEmptyStr;
      example = "/home/streamer/.config/redeemfx/secrets.env";
      description = "External systemd environment file containing API credentials.";
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = cfg.rewards != { };
        message = "services.redeemfx.rewards must contain at least one reward mapping";
      }
      {
        assertion = !lib.hasSuffix ".service" cfg.unitName;
        message = "services.redeemfx.unitName must omit the .service suffix";
      }
      {
        assertion = lib.all (reward: builtins.stringLength reward.title <= 45) (lib.attrValues cfg.rewards);
        message = "services.redeemfx reward titles must not exceed 45 characters";
      }
      {
        assertion =
          let
            titles = map (reward: reward.title) (lib.attrValues cfg.rewards);
          in
          builtins.length titles == builtins.length (lib.unique titles);
        message = "services.redeemfx reward titles must be unique";
      }
    ];

    environment.systemPackages = [ cfg.package ];
    environment.etc."redeemfx.toml".source = configFile;

    systemd.user.services.${cfg.unitName} = {
      description = "RedeemFX Twitch redemption effects";
      wantedBy = [ "graphical-session.target" ];
      partOf = [ "graphical-session.target" ];
      after = [ "graphical-session.target" ];

      unitConfig.ConditionUser = cfg.user;

      serviceConfig = {
        EnvironmentFile = [ cfg.secretsFile ];
        ExecStart = "${cfg.package}/bin/redeemfx run";
        Restart = "on-failure";
        RestartSec = "5s";
        StateDirectory = cfg.stateDirectory;
        StateDirectoryMode = "0700";
        NoNewPrivileges = true;
        PrivateTmp = true;
      };
    };
  };
}
