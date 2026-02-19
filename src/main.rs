use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};
use std::collections::HashSet;
use std::fs;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};

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

fn cache_dir() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("dev")
}

fn config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("dev")
}

fn has_flakes() -> bool {
    Command::new("nix")
        .args(["flake", "--help"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

struct SpawnResult {
    stdout: String,
    stderr: String,
    success: bool,
}

fn spawn(cmd: &str, args: &[&str], capture: bool, nix_path: Option<&str>) -> Result<SpawnResult> {
    let mut command = Command::new(cmd);
    command.args(args);

    if let Some(path) = nix_path {
        command.env("NIX_PATH", path);
    }

    if capture {
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
    } else {
        command.stdout(Stdio::inherit()).stderr(Stdio::inherit());
    }

    let output = command.output().context(format!("Failed to spawn {}", cmd))?;

    Ok(SpawnResult {
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        success: output.status.success(),
    })
}

fn spawn_inherit(cmd: &str, args: &[&str], nix_path: Option<&str>) -> Result<bool> {
    let mut command = Command::new(cmd);
    command.args(args);

    if let Some(path) = nix_path {
        command.env("NIX_PATH", path);
    }

    let status = command.status().context(format!("Failed to spawn {}", cmd))?;
    Ok(status.success())
}

struct Storage {
    packages: Vec<String>,
    disk_path: PathBuf,
    is_new: bool,
}

impl Storage {
    fn new(env: &str) -> Self {
        let disk_path = config_dir().join(format!("env.{}", env));
        let is_new = !disk_path.exists();

        let packages = if disk_path.exists() {
            fs::read_to_string(&disk_path)
                .unwrap_or_default()
                .lines()
                .filter(|l| !l.is_empty())
                .map(String::from)
                .collect()
        } else {
            Vec::new()
        };

        Storage {
            packages,
            disk_path,
            is_new,
        }
    }

    fn write(&self) -> Result<()> {
        fs::create_dir_all(config_dir())?;
        fs::write(&self.disk_path, self.packages.join("\n"))
            .context("Failed to write storage")?;
        Ok(())
    }

    fn add(&mut self, pkg: &str) {
        if !self.packages.contains(&pkg.to_string()) {
            self.packages.push(pkg.to_string());
            self.packages.sort();
        }
    }

    fn remove(&mut self, pkg: &str) {
        self.packages.retain(|p| p != pkg);
    }

    fn contains(&self, pkg: &str) -> bool {
        self.packages.contains(&pkg.to_string())
    }
}

struct Channels {
    disk_path: PathBuf,
}

impl Channels {
    fn new(env: &str) -> Result<Self> {
        let disk_path = cache_dir().join(env).join("channels");
        fs::create_dir_all(&disk_path)?;

        Ok(Channels { disk_path })
    }

    fn has(&self, name: &str) -> bool {
        self.disk_path.join(name).exists()
    }

    fn list(&self) -> Vec<String> {
        fs::read_dir(&self.disk_path)
            .map(|entries| {
                entries
                    .filter_map(|e| e.ok())
                    .filter_map(|e| e.file_name().into_string().ok())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn update(&self, name: &str, verbose: bool) -> Result<()> {
        if verbose {
            eprintln!("Updating channel: {}", name);
        }

        let channel_path = resolve_channel(name)?;

        let gc_root = self.disk_path.join(name);
        if gc_root.exists() {
            fs::remove_file(&gc_root).ok();
        }

        let result = spawn(
            "nix-store",
            &[
                "--realise",
                &channel_path,
                "--indirect",
                "--add-root",
                gc_root.to_str().unwrap(),
            ],
            true,
            None,
        )?;

        if !result.success {
            bail!("Failed to update channel {}: {}", name, result.stderr.trim());
        }

        Ok(())
    }

    fn remove(&self, name: &str) {
        let path = self.disk_path.join(name);
        if path.exists() {
            fs::remove_file(&path).ok();
        }
    }

    fn get_nix_path(&self) -> String {
        self.list()
            .iter()
            .map(|channel| {
                format!(
                    "{}={}",
                    channel,
                    self.disk_path.join(channel).display()
                )
            })
            .collect::<Vec<_>>()
            .join(":")
    }
}

fn resolve_channel(name: &str) -> Result<String> {
    let flakes = has_flakes();
    let expr = format!("(<{}>)", name);
    let args: Vec<&str> = if flakes {
        vec!["eval", "--raw", "--impure", "--expr", &expr]
    } else {
        vec!["eval", "--raw", &expr]
    };

    let result = spawn("nix", &args, true, None)?;

    let path = result.stdout.trim();
    if !path.starts_with('/') {
        bail!("nix: {}", result.stderr.trim());
    }

    Ok(path.to_string())
}

fn check_if_package_exists(attr: &str, channels: &Channels) -> Result<bool> {
    if !attr.contains('.') {
        return Ok(false);
    }

    let parts: Vec<&str> = attr.split('.').collect();
    let channel = parts[0];
    let channel_attr: Vec<String> = parts[1..].iter().map(|s| format!("\"{}\"", s)).collect();

    let expr = format!(
        "(let ch = (import <{}> {{}}); in ch ? {})",
        channel,
        channel_attr.join(".")
    );

    let flakes = has_flakes();
    let args: Vec<&str> = if flakes {
        vec!["eval", "--impure", "--expr", &expr]
    } else {
        vec!["eval", &expr]
    };

    let result = spawn(
        "nix",
        &args,
        true,
        Some(&channels.get_nix_path()),
    )?;

    if !result.success {
        bail!("nix: {}", result.stderr.trim());
    }

    Ok(result.stdout.trim() == "true")
}

fn generate_nix(name: &str, storage: &Storage, channels: &Channels) -> String {
    let channel_imports: String = channels
        .list()
        .iter()
        .map(|ch| format!("  {} = import <{}> {{}};", ch, ch))
        .collect::<Vec<_>>()
        .join("\n");

    let packages: String = storage
        .packages
        .iter()
        .map(|p| format!("    ({})", p))
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        r#"{{ pkgs ? import <nixpkgs> {{}} }}:

let
{channel_imports}
in
(pkgs.buildFHSEnv {{
  name = "dev-{name}";
  extraOutputsToInstall = ["include" "dev"];

  targetPkgs = pkgs: with pkgs; [
{packages}
  ];

  multiPkgs = pkgs: with pkgs; [
  ];

  profile = ''
    export IS_DEV=1
    export DEV_ENV="{name}"
  '';

  runScript = ''$SHELL'';
}})"#,
        channel_imports = channel_imports,
        name = name,
        packages = packages,
    )
}

fn rebuild(env: &str, storage: &Storage, channels: &Channels, verbose: bool) -> Result<()> {
    let env_cache = cache_dir().join(env);
    fs::create_dir_all(&env_cache)?;

    let nix_path = env_cache.join("default.nix");
    let result_path = env_cache.join("result");

    if verbose {
        eprintln!("Generating nix expression");
    }

    let nix_content = generate_nix(env, storage, channels);
    fs::write(&nix_path, nix_content)?;

    println!("rebuilding {}...", env);

    let success = spawn_inherit(
        "nix-build",
        &[
            nix_path.to_str().unwrap(),
            "-o",
            result_path.to_str().unwrap(),
        ],
        Some(&channels.get_nix_path()),
    )?;

    if !success {
        bail!("Build failed");
    }

    Ok(())
}

fn routine_stuff(storage: &Storage, channels: &Channels, verbose: bool) -> Result<()> {
    // Ensure nixpkgs channel exists
    if !channels.has("nixpkgs") {
        channels.update("nixpkgs", verbose)?;
    }

    // Determine which channels we need
    let mut should_have: HashSet<String> = storage
        .packages
        .iter()
        .filter_map(|p| p.split('.').next())
        .map(String::from)
        .collect();
    should_have.insert("nixpkgs".to_string());

    // GC unused channels
    for channel in channels.list() {
        if !should_have.contains(&channel) {
            if verbose {
                eprintln!("GC channel: {}", channel);
            }
            channels.remove(&channel);
        }
    }

    Ok(())
}

fn env_not_found(env: &str) {
    eprintln!(
        "Environment {:?} does not exist, please create it by adding packages",
        env
    );
    if env == "default" {
        eprintln!(" $ dev add <package>");
    } else {
        eprintln!(" $ dev add -e {} <package>", env);
    }
    std::process::exit(1);
}

fn cmd_add(
    env: &str,
    pkgs: Vec<String>,
    auto_rebuild: bool,
    verbose: bool,
) -> Result<()> {
    let mut storage = Storage::new(env);
    let channels = Channels::new(env)?;

    routine_stuff(&storage, &channels, verbose)?;

    let mut had_errors = false;

    for mut pkg in pkgs {
        match check_if_package_exists(&pkg, &channels) {
            Ok(true) => {}
            Ok(false) => {
                if verbose {
                    eprintln!("{}: not found, trying nixpkgs prefix", pkg);
                }
                let prefixed = format!("nixpkgs.{}", pkg);
                match check_if_package_exists(&prefixed, &channels) {
                    Ok(true) => {
                        if verbose {
                            eprintln!("{}: found as {}", pkg, prefixed);
                        }
                        pkg = prefixed;
                    }
                    Ok(false) => {
                        eprintln!("{}: does not exist or fails to evaluate", pkg);
                        had_errors = true;
                        continue;
                    }
                    Err(e) => {
                        eprintln!("{}: {}", pkg, e);
                        had_errors = true;
                        continue;
                    }
                }
            }
            Err(e) => {
                eprintln!("{}: {}", pkg, e);
                had_errors = true;
                continue;
            }
        }

        let channel = pkg.split('.').next().unwrap();

        if !channels.has(channel) {
            if verbose {
                eprintln!("Adding channel: {}", channel);
            }
            channels.update(channel, verbose)?;
        }

        storage.add(&pkg);
    }

    storage.write()?;

    if auto_rebuild {
        rebuild(env, &storage, &channels, verbose)?;
    }

    if had_errors {
        std::process::exit(1);
    }

    Ok(())
}

fn cmd_rm(
    env: &str,
    pkgs: Vec<String>,
    auto_rebuild: bool,
    verbose: bool,
) -> Result<()> {
    let mut storage = Storage::new(env);
    let channels = Channels::new(env)?;

    for mut pkg in pkgs {
        if !storage.contains(&pkg) {
            let prefixed = format!("nixpkgs.{}", pkg);
            if storage.contains(&prefixed) {
                pkg = prefixed;
            } else {
                println!("{}: not installed", pkg);
                continue;
            }
        }

        if verbose {
            eprintln!("Removing: {}", pkg);
        }
        storage.remove(&pkg);
    }

    routine_stuff(&storage, &channels, verbose)?;
    storage.write()?;

    if auto_rebuild {
        rebuild(env, &storage, &channels, verbose)?;
    }

    Ok(())
}

fn cmd_rebuild(env: &str, verbose: bool) -> Result<()> {
    let storage = Storage::new(env);
    if storage.is_new {
        env_not_found(env);
    }

    let channels = Channels::new(env)?;

    routine_stuff(&storage, &channels, verbose)?;
    rebuild(env, &storage, &channels, verbose)?;

    Ok(())
}

fn cmd_update(
    env: &str,
    fetch: bool,
    all: bool,
    auto_rebuild: bool,
    verbose: bool,
) -> Result<()> {
    if fetch {
        println!("Fetching channels...");
        spawn_inherit("nix-channel", &["--update", "-vv"], None)?;
    }

    let envs: Vec<String> = if all {
        fs::read_dir(config_dir())
            .map(|entries| {
                entries
                    .filter_map(|e| e.ok())
                    .filter_map(|e| e.file_name().into_string().ok())
                    .filter(|name| name.starts_with("env."))
                    .map(|name| name.strip_prefix("env.").unwrap().to_string())
                    .collect()
            })
            .unwrap_or_default()
    } else {
        vec![env.to_string()]
    };

    for env in envs {
        let storage = Storage::new(&env);
        if storage.is_new {
            env_not_found(&env);
        }

        let channels = Channels::new(&env)?;

        routine_stuff(&storage, &channels, verbose)?;

        println!("Updating environment: {}", env);
        for channel in channels.list() {
            channels.update(&channel, verbose)?;
        }

        if auto_rebuild {
            rebuild(&env, &storage, &channels, verbose)?;
        }
    }

    Ok(())
}

fn cmd_info(env: &str, json: bool) -> Result<()> {
    let storage = Storage::new(env);
    if storage.is_new {
        env_not_found(env);
    }

    let channels = Channels::new(env)?;
    let channel_list = channels.list();
    let package_list = &storage.packages;

    if json {
        let output = serde_json::json!({
            "channelList": channel_list,
            "packageList": package_list,
        });
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        println!("Environment {:?}\n", env);

        println!("Channels:");
        if channel_list.is_empty() {
            println!(" - <empty>");
        } else {
            for ch in &channel_list {
                println!(" - {}", ch);
            }
        }

        println!("\nPackages:");
        if package_list.is_empty() {
            println!(" - <empty>");
        } else {
            for pkg in package_list {
                println!(" - {}", pkg);
            }
        }
    }

    Ok(())
}

fn cmd_enter(env: &str, auto_rebuild: bool, verbose: bool) -> Result<()> {
    let storage = Storage::new(env);
    if storage.is_new {
        env_not_found(env);
    }

    let channels = Channels::new(env)?;
    let bin = cache_dir()
        .join(env)
        .join("result")
        .join("bin")
        .join(format!("dev-{}", env));

    if !bin.exists() {
        if !auto_rebuild {
            eprintln!("Environment needs rebuild, auto-rebuild disabled");
            if env == "default" {
                eprintln!(" $ dev rebuild");
            } else {
                eprintln!(" $ dev rebuild -e {}", env);
            }
            std::process::exit(1);
        }

        routine_stuff(&storage, &channels, verbose)?;
        rebuild(env, &storage, &channels, verbose)?;
    }

    // exec into the dev environment
    let nix_path = channels.get_nix_path();
    let err = Command::new(&bin)
        .env("NIX_PATH", &nix_path)
        .env("NIX_DEV", env)
        .exec();

    Err(anyhow!("Failed to exec: {}", err))
}

fn main() {
    let cli = Cli::parse();

    // Default is to rebuild; --no-rebuild disables it
    let auto_rebuild = !cli.no_rebuild;

    // Ensure directories exist
    fs::create_dir_all(cache_dir()).ok();
    fs::create_dir_all(config_dir()).ok();

    let result = match cli.command {
        Some(Commands::Add { pkgs }) => cmd_add(&cli.env, pkgs, auto_rebuild, cli.verbose),
        Some(Commands::Rm { pkgs }) => cmd_rm(&cli.env, pkgs, auto_rebuild, cli.verbose),
        Some(Commands::Rebuild) => cmd_rebuild(&cli.env, cli.verbose),
        Some(Commands::Update { fetch, all }) => {
            cmd_update(&cli.env, fetch, all, auto_rebuild, cli.verbose)
        }
        Some(Commands::Info { json }) => cmd_info(&cli.env, json),
        Some(Commands::Enter) | None => cmd_enter(&cli.env, auto_rebuild, cli.verbose),
    };

    if let Err(e) = result {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
}
