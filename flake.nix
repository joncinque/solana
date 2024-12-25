{
  inputs = {
    fenix.url = "github:nix-community/fenix";
    flake-utils.url = "github:numtide/flake-utils";
    naersk.url = "github:nix-community/naersk";
    nixpkgs.url = "github:NixOS/nixpkgs/release-25.11";
  };

  outputs = { self, fenix, flake-utils, naersk, nixpkgs }:
    flake-utils.lib.eachDefaultSystem (buildSystem:
      let
        pkgs = (import nixpkgs) {
          system = buildSystem;
        };

        # Define all targets, ie. `nix build .#x86_64-unknown-linux-musl`
        triples = {
          x86_64-linux = "x86_64-unknown-linux-musl";
          aarch64-linux = "aarch64-unknown-linux-musl";
        };

        buildTriple = triples.${buildSystem};

        mkPkgsCross = targetSystem: import nixpkgs ({
          system = buildSystem;
        } // (if targetSystem == buildSystem then {} else {
          # The nixpkgs cache doesn't have any packages where cross-compiling has
          # been enabled, even if the target platform is actually the same as the
          # build platform (and therefore it's not really cross-compiling). So we
          # only set up the cross-compiling config if the target platform is
          # different.
          crossSystem.config = targetSystem;
        }));

        fenixPkgs = fenix.packages.${buildSystem};

        mkToolchain = fenixPkgs: fenixPkgs.toolchainOf {
          channel = "1.90.0";
          sha256 = "SJwZ8g0zF2WrKDVmHrVG3pD2RGoQeo24MEXnNx5FyuI=";
        };

        mkToolchainOverride = targetSystem:
          fenixPkgs.combine [
            (mkToolchain fenixPkgs).rustc
            (mkToolchain fenixPkgs).cargo
            (mkToolchain fenixPkgs.targets.${targetSystem}).rust-std
          ];

        naerskBuildPackage = targetSystem: args:
          let
            toolchain = mkToolchainOverride targetSystem;
            naersk' = naersk.lib.${buildSystem}.override {
              cargo = toolchain;
              rustc = toolchain;
            };
          in
          naersk'.buildPackage (
            args
            // { CARGO_BUILD_TARGET = targetSystem; }
            // (mkCargoConfig targetSystem)
          );

        # All of the CARGO_* configurations which should be used for all
        # targets.
        #
        # Only use this for options which should be universally applied or which
        # can be applied to a specific target triple.
        #
        # This is also merged into the devShell.
        mkCargoConfig = targetSystem:
          let
            pkgsCross = mkPkgsCross targetSystem;
          in rec {
            TARGET_CC = "${pkgsCross.stdenv.cc}/bin/${pkgsCross.stdenv.cc.targetPrefix}cc";
            # Tells Cargo to enable static compilation.
            # (https://doc.rust-lang.org/cargo/reference/config.html#targettriplerustflags)
            #
            # Note that the resulting binary might still be considered dynamically
            # linked by ldd, but that's just because the binary might have
            # position-independent-execution enabled.
            # (see: https://github.com/rust-lang/rust/issues/79624#issuecomment-737415388)
            CARGO_BUILD_RUSTFLAGS = [
              "-C target-feature=+crt-static"
              # https://github.com/rust-lang/cargo/issues/4133
              "-C" "linker=${TARGET_CC}"
            ];

            OPENSSL_STATIC = "1";
            OPENSSL_LIB_DIR = "${pkgsCross.pkgsStatic.openssl.out}/lib";
            OPENSSL_INCLUDE_DIR = "${pkgsCross.pkgsStatic.openssl.dev}/include";
          };

      in rec {
        defaultPackage = packages.${buildTriple};

        devShell = pkgs.mkShell (
          {
            inputsFrom = builtins.attrValues packages;
            CARGO_BUILD_TARGET = buildTriple;
          } // (mkCargoConfig buildSystem)
        );

        packages = builtins.listToAttrs (
          map (
            targetSystem:
              let
                pkgsCross = mkPkgsCross targetSystem;
              in {
                name = targetSystem;
                value = naerskBuildPackage targetSystem {
                  src = ./.;
                  strictDeps = true;
                  doCheck = targetSystem == buildSystem;
                  cargoBuildOptions = x: x ++ [ "-p" "solana-keygen" ];
                  #cargoTestOptions = x: x ++ [ "-p" "solana-keygen" ];
                  singleStep = true;
                  # udev needs to be present in native and build inputs, one for
                  # compilation of the hidapi build dependencies, and another at
                  # link time
                  #nativeBuildInputs = with pkgs; [ perl protobuf pkgsStatic.udev ];
                  nativeBuildInputs = with pkgs; [ pkgsStatic.udev ];
                  buildInputs = with pkgsCross; [ pkgsStatic.udev ];
                  # This is needed for cross-compilation of hidapi (I wish I understood why)
                  depsBuildBuild = [ pkgsCross.stdenv.cc ];
                };
              }
          )
          (builtins.attrValues triples)
        );
      }
  );
}
