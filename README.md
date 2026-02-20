# nix-fhs

CLI for managing FHS environments on nixOS

# Getting started

Ever wanted to just get something to run on nixOS, quick and dirty, without the hassle of doing it properly?

Now there's the nix-dev cli

Create an environment using `$ fhs add -e test-environment some-package another package`

For example `$ fhs add -e headers zlib`

Now you've got an environment named `headers` that includes the zlib binary, library and include files

You can enter it with `$ fhs enter headers` which will spawn your default shell

# Usage

```
Nix development environment manager

Usage: fhs [OPTIONS] [COMMAND]

Commands:
  add      Add one or more packages
  rm       Remove one or more packages
  rebuild  Rebuild an environment
  update   Update an environment
  info     Print infos about an environment
  enter    Enter an environment
  help     Print this message or the help of the given subcommand(s)

Options:
  -e, --env <ENV>   Environment to use [default: default]
  -r, --rebuild     Rebuild automatically
      --no-rebuild  Disable automatic rebuild
  -v, --verbose     Run with verbose logging
  -h, --help        Print help
```
