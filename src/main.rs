use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};
use std::collections::HashSet;
use std::fs;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};

#[derive(Parser)]
#[command(name = "fhs")]
#[command(about = "CLI for managing FHS environments on nixOS", long_about = None)]
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

    /// List all environments
    List {
        /// Print list in JSON
        #[arg(short, long)]
        json: bool,
    },

    /// Enter an environment
    Enter,

    /// Update global nixpkgs (used for buildFHSEnv)
    UpdateGlobal,
}

fn cache_dir() -> PathBuf {
    if let Ok(path) = std::env::var("DEV_CACHE_DIR") {
        PathBuf::from(path)
    } else {
        dirs::cache_dir().unwrap().join("fhs")
    }
}

fn config_dir() -> PathBuf {
    if let Ok(path) = std::env::var("DEV_CONFIG_DIR") {
        PathBuf::from(path)
    } else {
        dirs::config_dir().unwrap().join("fhs")
    }
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
            PackageRef::Channel { source, attr } => {
                format!("_channel_{}.{}", source, attr)
            },
            PackageRef::Flake { source, attr } => {
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

        let packages: Vec<String> = if disk_path.exists() {
            fs::read_to_string(&disk_path)
                .unwrap_or_default()
                .lines()
                .filter(|l| !l.is_empty())
                .map(|l| {
                    // Convert entries without . or # to nixpkgs.entry
                    if !l.contains('.') && !l.contains('#') {
                        format!("nixpkgs.{}", l)
                    } else {
                        l.to_string()
                    }
                })
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

/// Manages global nixpkgs used for buildFHSEnv (independent of environments)
struct GlobalSources {
    path: PathBuf,
}

impl GlobalSources {
    fn new() -> Result<Self> {
        let path = cache_dir().join("global");
        fs::create_dir_all(&path)?;
        Ok(GlobalSources { path })
    }

    fn nixpkgs_path(&self) -> PathBuf {
        self.path.join("nixpkgs")
    }

    fn has_nixpkgs(&self) -> bool {
        self.nixpkgs_path().exists()
    }

    fn update_nixpkgs(&self, verbose: bool) -> Result<()> {
        if verbose {
            eprintln!("Updating global nixpkgs");
        }

        // Try channel first, fall back to flake
        let nixpkgs_path = match resolve_channel("nixpkgs") {
            Ok(path) => {
                if verbose {
                    eprintln!("Using nixpkgs channel");
                }
                path
            }
            Err(_) => {
                if verbose {
                    eprintln!("Channel not found, trying nixpkgs flake");
                }
                resolve_flake("nixpkgs").context("Failed to resolve nixpkgs from both channel and flake")?
            }
        };

        let gc_root = self.nixpkgs_path();
        if gc_root.exists() {
            fs::remove_file(&gc_root).ok();
        }

        let result = spawn(
            "nix-store",
            &[
                "--realise",
                &nixpkgs_path,
                "--indirect",
                "--add-root",
                gc_root.to_str().unwrap(),
            ],
            true,
            None,
        )?;

        if !result.success {
            bail!(
                "Failed to update global nixpkgs: {}",
                result.stderr.trim()
            );
        }

        Ok(())
    }

    fn ensure_nixpkgs(&self, verbose: bool) -> Result<()> {
        if !self.has_nixpkgs() {
            self.update_nixpkgs(verbose)?;
        }
        Ok(())
    }

    fn get_nixpkgs_store_path(&self) -> Result<PathBuf> {
        let gc_root = self.nixpkgs_path();
        if !gc_root.exists() {
            bail!("Global nixpkgs not initialized. Run 'fhs update-global' first.");
        }
        fs::canonicalize(&gc_root).context("Failed to resolve global nixpkgs path")
    }
}

fn nix_eval(expr: &str, raw: bool, nix_path: Option<&str>) -> Result<SpawnResult> {
    let args: Vec<&str> = if raw {
        vec!["eval", "--raw", "--impure", "--expr", expr]
    } else {
        vec!["eval", "--impure", "--expr", expr]
    };
    spawn("nix", &args, true, nix_path)
}

fn resolve_channel(name: &str) -> Result<String> {
    let expr = format!("(<{}>)", name);
    let result = nix_eval(&expr, true, None)?;

    let path = result.stdout.trim();
    if !path.starts_with('/') {
        bail!("nix: {}", result.stderr.trim());
    }

    Ok(path.to_string())
}

fn resolve_flake(name: &str) -> Result<String> {
    let expr = format!("(builtins.getFlake \"{}\").outPath", name);
    let result = nix_eval(&expr, true, None)?;

    let path = result.stdout.trim();
    if !path.starts_with('/') {
        bail!("nix: {}", result.stderr.trim());
    }

    Ok(path.to_string())
}

fn check_channel_package_exists(channel: &str, attr: &str, verbose: bool) -> Result<bool> {
    let attr_parts: Vec<String> = attr.split('.').map(|s| format!("\"{}\"", s)).collect();
    let expr = format!(
        "(let ch = (import <{}> {{}}); in ch ? {})",
        channel,
        attr_parts.join(".")
    );

    // Use system NIX_PATH (None) to check against all available system channels
    // If eval fails (e.g., empty NIX_PATH), treat as "not found" to allow flake fallback
    let result = match nix_eval(&expr, false, None) {
        Ok(r) => r,
        Err(e) => {
            if verbose {
                eprintln!("channel check failed: {}", e);
            }
            return Ok(false);
        }
    };

    if !result.success {
        if verbose {
            eprintln!("channel check failed: {}", result.stderr.trim());
        }
        return Ok(false);
    }

    Ok(result.stdout.trim() == "true")
}

fn check_flake_package_exists(flake: &str, attr: &str, verbose: bool) -> Result<bool> {
    if !has_flakes() {
        if verbose {
            eprintln!("flake check failed: flakes are not enabled");
        }
        return Ok(false);
    }

    let attr_parts: Vec<String> = attr.split('.').map(|s| format!("\"{}\"", s)).collect();

    // First try legacyPackages (standard flake output)
    let expr = format!(
        "(let f = builtins.getFlake \"{}\"; in f.legacyPackages.${{builtins.currentSystem}} ? {})",
        flake,
        attr_parts.join(".")
    );

    let result = match nix_eval(&expr, false, None) {
        Ok(r) => r,
        Err(e) => {
            if verbose {
                eprintln!("flake check failed: {}", e);
            }
            return Ok(false);
        }
    };
    if result.success && result.stdout.trim() == "true" {
        return Ok(true);
    }

    // Also try packages output
    let expr = format!(
        "(let f = builtins.getFlake \"{}\"; in f.packages.${{builtins.currentSystem}} ? {})",
        flake,
        attr_parts.join(".")
    );

    let result = match nix_eval(&expr, false, None) {
        Ok(r) => r,
        Err(e) => {
            if verbose {
                eprintln!("flake check failed: {}", e);
            }
            return Ok(false);
        }
    };

    if !result.success {
        if verbose {
            eprintln!("flake check failed: {}", result.stderr.trim());
        }
        return Ok(false);
    }

    Ok(result.stdout.trim() == "true")
}

fn check_package_exists(pkg_ref: &PackageRef, verbose: bool) -> Result<bool> {
    match pkg_ref {
        PackageRef::Channel { source, attr } => check_channel_package_exists(source, attr, verbose),
        PackageRef::Flake { source, attr } => check_flake_package_exists(source, attr, verbose),
    }
}

fn generate_nix(name: &str, storage: &Storage, sources: &Sources, global_nixpkgs: &PathBuf) -> String {
    let channel_imports: String = storage
        .list_channels()
        .iter()
        .map(|ch| format!("  _channel_{} = import <{}> {{}};", ch, ch))
        .collect::<Vec<_>>()
        .join("\n");

    let flake_imports: String = sources
        .get_flake_paths(storage)
        .iter()
        .map(|(name, path)| {
            let resolved = fs::read_link(path).unwrap();
            format!("  _flake_{} = (let flake = builtins.getFlake(\"{}\"); in (if flake ? legacyPackages.${{builtins.currentSystem}} then flake.legacyPackages.${{builtins.currentSystem}} else flake.packages.${{builtins.currentSystem}}));", name, resolved.display())
        })
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

    let global_nixpkgs_path = global_nixpkgs.display();

    format!(
        r#"let
  pkgs = import {global_nixpkgs_path} {{}};
{all_imports}
in
(pkgs.buildFHSEnv {{
  name = "fhs-{name}";
  extraOutputsToInstall = ["include" "dev"];

  targetPkgs = pkgs: with pkgs; [
{packages}
  ];

  multiPkgs = pkgs: with pkgs; [
  ];

  profile = ''
    export IS_FHS=1
    export FHS_ENV="{name}"
  '';

  runScript = ''$SHELL'';
}})"#,
        global_nixpkgs_path = global_nixpkgs_path,
        all_imports = all_imports,
        name = name,
        packages = packages,
    )
}

fn rebuild(env: &str, storage: &Storage, sources: &Sources, global: &GlobalSources, verbose: bool) -> Result<()> {
    let env_cache = cache_dir().join(env);
    fs::create_dir_all(&env_cache)?;

    let nix_path = env_cache.join("default.nix");
    let result_path = env_cache.join("result");

    // Ensure global nixpkgs is available
    global.ensure_nixpkgs(verbose)?;
    let global_nixpkgs = global.get_nixpkgs_store_path()?;

    if verbose {
        eprintln!("Generating nix expression");
    }

    let nix_content = generate_nix(env, storage, sources, &global_nixpkgs);
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

    // GC unused channels
    for channel in sources.list_channel_roots() {
        if !needed_channels.contains(&channel) {
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

    for channel in needed_channels {
        if !sources.has_channel(&channel) {
            sources.update_channel(&channel, verbose)?;
        }
    }

    for flake in needed_flakes {
        if !sources.has_flake(&flake) {
            sources.update_flake(&flake, verbose)?;
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
        eprintln!(" $ fhs add <package>");
    } else {
        eprintln!(" $ fhs add -e {} <package>", env);
    }
    std::process::exit(1);
}

fn cmd_add(env: &str, pkgs: Vec<String>, auto_rebuild: bool, verbose: bool) -> Result<()> {
    let mut storage = Storage::new(env);
    let sources = Sources::new(env)?;
    let global = GlobalSources::new()?;

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

                match check_package_exists(&channel_ref, verbose) {
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

                        match check_package_exists(&flake_ref, verbose) {
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

        // Ensure source is available before checking package exists
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

        // Verify package exists
        match check_package_exists(&pkg_ref, verbose) {
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

        storage.add(&pkg_ref.to_string());
    }

    storage.write()?;

    if auto_rebuild {
        rebuild(env, &storage, &sources, &global, verbose)?;
    }

    if had_errors {
        std::process::exit(1);
    }

    Ok(())
}

fn cmd_rm(env: &str, pkgs: Vec<String>, auto_rebuild: bool, verbose: bool) -> Result<()> {
    let mut storage = Storage::new(env);
    let sources = Sources::new(env)?;
    let global = GlobalSources::new()?;

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
        rebuild(env, &storage, &sources, &global, verbose)?;
    }

    Ok(())
}

fn cmd_rebuild(env: &str, verbose: bool) -> Result<()> {
    let storage = Storage::new(env);
    if storage.is_new {
        env_not_found(env);
    }

    let sources = Sources::new(env)?;
    let global = GlobalSources::new()?;

    routine_stuff(&storage, &sources, verbose)?;
    rebuild(env, &storage, &sources, &global, verbose)?;

    Ok(())
}

fn cmd_update(env: &str, fetch: bool, all: bool, auto_rebuild: bool, verbose: bool) -> Result<()> {
    if fetch {
        println!("Fetching channels...");
        spawn_inherit("nix-channel", &["--update", "-vv"], None)?;
    }

    let global = GlobalSources::new()?;

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
            rebuild(&env, &storage, &sources, &global, verbose)?;
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

fn cmd_list(json: bool) -> Result<()> {
    let envs: Vec<String> = fs::read_dir(config_dir())
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|name| name.starts_with("env."))
                .map(|name| name.strip_prefix("env.").unwrap().to_string())
                .collect()
        })
        .unwrap_or_default();

    if json {
        let mut env_data: Vec<serde_json::Value> = Vec::new();
        for env in &envs {
            let storage = Storage::new(env);
            env_data.push(serde_json::json!({
                "name": env,
                "packages": storage.packages,
            }));
        }
        println!("{}", serde_json::to_string_pretty(&env_data)?);
    } else {
        if envs.is_empty() {
            println!("No environments found.");
            println!("\nCreate one with:");
            println!("  $ fhs add <package>");
        } else {
            for env in &envs {
                let storage = Storage::new(env);
                println!("{}:", env);
                if storage.packages.is_empty() {
                    println!("  <empty>");
                } else {
                    for pkg in &storage.packages {
                        println!("  - {}", pkg);
                    }
                }
                println!();
            }
        }
    }

    Ok(())
}

fn cmd_update_global(verbose: bool) -> Result<()> {
    let global = GlobalSources::new()?;
    global.update_nixpkgs(verbose)?;
    println!("Global nixpkgs updated successfully.");
    Ok(())
}

fn cmd_enter(env: &str, auto_rebuild: bool, verbose: bool) -> Result<()> {
    let storage = Storage::new(env);
    if storage.is_new {
        env_not_found(env);
    }

    let sources = Sources::new(env)?;
    let global = GlobalSources::new()?;
    let bin = cache_dir()
        .join(env)
        .join("result")
        .join("bin")
        .join(format!("fhs-{}", env));

    if !bin.exists() {
        if !auto_rebuild {
            eprintln!("Environment needs rebuild, auto-rebuild disabled");
            if env == "default" {
                eprintln!(" $ fhs rebuild");
            } else {
                eprintln!(" $ fhs rebuild -e {}", env);
            }
            std::process::exit(1);
        }

        routine_stuff(&storage, &sources, verbose)?;
        rebuild(env, &storage, &sources, &global, verbose)?;
    }

    // exec into the fhs environment
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
        Some(Commands::List { json }) => cmd_list(json),
        Some(Commands::UpdateGlobal) => cmd_update_global(cli.verbose),
        Some(Commands::Enter) | None => cmd_enter(&cli.env, auto_rebuild, cli.verbose),
    };

    if let Err(e) = result {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tempfile::TempDir;

    // Mutex to ensure tests don't run in parallel (they modify env vars)
    static TEST_MUTEX: Mutex<()> = Mutex::new(());

    struct TestEnv {
        _temp_dir: TempDir,
        cache_path: PathBuf,
        config_path: PathBuf,
    }

    impl TestEnv {
        fn new() -> Self {
            let temp_dir = TempDir::new().unwrap();
            let cache_path = temp_dir.path().join("cache");
            let config_path = temp_dir.path().join("config");

            fs::create_dir_all(&cache_path).unwrap();
            fs::create_dir_all(&config_path).unwrap();

            // SAFETY: Tests run with mutex lock, so env var access is serialized
            unsafe {
                std::env::set_var("DEV_CACHE_DIR", &cache_path);
                std::env::set_var("DEV_CONFIG_DIR", &config_path);
            }

            TestEnv {
                _temp_dir: temp_dir,
                cache_path,
                config_path,
            }
        }
    }

    impl Drop for TestEnv {
        fn drop(&mut self) {
            // SAFETY: Tests run with mutex lock, so env var access is serialized
            unsafe {
                std::env::remove_var("DEV_CACHE_DIR");
                std::env::remove_var("DEV_CONFIG_DIR");
            }
        }
    }

    #[test]
    fn test_package_ref_parse_channel() {
        let _lock = TEST_MUTEX.lock().unwrap();

        let pkg = PackageRef::parse("nixpkgs.git").unwrap();
        assert!(matches!(pkg, PackageRef::Channel { source, attr } if source == "nixpkgs" && attr == "git"));
    }

    #[test]
    fn test_package_ref_parse_flake() {
        let _lock = TEST_MUTEX.lock().unwrap();

        let pkg = PackageRef::parse("nixpkgs#hello").unwrap();
        assert!(matches!(pkg, PackageRef::Flake { source, attr } if source == "nixpkgs" && attr == "hello"));
    }

    #[test]
    fn test_package_ref_parse_no_prefix() {
        let _lock = TEST_MUTEX.lock().unwrap();

        let pkg = PackageRef::parse("git");
        assert!(pkg.is_none());
    }

    #[test]
    fn test_package_ref_to_string() {
        let _lock = TEST_MUTEX.lock().unwrap();

        let channel = PackageRef::Channel {
            source: "nixpkgs".to_string(),
            attr: "git".to_string(),
        };
        assert_eq!(channel.to_string(), "nixpkgs.git");

        let flake = PackageRef::Flake {
            source: "nixpkgs".to_string(),
            attr: "hello".to_string(),
        };
        assert_eq!(flake.to_string(), "nixpkgs#hello");
    }

    #[test]
    fn test_storage_new_empty() {
        let _lock = TEST_MUTEX.lock().unwrap();
        let _env = TestEnv::new();

        let storage = Storage::new("test");
        assert!(storage.is_new);
        assert!(storage.packages.is_empty());
    }

    #[test]
    fn test_storage_add_and_write() {
        let _lock = TEST_MUTEX.lock().unwrap();
        let _env = TestEnv::new();

        let mut storage = Storage::new("test");
        storage.add("nixpkgs.git");
        storage.add("nixpkgs.curl");
        storage.write().unwrap();

        // Reload and verify
        let storage2 = Storage::new("test");
        assert!(!storage2.is_new);
        assert_eq!(storage2.packages.len(), 2);
        assert!(storage2.contains("nixpkgs.git"));
        assert!(storage2.contains("nixpkgs.curl"));
    }

    #[test]
    fn test_storage_remove() {
        let _lock = TEST_MUTEX.lock().unwrap();
        let _env = TestEnv::new();

        let mut storage = Storage::new("test");
        storage.add("nixpkgs.git");
        storage.add("nixpkgs.curl");
        storage.remove("nixpkgs.git");

        assert!(!storage.contains("nixpkgs.git"));
        assert!(storage.contains("nixpkgs.curl"));
    }

    #[test]
    fn test_storage_migration_unprefixed() {
        let _lock = TEST_MUTEX.lock().unwrap();
        let env = TestEnv::new();

        // Write unprefixed entries directly
        let env_file = env.config_path.join("env.migration");
        fs::write(&env_file, "git\ncurl\nvim").unwrap();

        // Load and verify migration
        let storage = Storage::new("migration");
        assert_eq!(storage.packages.len(), 3);
        assert!(storage.contains("nixpkgs.git"));
        assert!(storage.contains("nixpkgs.curl"));
        assert!(storage.contains("nixpkgs.vim"));
    }

    #[test]
    fn test_storage_list_channels() {
        let _lock = TEST_MUTEX.lock().unwrap();
        let _env = TestEnv::new();

        let mut storage = Storage::new("test");
        storage.add("nixpkgs.git");
        storage.add("nixpkgs.curl");
        storage.add("unstable.firefox");

        let channels = storage.list_channels();
        assert_eq!(channels.len(), 2);
        assert!(channels.contains(&"nixpkgs".to_string()));
        assert!(channels.contains(&"unstable".to_string()));
    }

    #[test]
    fn test_storage_list_flakes() {
        let _lock = TEST_MUTEX.lock().unwrap();
        let _env = TestEnv::new();

        let mut storage = Storage::new("test");
        storage.add("nixpkgs#hello");
        storage.add("nixpkgs#cowsay");
        storage.add("home-manager#home-manager");

        let flakes = storage.list_flakes();
        assert_eq!(flakes.len(), 2);
        assert!(flakes.contains(&"nixpkgs".to_string()));
        assert!(flakes.contains(&"home-manager".to_string()));
    }

    #[test]
    fn test_storage_mixed_sources() {
        let _lock = TEST_MUTEX.lock().unwrap();
        let _env = TestEnv::new();

        let mut storage = Storage::new("test");
        storage.add("nixpkgs.git");
        storage.add("nixpkgs#hello");

        assert_eq!(storage.list_channels().len(), 1);
        assert_eq!(storage.list_flakes().len(), 1);
    }

    #[test]
    fn test_sources_new() {
        let _lock = TEST_MUTEX.lock().unwrap();
        let env = TestEnv::new();

        let _sources = Sources::new("test").unwrap();
        assert!(env.cache_path.join("test/channels").exists());
        assert!(env.cache_path.join("test/flakes").exists());
    }

    #[test]
    fn test_sources_has_channel() {
        let _lock = TEST_MUTEX.lock().unwrap();
        let env = TestEnv::new();

        let sources = Sources::new("test").unwrap();

        // Create a fake channel GC root
        let channel_path = env.cache_path.join("test/channels/nixpkgs");
        fs::write(&channel_path, "fake").unwrap();

        assert!(sources.has_channel("nixpkgs"));
        assert!(!sources.has_channel("unstable"));
    }

    #[test]
    fn test_sources_has_flake() {
        let _lock = TEST_MUTEX.lock().unwrap();
        let env = TestEnv::new();

        let sources = Sources::new("test").unwrap();

        // Create a fake flake GC root
        let flake_path = env.cache_path.join("test/flakes/nixpkgs");
        fs::write(&flake_path, "fake").unwrap();

        assert!(sources.has_flake("nixpkgs"));
        assert!(!sources.has_flake("home-manager"));
    }

    #[test]
    fn test_sources_remove_channel() {
        let _lock = TEST_MUTEX.lock().unwrap();
        let env = TestEnv::new();

        let sources = Sources::new("test").unwrap();

        let channel_path = env.cache_path.join("test/channels/nixpkgs");
        fs::write(&channel_path, "fake").unwrap();
        assert!(channel_path.exists());

        sources.remove_channel("nixpkgs");
        assert!(!channel_path.exists());
    }

    #[test]
    fn test_sources_remove_flake() {
        let _lock = TEST_MUTEX.lock().unwrap();
        let env = TestEnv::new();

        let sources = Sources::new("test").unwrap();

        let flake_path = env.cache_path.join("test/flakes/nixpkgs");
        fs::write(&flake_path, "fake").unwrap();
        assert!(flake_path.exists());

        sources.remove_flake("nixpkgs");
        assert!(!flake_path.exists());
    }

    #[test]
    fn test_sources_list_roots() {
        let _lock = TEST_MUTEX.lock().unwrap();
        let env = TestEnv::new();

        let sources = Sources::new("test").unwrap();

        // Create fake GC roots
        fs::write(env.cache_path.join("test/channels/nixpkgs"), "fake").unwrap();
        fs::write(env.cache_path.join("test/channels/unstable"), "fake").unwrap();
        fs::write(env.cache_path.join("test/flakes/home-manager"), "fake").unwrap();

        let channel_roots = sources.list_channel_roots();
        assert_eq!(channel_roots.len(), 2);

        let flake_roots = sources.list_flake_roots();
        assert_eq!(flake_roots.len(), 1);
    }

    #[test]
    fn test_sources_get_nix_path() {
        let _lock = TEST_MUTEX.lock().unwrap();
        let _env = TestEnv::new();

        let sources = Sources::new("test").unwrap();

        let mut storage = Storage::new("test");
        storage.add("nixpkgs.git");
        storage.add("unstable.firefox");

        let nix_path = sources.get_nix_path(&storage);
        assert!(nix_path.contains("nixpkgs="));
        assert!(nix_path.contains("unstable="));
    }

    #[test]
    fn test_generate_nix_channels_only() {
        let _lock = TEST_MUTEX.lock().unwrap();
        let _env = TestEnv::new();

        let mut storage = Storage::new("test");
        storage.add("nixpkgs.git");
        storage.add("nixpkgs.curl");

        let sources = Sources::new("test").unwrap();
        let global_nixpkgs = PathBuf::from("/nix/store/fake-nixpkgs");
        let nix = generate_nix("test", &storage, &sources, &global_nixpkgs);

        assert!(nix.contains("pkgs = import /nix/store/fake-nixpkgs {};"));
        assert!(nix.contains("_channel_nixpkgs = import <nixpkgs> {};"));
        assert!(nix.contains("(_channel_nixpkgs.curl)"));
        assert!(nix.contains("(_channel_nixpkgs.git)"));
        assert!(nix.contains("fhs-test"));
    }

    #[test]
    fn test_generate_nix_with_flakes() {
        let _lock = TEST_MUTEX.lock().unwrap();
        let test_env = TestEnv::new();

        let mut storage = Storage::new("test");
        storage.add("nixpkgs.git");
        storage.add("myflake#hello");

        let sources = Sources::new("test").unwrap();

        // Create fake flake path
        let flake_path = test_env.cache_path.join("test/flakes/myflake");
        std::os::unix::fs::symlink("/flake", &flake_path).unwrap();

        let global_nixpkgs = PathBuf::from("/nix/store/fake-nixpkgs");
        let nix = generate_nix("test", &storage, &sources, &global_nixpkgs);

        assert!(nix.contains("pkgs = import /nix/store/fake-nixpkgs {};"));
        assert!(nix.contains("_channel_nixpkgs = import <nixpkgs> {};"));
        assert!(nix.contains("_flake_myflake = (let flake "));
        assert!(nix.contains("(_channel_nixpkgs.git)"));
        assert!(nix.contains("(_flake_myflake.hello)"));
    }

    #[test]
    fn test_env_not_built_detection() {
        let _lock = TEST_MUTEX.lock().unwrap();
        let test_env = TestEnv::new();

        // Create a storage file but no result binary
        let mut storage = Storage::new("unbuilt");
        storage.add("nixpkgs.git");
        storage.write().unwrap();

        let bin_path = test_env.cache_path
            .join("unbuilt/result/bin/fhs-unbuilt");

        assert!(!bin_path.exists());
    }

    #[test]
    fn test_multiple_environments() {
        let _lock = TEST_MUTEX.lock().unwrap();
        let env = TestEnv::new();

        // Create multiple environments
        let mut storage1 = Storage::new("env1");
        storage1.add("nixpkgs.git");
        storage1.write().unwrap();

        let mut storage2 = Storage::new("env2");
        storage2.add("nixpkgs.curl");
        storage2.write().unwrap();

        // Verify they're independent
        let reload1 = Storage::new("env1");
        let reload2 = Storage::new("env2");

        assert!(reload1.contains("nixpkgs.git"));
        assert!(!reload1.contains("nixpkgs.curl"));

        assert!(reload2.contains("nixpkgs.curl"));
        assert!(!reload2.contains("nixpkgs.git"));

        // List all env files
        let envs: Vec<String> = fs::read_dir(&env.config_path)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|name| name.starts_with("env."))
            .collect();

        assert_eq!(envs.len(), 2);
    }

    #[test]
    fn test_package_ref_source_name() {
        let _lock = TEST_MUTEX.lock().unwrap();

        let channel = PackageRef::Channel {
            source: "nixpkgs".to_string(),
            attr: "git".to_string(),
        };
        assert_eq!(channel.source_name(), "nixpkgs");

        let flake = PackageRef::Flake {
            source: "home-manager".to_string(),
            attr: "home-manager".to_string(),
        };
        assert_eq!(flake.source_name(), "home-manager");
    }

    #[test]
    fn test_package_ref_to_nix_expr() {
        let _lock = TEST_MUTEX.lock().unwrap();

        let channel = PackageRef::Channel {
            source: "nixpkgs".to_string(),
            attr: "git".to_string(),
        };
        assert_eq!(channel.to_nix_expr(), "_channel_nixpkgs.git");

        let flake = PackageRef::Flake {
            source: "myflake".to_string(),
            attr: "hello".to_string(),
        };
        assert_eq!(flake.to_nix_expr(), "_flake_myflake.hello");
    }

    #[test]
    fn test_storage_packages_sorted() {
        let _lock = TEST_MUTEX.lock().unwrap();
        let _env = TestEnv::new();

        let mut storage = Storage::new("test");
        storage.add("nixpkgs.zsh");
        storage.add("nixpkgs.git");
        storage.add("nixpkgs.curl");

        assert_eq!(storage.packages[0], "nixpkgs.curl");
        assert_eq!(storage.packages[1], "nixpkgs.git");
        assert_eq!(storage.packages[2], "nixpkgs.zsh");
    }

    #[test]
    fn test_storage_no_duplicates() {
        let _lock = TEST_MUTEX.lock().unwrap();
        let _env = TestEnv::new();

        let mut storage = Storage::new("test");
        storage.add("nixpkgs.git");
        storage.add("nixpkgs.git");
        storage.add("nixpkgs.git");

        assert_eq!(storage.packages.len(), 1);
    }

    #[test]
    fn test_storage_get_refs() {
        let _lock = TEST_MUTEX.lock().unwrap();
        let _env = TestEnv::new();

        let mut storage = Storage::new("test");
        storage.add("nixpkgs.git");
        storage.add("nixpkgs#hello");

        let refs = storage.get_refs();
        assert_eq!(refs.len(), 2);

        assert!(refs.iter().any(|r| matches!(r, PackageRef::Channel { source, attr } if source == "nixpkgs" && attr == "git")));
        assert!(refs.iter().any(|r| matches!(r, PackageRef::Flake { source, attr } if source == "nixpkgs" && attr == "hello")));
    }

    #[test]
    fn test_interop_add_mixed() {
        let _lock = TEST_MUTEX.lock().unwrap();
        let _env = TestEnv::new();

        cmd_add("add-mixed", vec!["nixpkgs#hello".into(), "nixpkgs.nixVersions.latest".into(), "cloud-utils".into()], true, true).unwrap();
    }

    #[test]
    fn test_interop_build_mixed() {
        let _lock = TEST_MUTEX.lock().unwrap();
        let _env = TestEnv::new();

        let mut storage = Storage::new("build-mixed");
        storage.add("nixpkgs.nixVersions.latest");
        storage.add("nixpkgs#hello");
        storage.write().unwrap();

        cmd_rebuild("build-mixed", true).unwrap();
    }

    #[test]
    fn test_cmd_list_empty() {
        let _lock = TEST_MUTEX.lock().unwrap();
        let _env = TestEnv::new();

        // Should not error with no environments
        cmd_list(false).unwrap();
        cmd_list(true).unwrap();
    }

    #[test]
    fn test_cmd_list_with_envs() {
        let _lock = TEST_MUTEX.lock().unwrap();
        let _env = TestEnv::new();

        // Create some environments
        let mut storage1 = Storage::new("list-test1");
        storage1.add("nixpkgs.git");
        storage1.add("nixpkgs.curl");
        storage1.write().unwrap();

        let mut storage2 = Storage::new("list-test2");
        storage2.add("nixpkgs#hello");
        storage2.write().unwrap();

        // Should not error
        cmd_list(false).unwrap();
        cmd_list(true).unwrap();
    }
}
