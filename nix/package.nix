{
  perSystem = { lib, pkgs, self', ... }: {
    packages = {
      default = self'.packages.pty-mcp;
      pty-mcp = pkgs.rustPlatform.buildRustPackage {
        pname = "pty-mcp";
        version = "0.0.7";
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
        # cargo-lock: 085e1afb543b4663099d63230f680e0f8a347340e4df7756ec800deb20fa6294
        cargoHash = "sha256-Vn16nZPUIPmtv60OvcyDRTWS2scQZ/ltimM7qTfEXTI=";
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
