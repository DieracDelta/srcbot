# Evaluates all packages and extracts their intermediate attribute drvPaths
# Used by srcbot --full-eval to detect which packages have changed sources
#
# This file is used with nix-eval-jobs --apply to extract intermediate drvPaths
# from each derivation that nix-eval-jobs finds.
#
# Usage:
#   nix-eval-jobs \
#     --expr 'import <nixpkgs> { system = "x86_64-linux"; }' \
#     --apply 'import ./extract-intermediates.nix' \
#     --workers 8
#
# The --apply function receives each derivation and returns extra data
# that gets included in the "extraValue" field of the JSON output.

# This is the --apply function
drv:
let
  # List of intermediate FOD attributes we care about
  intermediateAttrNames = [
    "src"
    "goModules"
    "cargoDeps"
    "npmDeps"
    "pnpmDeps"
    "yarnOfflineCache"
    "offlineCache"
    "nugetDeps"
    "pubcache"
    "composerVendor"
    "composerRepository"
    "fetchedMavenDeps"
    "mitmCache"
    "mixFodDeps"
  ];

  # Try to get drvPath from an attribute, return null if it fails
  tryGetDrvPath = name:
    let
      result = builtins.tryEval (
        if drv ? ${name} then
          let attr = drv.${name}; in
          if attr ? drvPath then attr.drvPath
          else null
        else null
      );
    in
    if result.success then result.value else null;

  # Build attrset of { intermediateName = drvPath or null }
  intermediates = builtins.listToAttrs (
    builtins.filter (x: x.value != null) (
      map (name: { inherit name; value = tryGetDrvPath name; }) intermediateAttrNames
    )
  );

in
intermediates
