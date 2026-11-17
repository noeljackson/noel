{
  description = "noel: a coding agent for the terminal";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    # The Rust of rust-toolchain.toml, which nixpkgs' own may not be.
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      rust-overlay,
    }:
    let
      inherit (nixpkgs) lib;
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];
      forAllSystems =
        f:
        lib.genAttrs systems (
          system:
          f (
            import nixpkgs {
              inherit system;
              overlays = [ rust-overlay.overlays.default ];
            }
          )
        );

      version = (lib.importTOML ./Cargo.toml).workspace.package.version;

      # What the workspace builds from: not the apps (their own workspaces),
      # nor what only CI, scripts and this flake read.
      src = lib.fileset.toSource {
        root = ./.;
        fileset = lib.fileset.difference ./. (
          lib.fileset.unions [
            ./apps
            ./scripts
            ./.github
            ./flake.nix
            ./flake.lock
          ]
        );
      };

      # The djs crates come from git at a pinned rev, which fetchGit takes
      # without a hash to keep up to date here.
      cargoLock = {
        lockFile = ./Cargo.lock;
        allowBuiltinFetchGit = true;
      };

      # The djs crates read files of their repository from outside their own
      # directories (its prelude and its builtin registry), which vendoring
      # crate by crate leaves behind: the repository at Cargo.lock's rev.
      djsSrc =
        let
          source =
            (lib.findFirst (package: package.name == "djs-core") null (lib.importTOML ./Cargo.lock).package)
            .source;
        in
        builtins.fetchGit {
          url = "https://github.com/earendil-works/djs";
          rev = lib.last (lib.splitString "#" source);
          allRefs = true;
        };

      meta = {
        description = "A coding agent for the terminal";
        homepage = "https://github.com/noeljackson/noel";
        license = lib.licenses.mit;
        mainProgram = "noel";
      };

      toolchainOf = pkgs: pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;

      # noel-env, the daemon `noel --workspace ssh://…` uploads to a remote
      # machine: a static musl binary built as the env-daemon profile is, for
      # this system's architecture (Linux only, as
      # scripts/build-env-daemons.sh). It has no C in it, so the toolchain's
      # own musl std and rust-lld link it, with no musl C toolchain.
      envDaemonOf =
        pkgs:
        let
          target = "${pkgs.stdenv.hostPlatform.parsed.cpu.name}-unknown-linux-musl";
          toolchain = (toolchainOf pkgs).override { targets = [ target ]; };
          rustPlatform = pkgs.makeRustPlatform {
            cargo = toolchain;
            rustc = toolchain;
          };
        in
        rustPlatform.buildRustPackage {
          pname = "noel-env";
          inherit version src cargoLock;
          env = {
            # nixpkgs' cargo hook cannot name a profile with a hyphen, so
            # these are env-daemon's settings on the release profile (strip
            # is on the command line: the hook's environment turns it off).
            CARGO_PROFILE_RELEASE_LTO = "true";
            CARGO_PROFILE_RELEASE_CODEGEN_UNITS = "1";
            "CARGO_TARGET_${lib.toUpper (lib.replaceStrings [ "-" ] [ "_" ] target)}_LINKER" = "rust-lld";
          };
          buildPhase = ''
            runHook preBuild
            cargo build --frozen --release -j "$NIX_BUILD_CORES" --target ${target} -p noel-env-daemon \
              --config profile.release.strip=true
            runHook postBuild
          '';
          installPhase = ''
            runHook preInstall
            install -D -m 0755 target/${target}/release/noel-env "$out/bin/noel-env"
            runHook postInstall
          '';
          doCheck = false;
          meta = meta // {
            description = "The daemon noel runs a remote workspace's tools through";
            mainProgram = "noel-env";
            platforms = lib.platforms.linux;
          };
        };

      # The name noel looks its daemon up by, beside itself.
      envDaemonName =
        pkgs: "noel-env-linux-${if pkgs.stdenv.hostPlatform.isAarch64 then "arm64" else "x64"}";

      # noel, with what `make install` puts beside it: the bun host of
      # TypeScript extensions and, on Linux, the daemon for remote
      # workspaces. `noel bun install` downloads a bun that NixOS cannot
      # run, so nixpkgs' is last on noel's PATH (withBun).
      noelOf =
        pkgs:
        {
          withBun ? true,
        }:
        let
          toolchain = toolchainOf pkgs;
          rustPlatform = pkgs.makeRustPlatform {
            cargo = toolchain;
            rustc = toolchain;
          };
          inherit (pkgs.stdenv.hostPlatform) isLinux;
        in
        rustPlatform.buildRustPackage {
          pname = "noel";
          inherit version src cargoLock;
          cargoBuildFlags = [
            "-p"
            "noel-cli"
            "--bin"
            "noel"
          ];
          nativeBuildInputs = [ pkgs.makeWrapper ];
          # Where the vendored djs crates look: two levels above themselves.
          preBuild = ''
            ln -s ${djsSrc}/builtins ${djsSrc}/tests "$NIX_BUILD_TOP/"
          '';
          # The tests start servers, terminals and git; `make check` runs them.
          doCheck = false;
          postInstall = ''
            install -m 0644 crates/noel-cli/src/extension-hosts/noel-bun-host.ts "$out/bin/noel-bun-host.ts"
          ''
          + lib.optionalString isLinux ''
            install -m 0755 ${envDaemonOf pkgs}/bin/noel-env "$out/bin/${envDaemonName pkgs}"
          ''
          + lib.optionalString withBun ''
            wrapProgram "$out/bin/noel" --suffix PATH : ${lib.makeBinPath [ pkgs.bun ]}
          '';
          inherit meta;
        };
    in
    {
      packages = forAllSystems (
        pkgs:
        let
          noel = lib.makeOverridable (noelOf pkgs) { };
        in
        {
          inherit noel;
          default = noel;
        }
        // lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
          noel-env = envDaemonOf pkgs;
        }
      );

      apps = forAllSystems (pkgs: {
        default = {
          type = "app";
          program = lib.getExe self.packages.${pkgs.stdenv.hostPlatform.system}.noel;
          meta.description = meta.description;
        };
      });

      overlays.default = final: _prev: {
        noel = self.packages.${final.stdenv.hostPlatform.system}.noel;
      };

      # What `make check` and `make e2e` use.
      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          packages = [
            ((toolchainOf pkgs).override {
              extensions = [
                "clippy"
                "rustfmt"
                "rust-src"
                "rust-analyzer"
              ];
            })
            pkgs.bun
            pkgs.git
            pkgs.gnumake
            pkgs.tmux
          ];
        };
      });

      checks = forAllSystems (pkgs: {
        inherit (self.packages.${pkgs.stdenv.hostPlatform.system}) noel;
      });

      formatter = forAllSystems (pkgs: pkgs.nixfmt);
    };
}
