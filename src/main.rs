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
    dirs::cache_dir().unwrap().join("dev")
}

fn config_dir() -> PathBuf {
    dirs::config_dir().unwrap().join("dev")
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

/// Represents a package reference - either from a channel or a flake
#[derive(Clone, Debug, PartialEq)]
enum PackageRef {
    /// Channel package: channel.attr (e.g., nixpkgs.git)
    Channel { source: String, attr: String },
    /// Flake package: flake#attr (e.g., nixpkgs#git)
    Flake { source: String, attr: String },
}

impl PackageRef {
    fn parse(s: &str) -> Option<Self> {
        if let Some((source, attr)) = s.split_once('#') {
            Some(PackageRef::Flake {
                source: source.to_string(),
                attr: attr.to_string(),
            })
        } else if let Some((source, attr)) = s.split_once('.') {
            Some(PackageRef::Channel {
                source: source.to_string(),
                attr: attr.to_string(),
            })
        } else {
            None
        }
    }

    fn source_name(&self) -> &str {
        match self {
            PackageRef::Channel { source, .. } => source,
            PackageRef::Flake { source, .. } => source,
        }
    }

    fn to_string(&self) -> String {
        match self {
            PackageRef::Channel { source, attr } => format!("{}.{}", source, attr),
            PackageRef::Flake { source, attr } => format!("{}#{}", source, attr),
        }
    }

    /// Returns the nix expression to access this package
    fn to_nix_expr(&self) -> String {
        match self {
            PackageRef::Channel { source, attr } => format!("{}.{}", source, attr),
            PackageRef::Flake { source, attr } => {
                // Flakes are imported directly, so they work like channels
                format!("_flake_{}.{}", source, attr)
            }
        }
    }
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
        fs::write(&self.disk_path, self.packages.join("\n")).context("Failed to write storage")?;
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

    fn get_refs(&self) -> Vec<PackageRef> {
        self.packages.iter().filter_map(|p| PackageRef::parse(p)).collect()
    }

    fn list_channels(&self) -> Vec<String> {
        let mut channels: Vec<String> = self
            .packages
            .iter()
            .filter_map(|p| PackageRef::parse(p))
            .filter_map(|r| match r {
                PackageRef::Channel { source, .. } => Some(source),
                _ => None,
            })
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        channels.sort();
        channels
    }

    fn list_flakes(&self) -> Vec<String> {
        let mut flakes: Vec<String> = self
            .packages
            .iter()
            .filter_map(|p| PackageRef::parse(p))
            .filter_map(|r| match r {
                PackageRef::Flake { source, .. } => Some(source),
                _ => None,
            })
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        flakes.sort();
        flakes
    }
}

/// Manages both channels and flakes as sources
struct Sources {
    channels_path: PathBuf,
    flakes_path: PathBuf,
}

impl Sources {
    fn new(env: &str) -> Result<Self> {
        let base = cache_dir().join(env);
        let channels_path = base.join("channels");
        let flakes_path = base.join("flakes");
        fs::create_dir_all(&channels_path)?;
        fs::create_dir_all(&flakes_path)?;

        Ok(Sources {
            channels_path,
            flakes_path,
        })
    }

    fn has_channel(&self, name: &str) -> bool {
        self.channels_path.join(name).exists()
    }

    fn has_flake(&self, name: &str) -> bool {
        self.flakes_path.join(name).exists()
    }

    fn has(&self, pkg_ref: &PackageRef) -> bool {
        match pkg_ref {
            PackageRef::Channel { source, .. } => self.has_channel(source),
            PackageRef::Flake { source, .. } => self.has_flake(source),
        }
    }

    fn update_channel(&self, name: &str, verbose: bool) -> Result<()> {
        if verbose {
            eprintln!("Updating channel: {}", name);
        }

        let channel_path = resolve_channel(name)?;

        let gc_root = self.channels_path.join(name);
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
            bail!(
                "Failed to update channel {}: {}",
                name,
                result.stderr.trim()
            );
        }

        Ok(())
    }

    fn update_flake(&self, name: &str, verbose: bool) -> Result<()> {
        if verbose {
            eprintln!("Updating flake: {}", name);
        }

        if !has_flakes() {
            bail!("Flakes are not enabled in your nix installation");
        }

        let flake_path = resolve_flake(name)?;

        let gc_root = self.flakes_path.join(name);
        if gc_root.exists() {
            fs::remove_file(&gc_root).ok();
        }

        let result = spawn(
            "nix-store",
            &[
                "--realise",
                &flake_path,
                "--indirect",
                "--add-root",
                gc_root.to_str().unwrap(),
            ],
            true,
            None,
        )?;

        if !result.success {
            bail!("Failed to update flake {}: {}", name, result.stderr.trim());
        }

        Ok(())
    }

    fn update_source(&self, pkg_ref: &PackageRef, verbose: bool) -> Result<()> {
        match pkg_ref {
            PackageRef::Channel { source, .. } => self.update_channel(source, verbose),
            PackageRef::Flake { source, .. } => self.update_flake(source, verbose),
        }
    }

    fn remove_channel(&self, name: &str) {
        let path = self.channels_path.join(name);
        if path.exists() {
            fs::remove_file(&path).ok();
        }
    }

    fn remove_flake(&self, name: &str) {
        let path = self.flakes_path.join(name);
        if path.exists() {
            fs::remove_file(&path).ok();
        }
    }

    /// List channel GC roots that exist on disk (for garbage collection)
    fn list_channel_roots(&self) -> Vec<String> {
        fs::read_dir(&self.channels_path)
            .map(|entries| {
                entries
                    .filter_map(|e| e.ok())
                    .filter_map(|e| e.file_name().into_string().ok())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// List flake GC roots that exist on disk (for garbage collection)
    fn list_flake_roots(&self) -> Vec<String> {
        fs::read_dir(&self.flakes_path)
            .map(|entries| {
                entries
                    .filter_map(|e| e.ok())
                    .filter_map(|e| e.file_name().into_string().ok())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn get_nix_path(&self, storage: &Storage) -> String {
        storage
            .list_channels()
            .iter()
            .map(|channel| {
                format!(
                    "{}={}",
                    channel,
                    self.channels_path.join(channel).display()
                )
            })
            .collect::<Vec<_>>()
            .join(":")
    }

    fn get_flake_paths(&self, storage: &Storage) -> Vec<(String, PathBuf)> {
        storage
            .list_flakes()
            .iter()
            .map(|flake| (flake.clone(), self.flakes_path.join(flake)))
            .collect()
    }
}

fn resolve_channel(name: &str) -> Result<String> {
    let expr = format!("(<{}>)", name);
    let args = vec!["eval", "--raw", "--impure", "--expr", &expr];

    let result = spawn("nix", &args, true, None)?;

    let path = result.stdout.trim();
    if !path.starts_with('/') {
        bail!("nix: {}", result.stderr.trim());
    }

    Ok(path.to_string())
}

fn resolve_flake(name: &str) -> Result<String> {
    let expr = format!("(builtins.getFlake \"{}\").outPath", name);
    let args = vec!["eval", "--raw", "--impure", "--expr", &expr];

    let result = spawn("nix", &args, true, None)?;

    let path = result.stdout.trim();
    if !path.starts_with('/') {
        bail!("nix: {}", result.stderr.trim());
    }

    Ok(path.to_string())
}

fn check_channel_package_exists(channel: &str, attr: &str, sources: &Sources, storage: &Storage) -> Result<bool> {
    let attr_parts: Vec<String> = attr.split('.').map(|s| format!("\"{}\"", s)).collect();

    let expr = format!(
        "(let ch = (import <{}> {{}}); in ch ? {})",
        channel,
        attr_parts.join(".")
    );

    let args = vec!["eval", "--impure", "--expr", &expr];

    let result = spawn("nix", &args, true, Some(&sources.get_nix_path(storage)))?;

    if !result.success {
        bail!("nix: {}", result.stderr.trim());
    }

    Ok(result.stdout.trim() == "true")
}

fn check_flake_package_exists(flake: &str, attr: &str) -> Result<bool> {
    if !has_flakes() {
        bail!("Flakes are not enabled in your nix installation");
    }

    let attr_parts: Vec<String> = attr.split('.').map(|s| format!("\"{}\"", s)).collect();

    // First try legacyPackages (standard flake output)
    let expr = format!(
        "(let f = builtins.getFlake \"{}\"; in f.legacyPackages.${{builtins.currentSystem}} ? {})",
        flake,
        attr_parts.join(".")
    );

    let args = vec!["eval", "--impure", "--expr", &expr];
    let result = spawn("nix", &args, true, None)?;

    if result.success && result.stdout.trim() == "true" {
        return Ok(true);
    }

    // Also try packages output
    let expr = format!(
        "(let f = builtins.getFlake \"{}\"; in f.packages.${{builtins.currentSystem}} ? {})",
        flake,
        attr_parts.join(".")
    );

    let args = vec!["eval", "--impure", "--expr", &expr];
    let result = spawn("nix", &args, true, None)?;

    if !result.success {
        bail!("nix: {}", result.stderr.trim());
    }

    Ok(result.stdout.trim() == "true")
}

fn check_package_exists(pkg_ref: &PackageRef, sources: &Sources, storage: &Storage) -> Result<bool> {
    match pkg_ref {
        PackageRef::Channel { source, attr } => check_channel_package_exists(source, attr, sources, storage),
        PackageRef::Flake { source, attr } => check_flake_package_exists(source, attr),
    }
}

fn generate_nix(name: &str, storage: &Storage, sources: &Sources) -> String {
    let channel_imports: String = storage
        .list_channels()
        .iter()
        .map(|ch| format!("  {} = import <{}> {{}};", ch, ch))
        .collect::<Vec<_>>()
        .join("\n");

    let flake_imports: String = sources
        .get_flake_paths(storage)
        .iter()
        .map(|(name, path)| format!("  _flake_{} = import {} {{}};", name, path.display()))
        .collect::<Vec<_>>()
        .join("\n");

    let packages: String = storage
        .get_refs()
        .iter()
        .map(|p| format!("    ({})", p.to_nix_expr()))
        .collect::<Vec<_>>()
        .join("\n");

    let all_imports = if flake_imports.is_empty() {
        channel_imports
    } else if channel_imports.is_empty() {
        flake_imports
    } else {
        format!("{}\n{}", channel_imports, flake_imports)
    };

    format!(
        r#"{{ pkgs ? import <nixpkgs> {{}} }}:

let
{all_imports}
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
        all_imports = all_imports,
        name = name,
        packages = packages,
    )
}

fn rebuild(env: &str, storage: &Storage, sources: &Sources, verbose: bool) -> Result<()> {
    let env_cache = cache_dir().join(env);
    fs::create_dir_all(&env_cache)?;

    let nix_path = env_cache.join("default.nix");
    let result_path = env_cache.join("result");

    if verbose {
        eprintln!("Generating nix expression");
    }

    let nix_content = generate_nix(env, storage, sources);
    fs::write(&nix_path, &nix_content)?;

    if verbose {
        eprintln!("Generated nix:\n{}", nix_content);
    }

    println!("rebuilding {}...", env);

    let success = spawn_inherit(
        "nix-build",
        &[
            nix_path.to_str().unwrap(),
            "-o",
            result_path.to_str().unwrap(),
        ],
        Some(&sources.get_nix_path(storage)),
    )?;

    if !success {
        bail!("Build failed");
    }

    Ok(())
}

fn routine_stuff(storage: &Storage, sources: &Sources, verbose: bool) -> Result<()> {
    // Ensure nixpkgs channel exists (needed for buildFHSEnv)
    if !sources.has_channel("nixpkgs") {
        sources.update_channel("nixpkgs", verbose)?;
    }

    // Determine which sources we need
    let refs = storage.get_refs();

    let needed_channels: HashSet<String> = refs
        .iter()
        .filter_map(|r| match r {
            PackageRef::Channel { source, .. } => Some(source.clone()),
            _ => None,
        })
        .collect();

    let needed_flakes: HashSet<String> = refs
        .iter()
        .filter_map(|r| match r {
            PackageRef::Flake { source, .. } => Some(source.clone()),
            _ => None,
        })
        .collect();

    // GC unused channels (but always keep nixpkgs)
    for channel in sources.list_channel_roots() {
        if channel != "nixpkgs" && !needed_channels.contains(&channel) {
            if verbose {
                eprintln!("GC channel: {}", channel);
            }
            sources.remove_channel(&channel);
        }
    }

    // GC unused flakes
    for flake in sources.list_flake_roots() {
        if !needed_flakes.contains(&flake) {
            if verbose {
                eprintln!("GC flake: {}", flake);
            }
            sources.remove_flake(&flake);
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

fn cmd_add(env: &str, pkgs: Vec<String>, auto_rebuild: bool, verbose: bool) -> Result<()> {
    let mut storage = Storage::new(env);
    let sources = Sources::new(env)?;

    routine_stuff(&storage, &sources, verbose)?;

    let mut had_errors = false;

    for pkg in pkgs {
        // Parse the package reference
        let pkg_ref = match PackageRef::parse(&pkg) {
            Some(r) => r,
            None => {
                // No prefix, try nixpkgs.pkg first, then nixpkgs#pkg
                if verbose {
                    eprintln!("{}: no source prefix, trying nixpkgs.{}", pkg, pkg);
                }

                let channel_ref = PackageRef::Channel {
                    source: "nixpkgs".to_string(),
                    attr: pkg.clone(),
                };

                match check_package_exists(&channel_ref, &sources, &storage) {
                    Ok(true) => {
                        if verbose {
                            eprintln!("{}: found as nixpkgs.{}", pkg, pkg);
                        }
                        channel_ref
                    }
                    _ => {
                        // Try flake
                        if verbose {
                            eprintln!("{}: not in channel, trying nixpkgs#{}", pkg, pkg);
                        }
                        let flake_ref = PackageRef::Flake {
                            source: "nixpkgs".to_string(),
                            attr: pkg.clone(),
                        };

                        match check_package_exists(&flake_ref, &sources, &storage) {
                            Ok(true) => {
                                if verbose {
                                    eprintln!("{}: found as nixpkgs#{}", pkg, pkg);
                                }
                                flake_ref
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
                }
            }
        };

        // Verify package exists
        match check_package_exists(&pkg_ref, &sources, &storage) {
            Ok(true) => {}
            Ok(false) => {
                eprintln!("{}: does not exist or fails to evaluate", pkg_ref.to_string());
                had_errors = true;
                continue;
            }
            Err(e) => {
                eprintln!("{}: {}", pkg_ref.to_string(), e);
                had_errors = true;
                continue;
            }
        }

        // Ensure source is available
        if !sources.has(&pkg_ref) {
            if verbose {
                eprintln!("Adding source: {}", pkg_ref.source_name());
            }
            if let Err(e) = sources.update_source(&pkg_ref, verbose) {
                eprintln!("{}: failed to add source: {}", pkg_ref.to_string(), e);
                had_errors = true;
                continue;
            }
        }

        storage.add(&pkg_ref.to_string());
    }

    storage.write()?;

    if auto_rebuild {
        rebuild(env, &storage, &sources, verbose)?;
    }

    if had_errors {
        std::process::exit(1);
    }

    Ok(())
}

fn cmd_rm(env: &str, pkgs: Vec<String>, auto_rebuild: bool, verbose: bool) -> Result<()> {
    let mut storage = Storage::new(env);
    let sources = Sources::new(env)?;

    for pkg in pkgs {
        // Try exact match first
        if storage.contains(&pkg) {
            if verbose {
                eprintln!("Removing: {}", pkg);
            }
            storage.remove(&pkg);
            continue;
        }

        // Try with nixpkgs. prefix
        let channel_pkg = format!("nixpkgs.{}", pkg);
        if storage.contains(&channel_pkg) {
            if verbose {
                eprintln!("Removing: {}", channel_pkg);
            }
            storage.remove(&channel_pkg);
            continue;
        }

        // Try with nixpkgs# prefix
        let flake_pkg = format!("nixpkgs#{}", pkg);
        if storage.contains(&flake_pkg) {
            if verbose {
                eprintln!("Removing: {}", flake_pkg);
            }
            storage.remove(&flake_pkg);
            continue;
        }

        println!("{}: not installed", pkg);
    }

    routine_stuff(&storage, &sources, verbose)?;
    storage.write()?;

    if auto_rebuild {
        rebuild(env, &storage, &sources, verbose)?;
    }

    Ok(())
}

fn cmd_rebuild(env: &str, verbose: bool) -> Result<()> {
    let storage = Storage::new(env);
    if storage.is_new {
        env_not_found(env);
    }

    let sources = Sources::new(env)?;

    routine_stuff(&storage, &sources, verbose)?;
    rebuild(env, &storage, &sources, verbose)?;

    Ok(())
}

fn cmd_update(env: &str, fetch: bool, all: bool, auto_rebuild: bool, verbose: bool) -> Result<()> {
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

        let sources = Sources::new(&env)?;

        routine_stuff(&storage, &sources, verbose)?;

        println!("Updating environment: {}", env);

        for channel in storage.list_channels() {
            sources.update_channel(&channel, verbose)?;
        }

        for flake in storage.list_flakes() {
            sources.update_flake(&flake, verbose)?;
        }

        if auto_rebuild {
            rebuild(&env, &storage, &sources, verbose)?;
        }
    }

    Ok(())
}

fn cmd_info(env: &str, json: bool) -> Result<()> {
    let storage = Storage::new(env);
    if storage.is_new {
        env_not_found(env);
    }

    let channel_list = storage.list_channels();
    let flake_list = storage.list_flakes();
    let package_list = &storage.packages;

    if json {
        let output = serde_json::json!({
            "channels": channel_list,
            "flakes": flake_list,
            "packages": package_list,
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

        println!("\nFlakes:");
        if flake_list.is_empty() {
            println!(" - <empty>");
        } else {
            for fl in &flake_list {
                println!(" - {}", fl);
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

    let sources = Sources::new(env)?;
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

        routine_stuff(&storage, &sources, verbose)?;
        rebuild(env, &storage, &sources, verbose)?;
    }

    // exec into the dev environment
    let nix_path = sources.get_nix_path(&storage);
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
