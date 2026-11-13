{
  perSystem = { lib, pkgs, self', ... }: {
    packages = {
      default = self'.packages.pty-mcp;
      pty-mcp = pkgs.rustPlatform.buildRustPackage {
        pname = "pty-mcp";
        version = "0.0.6";
        src = lib.fileset.toSource {
          root = ../.;
          fileset = lib.fileset.difference (lib.fileset.gitTracked ../.) (
            lib.fileset.unions [
              ../flake.nix
              ../flake.lock
              ../nix
            ]
          );
        };
        # rustPlatform hashes the fetched+vendored crate tree; `cargoHash`
        # pins it so the sandboxed build is reproducible. Bump after any
        # dependency change (Cargo.lock churn) — `nix build` prints the
        # expected hash on mismatch. `just sync-flake` automates this.
        # cargo-lock: 00efed247495c4c2c44ee6e4b8f9f535f471d4dff9f95b209c04880d55463707
        cargoHash = "sha256-6hjlCBEwbmKt24nVtbbASYIp7ROdx3RPQaI8xhSodMA=";
        # The PTY integration tests spawn /bin/sh, which the Nix build
        # sandbox doesn't provide — they fail there for lack of a shell,
        # not lack of correctness. `cargo test` in CI (real environment)
        # is the authoritative gate; here we just build the binary.
        doCheck = false;
      };
    };

    apps.default = {
      type = "app";
      program = "${self'.packages.pty-mcp}/bin/pty-mcp";
    };

    checks.build = self'.packages.pty-mcp;
  };
}
