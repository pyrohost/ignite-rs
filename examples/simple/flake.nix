# SPDX-FileCopyrightText: 2020 Serokell <https://serokell.io/>
#
# SPDX-License-Identifier: MPL-2.0

{
  description = "Deploy GNU hello to localhost";

  inputs.ignite-rs.url = "github:serokell/ignite-rs";

  outputs = { self, nixpkgs, ignite-rs }: {
    deploy.nodes.example = {
      hostname = "localhost";
      profiles.hello = {
        user = "balsoft";
        path = ignite-rs.lib.x86_64-linux.setActivate nixpkgs.legacyPackages.x86_64-linux.hello "./bin/hello";
      };
    };

    checks = builtins.mapAttrs (system: deployLib: deployLib.deployChecks self.deploy) ignite-rs.lib;
  };
}
