{
  description = "pty-mcp — low-footprint MCP server: interactive PTY sessions + passwordless sudo";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs { inherit system; };

        pty-mcp = pkgs.rustPlatform.buildRustPackage {
          pname = "pty-mcp";
          version = "0.0.4";
          src = ./.;
          # rustPlatform hashes the fetched+vendored crate tree; `cargoHash`
          # pins it so the sandboxed build is reproducible. Bump after any
          # dependency change (Cargo.lock churn) — `nix build` prints the
          # expected hash on mismatch. `just sync-flake` automates this.
          # cargo-lock: e911ce06fbbd70130f515640e185e6ccf877c75a6658ee24c3f56466c840c346
          cargoHash = "sha256-Mr1bIwEiFwEVbLDbAqvWxrrzg2jVXfsnrMjz+c4MopM=";
          # The PTY integration tests spawn /bin/sh, which the Nix build
          # sandbox doesn't provide — they fail there for lack of a shell,
          # not lack of correctness. `cargo test` in CI (real environment)
          # is the authoritative gate; here we just build the binary.
          doCheck = false;
        };
      in
      {
        packages = {
          default = pty-mcp;
          pty-mcp = pty-mcp;
        };

        apps.default = flake-utils.lib.mkApp {
          drv = pty-mcp;
          name = "pty-mcp";
        };

        checks.build = pty-mcp;

        devShells.default = pkgs.mkShell {
          packages = with pkgs; [
            cargo
            rustc
            clippy
            rustfmt
            rust-analyzer
            just
            git
          ];
          shellHook = ''
            echo "pty-mcp dev shell — \`just build\` to compile, \`just test\` to test"
          '';
        };

        formatter = pkgs.nixpkgs-fmt;
      });
}
