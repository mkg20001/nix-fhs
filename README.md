# nix-dev

CLI for managing FHS environments on nixOS

# Getting started

Ever wanted to just get something to run on nixOS, quick and dirty, without the hassle of doing it properly?

Now there's the nix-dev cli

Create an environment using `$ dev add -e test-environment some-package another package`

For example `$ dev add -e headers zlib`

Now you've got an environment named `headers` that includes the zlib binary, library and include files

You can enter it with `$ dev enter headers` which will spawn your default shell

# Usage

```
Commands:
  dev add [pkgs..]   add one or more packages
  dev rm [pkgs..]    remove one or more packages
  dev rebuild [env]  rebuild an environment
  dev update [env]   update an environment
  dev info [env]     print infos about an environment
  dev enter [env]    enter an environment                              [default]

Options:
  --version      Show version number                                   [boolean]
  --env, -e      Environment to use                [string] [default: "default"]
  --rebuild, -r  Rebuild automatically (disable: --no-rebuild)
                                                       [boolean] [default: true]
  --verbose, -v  Run with verbose logging                              [boolean]
  --help         Show help                                             [boolean]
```
