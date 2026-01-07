DISCLAIMER: I worked closely with claude-code while building this. Turns out it's good at rust?

Another disclaimer: this was written as an attempt to globally fix problems I found. I think it did a kind of useful job, though we should definitely be periodically fetching all sources to identify failures earlier.

# What is this

A completement to nixpkgs-review with the purpose of automating tasks in nixpkgs

# What does this implement

Define an intermediate attribute of a derivation as "source" in some sense. Where that's maybe `mydrv.src` or some vendored dependency like `cargoDeps`.

The goal is of this tool is primarily to:

- check that the source/intermediate attributes of a derivation changed by a PR is actually correct. This can be used to ensure there are no hash mismatches for a derivation. Note that this parses the title of the PR to decide which attribute to use (unless you specify differently)
- completely verify that all changed attributes between HEAD and the base of a PR using nix-eval-jobs (--full-eval flag of verify subcommand) are still able to be built
- Given an attribute that has mismatched hashes, fix the mismatched hashes and push to a branch and generate text to make a PR with that links to a locally hosted version of the build logs. An example of this is [hosted here](https://instance-20251227-2125.tail5ca7.ts.net/srcbot-srv/). We seemingly have a lot of mismatched hashes in nixpkgs.
- Iterate through all attributes in a pkgset, and build their intermediate attributes. This was implemented, then broke, and I haven't re-added it yet
- Drop packages and auto push to a branch, and generate PR text. This is not yet implemented, but I really feel like dropping a package should be automated so we don't have to mess around with checking the date. We should just provide a reason and srcbot should just figure out where to insert the drop for each pkgset in the related aliases file

The first three features I've tested pretty heavily and am confident work well

# Why?

I was going through the config options a few weeks ago and stumbled upon:

```
fetchedSourceNameDefault = "full";
```

I decided to add this, and it turns out that this causes all the src attributes have to re completely refetched and miss cache.nixos.org. It turns out that this causes a lot of hash mismatches. This kinda tur

# I hate that you used claude code

I'm not a fan of it either! But, undeniably I think this is something useful that allowed me to contribute somewhat impactfully to nixpkgs over my vacation. There were a LOT of mismatched hashes amongst different packages. And a lot of dead sources. This tool is able to both identify and automatically fix some of these. It's also hitting a niche that nixpkgs-review doesn't really hit which is "it's nontrivial to check intermediate attributes of a derivation"

# Attribution

This project would not have been possible without the incredible work done by contributors to nixpkgs-review. This was written after a thorough reading of the nixpkgs-review, which is licensed under MIT. I copy pasted the license as requested by the license because (I believe) it's derivative work.
