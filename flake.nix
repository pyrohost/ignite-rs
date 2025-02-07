# SPDX-FileCopyrightText: 2020 Serokell <https://serokell.io/>
# SPDX-FileCopyrightText: 2020 Andreas Fuchs <asf@boinkor.net>
#
# SPDX-License-Identifier: MPL-2.0

{
  description = "A fast and reliable deployment tool for mass-scale NixOS deployments";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    utils.url = "github:numtide/flake-utils";
    rust-overlay.url = "github:oxalica/rust-overlay";
    flake-compat = {
      url = "github:edolstra/flake-compat";
      flake = false;
    };
  };

  outputs = { self, nixpkgs, utils, rust-overlay, ... }@inputs:
    let
      # Common functions and settings
      systems = utils.lib.defaultSystems ++ ["aarch64-darwin"];
      mkPkgs = system: import nixpkgs {
        inherit system;
        overlays = [ rust-overlay.overlays.default self.overlays.default ];
      };
    in
    {
      overlays.default = final: prev: let
        darwinOptions = final.lib.optionalAttrs final.stdenv.isDarwin {
          buildInputs = with final.darwin.apple_sdk.frameworks; [
            SystemConfiguration
            CoreServices
          ];
        };
      in {
        ignite-rs = {
          ignite-rs = final.rustPlatform.buildRustPackage (darwinOptions // {
            pname = "ignite-rs";
            version = "1.0.0";
            src = final.lib.sourceByRegex ./. [
              "Cargo\.lock"
              "Cargo\.toml"
              "src"
              "src/bin"
              ".*\.rs$"
            ];
            cargoLock.lockFile = ./Cargo.lock;
            meta = {
              description = "A fast and reliable deployment tool for mass-scale NixOS deployments";
              mainProgram = "ignite";
            };
          });

          lib = rec {
            setActivate = builtins.trace
              "ignite-rs#lib.setActivate is deprecated, use activate.noop, activate.nixos or activate.custom instead"
              activate.custom;

            activate = rec {
              custom =
                {
                  __functor = customSelf: base: activate:
                    final.buildEnv {
                      name = ("ignite-" + base.name);
                      paths =
                        [
                          base
                          (final.writeTextFile {
                            name = base.name + "-ignite-activate";
                            text = ''
                              #!${final.runtimeShell}
                              set -euo pipefail

                              if [[ "''${DRY_ACTIVATE:-}" == "1" ]]
                              then
                                  ${customSelf.dryActivate or "echo ${final.writeScript "activate" activate}"}
                              elif [[ "''${BOOT:-}" == "1" ]]
                              then
                                  ${customSelf.boot or "echo ${final.writeScript "activate" activate}"}
                              else
                                  ${activate}
                              fi
                            '';
                            executable = true;
                            destination = "/ignite-activate";
                          })
                          (final.writeTextFile {
                              name = base.name + "-ignite";
                              text = ''
                              #!${final.runtimeShell}
                              exec ${final.ignite-rs.ignite-rs}/bin/ignite "$@"
                            '';
                            executable = true;
                            destination = "/ignite";
                          })
                        ];
                    };
                };

              nixos = base:
                (custom // {
                  dryActivate = "$PROFILE/bin/switch-to-configuration dry-activate";
                  boot = "$PROFILE/bin/switch-to-configuration boot";
                })
                base.config.system.build.toplevel
                ''
                  # work around https://github.com/NixOS/nixpkgs/issues/73404
                  cd /tmp

                  $PROFILE/bin/switch-to-configuration switch

                  # https://github.com/serokell/ignite-rs/issues/31
                  ${with base.config.boot.loader;
                  final.lib.optionalString systemd-boot.enable
                  "sed -i '/^default /d' ${efi.efiSysMountPoint}/loader/loader.conf"}
                '';

              home-manager = base: custom base.activationPackage "$PROFILE/activate";

              # Activation script for 'darwinSystem' from nix-darwin.
              # 'HOME=/var/root' is needed because 'sudo' on darwin doesn't change 'HOME' directory,
              # while 'darwin-rebuild' (which is invoked under the hood) performs some nix-channel
              # checks that rely on 'HOME'. As a result, if 'sshUser' is different from root,
              # deployment may fail without explicit 'HOME' redefinition.
              darwin = base: custom base.config.system.build.toplevel "HOME=/var/root $PROFILE/activate";

              noop = base: custom base ":";
            };

            deployChecks = deploy: builtins.mapAttrs (_: check: check deploy) {
              deploy-schema = deploy: final.runCommand "jsonschema-deploy-system" { } ''
                ${final.check-jsonschema}/bin/check-jsonschema --schemafile ${./interface.json} ${final.writeText "deploy.json" (builtins.toJSON deploy)} && touch $out
              '';

              deploy-activate = deploy:
                let
                  profiles = builtins.concatLists (final.lib.mapAttrsToList (nodeName: node: final.lib.mapAttrsToList (profileName: profile: [ (toString profile.path) nodeName profileName ]) node.profiles) deploy.nodes);
                in
                final.runCommand "ignite-rs-check-activate" { } ''
                  for x in ${builtins.concatStringsSep " " (map (p: builtins.concatStringsSep ":" p) profiles)}; do
                    profile_path=$(echo $x | cut -f1 -d:)
                    node_name=$(echo $x | cut -f2 -d:)
                    profile_name=$(echo $x | cut -f3 -d:)

                    test -f "$profile_path/ignite-activate" || (echo "#$node_name.$profile_name is missing the ignite-activate activation script" && exit 1);

                    test -f "$profile_path/ignite" || (echo "#$node_name.$profile_name is missing the ignite activation script" && exit 1);
                  done

                  touch $out
                '';
            };
          };
        };
      };
    } // utils.lib.eachSystem systems (system:
      let
        pkgs = mkPkgs system;
        toolchain = pkgs.rust-bin.nightly.latest.default;
        package = pkgs.ignite-rs.ignite-rs;
      in
      {
        packages = {
          default = package;
          ignite-rs = package;
        };

        apps.default = {
          type = "app";
          program = "${package}/bin/ignite";
        };

        devShell = pkgs.mkShell {
          inputsFrom = [ package ];
          RUST_SRC_PATH = "${toolchain}/lib/rustlib/src/rust/library";
          buildInputs = with pkgs; [
            nixVersions.latest
            toolchain
            rust-analyzer
            rustfmt
            clippy
            reuse
            package
          ];
        };

        checks = {
          ignite-rs = package.overrideAttrs (super: { doCheck = true; });
        } // (pkgs.lib.optionalAttrs (system == "x86_64-linux") 
          (import ./nix/tests { inherit inputs pkgs; }));

        inherit (pkgs.ignite-rs) lib;

        check-matrix = {
          include = map (v: { check = v; }) (pkgs.lib.attrNames self.checks.${system});
        };
      });
}
