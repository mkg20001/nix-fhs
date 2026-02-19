use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "dev")]
#[command(about = "Nix development environment manager", long_about = None)]
struct Cli {
    /// Environment to use
    #[arg(short, long, default_value = "default", global = true)]
    env: String,

    /// Rebuild automatically
    #[arg(short, long, global = true, overrides_with = "no_rebuild")]
    rebuild: bool,

    /// Disable automatic rebuild
    #[arg(long, global = true, overrides_with = "rebuild")]
    no_rebuild: bool,

    /// Run with verbose logging
    #[arg(short, long, global = true)]
    verbose: bool,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Add one or more packages
    Add {
        /// Packages to add
        #[arg(required = true)]
        pkgs: Vec<String>,
    },

    /// Remove one or more packages
    Rm {
        /// Packages to remove
        #[arg(required = true)]
        pkgs: Vec<String>,
    },

    /// Rebuild an environment
    Rebuild,

    /// Update an environment
    Update {
        /// Fetch channels before updating
        #[arg(short, long)]
        fetch: bool,

        /// Update all existing environments
        #[arg(short, long)]
        all: bool,
    },

    /// Print infos about an environment
    Info {
        /// Print info in JSON
        #[arg(short, long)]
        json: bool,
    },

    /// Enter an environment
    Enter,
}

fn main() {
    let cli = Cli::parse();

    // Default is to rebuild; --no-rebuild disables it
    let rebuild = !cli.no_rebuild;

    if cli.verbose {
        eprintln!("Verbose mode enabled");
    }

    match cli.command {
        Some(Commands::Add { pkgs }) => {
            println!("Adding packages to env '{}': {:?}", cli.env, pkgs);
            if rebuild {
                println!("Auto-rebuild enabled");
            }
            // TODO: implement add logic
        }
        Some(Commands::Rm { pkgs }) => {
            println!("Removing packages from env '{}': {:?}", cli.env, pkgs);
            if rebuild {
                println!("Auto-rebuild enabled");
            }
            // TODO: implement rm logic
        }
        Some(Commands::Rebuild) => {
            println!("Rebuilding env '{}'", cli.env);
            // TODO: implement rebuild logic
        }
        Some(Commands::Update { fetch, all }) => {
            if all {
                println!("Updating all environments");
            } else {
                println!("Updating env '{}'", cli.env);
            }
            if fetch {
                println!("Fetching channels first");
            }
            if rebuild {
                println!("Auto-rebuild enabled");
            }
            // TODO: implement update logic
        }
        Some(Commands::Info { json }) => {
            println!("Info for env '{}'", cli.env);
            if json {
                println!("JSON output mode");
            }
            // TODO: implement info logic
        }
        Some(Commands::Enter) | None => {
            // Enter is the default command when no subcommand is given
            println!("Entering env '{}'", cli.env);
            if rebuild {
                println!("Auto-rebuild enabled");
            }
            // TODO: implement enter logic
        }
    }
}
