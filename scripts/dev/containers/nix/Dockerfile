# s2udio distro-support harness — nix target (nix package manager container).
# The image is used as-is (no init to bake); the driver builds uniformly.
FROM ghcr.io/nixos/nix
# The official nix image ships NO /etc/os-release; setup.sh needs ID=nixos to
# route to the nix backend (the container approximates a NixOS host for the
# flake/profile-install path — plan §6.3; mirrors the mock os-release fixture).
RUN printf 'NAME="NixOS"\nID=nixos\nID_LIKE=""\n' > /etc/os-release
CMD ["sleep", "infinity"]
