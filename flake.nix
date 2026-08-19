{
  # s2udio — media center TUI (rmpc fork). Flake: package + devShell +
  # profile-install path (plan docs/design/Validation/distro-support.md §6.3).
  #
  #   nix develop            # devShell: rust toolchain + all runtime deps
  #   nix profile install .#s2udio   # install the app + support scripts
  #   nix build .#s2udio     # build into ./result
  #
  # The NixOS module + nixosTest VM live in a separate module flake
  # (out-of-container validation — see the plan §6.3 / §8 P2 exit).
  description = "s2udio — a beautiful, configurable media center TUI for MPD / Jellyfin / YouTube";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs { inherit system; };
        rustPlatform = pkgs.rustPlatform;

        # The runtime python environment the support scripts need:
        # dbus-python (python3-dbus), PyGObject (python3-gi) and mutagen
        # (python-mutagen — checked by the app's dependency probe).
        # mpdris2 (upstream source, used by the s2u-mpdris2 shim) needs
        # python-mpd2 on top of dbus/gi.
        bridgePython = pkgs.python3.withPackages (ps: [
          ps.dbus-python
          ps.pygobject3
          ps.mutagen
          ps.python-mpd2
        ]);

        s2udio = rustPlatform.buildRustPackage {
          pname = "s2udio";
          version = "0.11.0";
          src = self;
          cargoLock.lockFile = ./Cargo.lock;

          nativeBuildInputs = [ pkgs.git ];

          # The full 1282-test suite is validated in the ephemeral harness
          # containers (gate G3) and in `nix develop`; the nix sandbox lacks
          # network/HOME so a subset of env-dependent tests fails there.
          doCheck = false;

          # build.rs embeds git info via vergen_gitcl and FAILS without a git
          # repo (the flake source is copied to the store without .git).
          preBuild = ''
            git init -q
            git -c user.email=s2udio@localhost -c user.name=s2udio add -A
            git -c user.email=s2udio@localhost -c user.name=s2udio commit -qm "flake build snapshot"
          '';

          # The runtime assets the support scripts / config seed need.
          postInstall = ''
            mkdir -p $out/share/s2udio
            install -Dm755 scripts/s2u-mpv-tracker $out/bin/s2u-mpv-tracker
            install -Dm755 scripts/s2udio-mpris    $out/bin/s2udio-mpris
            install -Dm755 scripts/s2u-mpdris2     $out/bin/s2u-mpdris2
            install -Dm755 scripts/rmpc-fetch-lyrics $out/bin/rmpc-fetch-lyrics
            install -Dm755 scripts/s2u-svc         $out/bin/s2u-svc
            install -Dm644 assets/example_config.ron $out/share/s2udio/example_config.ron
            install -Dm644 assets/example_theme.ron  $out/share/s2udio/example_theme.ron
            # the app is launched as `s2udio` (setup.sh renames s2u -> s2udio)
            ln -s $out/bin/s2u $out/bin/s2udio
          '';

          meta = with pkgs.lib; {
            description = "s2udio media center TUI";
            license = licenses.bsd3;
            platforms = platforms.linux;
          };
        };
      in
      {
        packages.default = s2udio;
        packages.s2udio = s2udio;

        # The python env the support scripts need (dbus-python/PyGObject/
        # mutagen) — installable alongside the app for runtime use.
        packages.bridgePython = bridgePython;

        # nix develop: everything needed to build AND run the full feature set
        # (mpd/mpv/yt-dlp/cava/mpdris2/ffmpeg + the python bridge env).
        devShells.default = pkgs.mkShell {
          packages = [
            s2udio
            pkgs.mpd
            pkgs.mpv
            pkgs.yt-dlp
            pkgs.cava
            pkgs.mpdris2
            pkgs.ffmpeg
            bridgePython
            pkgs.tmux
            pkgs.git
          ];
          shellHook = ''
            export PATH="$PATH:${bridgePython}/bin"
          '';
        };
      });
}
