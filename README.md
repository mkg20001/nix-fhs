# nix-fhs

CLI for managing FHS environments on nixOS

# What is an FHS environment?

FHS stands for first-hand shell aka the environment you find on most linux distributions, containing /usr/bin, /usr/lib, and all the other folders nixos doesn't have.

Sometimes a FHS may be necessary for an application to run or it might be easier to get the application running by simulating an FHS.

Additionally for some compilation operations it may be easier to just have all the headers in /usr/include as they are expected, instead of patching scripts layers deep.

nix-fhs makes this rather convoluted mechanism easy to use by providing commands to easily create, manage and update multiple independent fhs environments

Note that this is similar to nix-ld, but goes beyond just binaries with dynamic linking.

# Getting started

Create an environment using `$ fhs add -e test-environment some-package another package`

For example `$ fhs add -e headers zlib`

Now you've got an environment named `headers` that includes the zlib binary, library and include files

You can enter it with `$ fhs enter headers` which will spawn your default shell

# Usage

```
CLI for managing FHS environments on nixOS

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
