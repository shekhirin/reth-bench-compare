//! CLI argument parsing and main command orchestration.

use alloy_provider::{Provider, ProviderBuilder};
use clap::Parser;
use eyre::{eyre, Result, WrapErr};
use reth_chainspec::Chain;
use reth_cli_runner::CliContext;
use reth_node_core::args::{DatadirArgs, LogArgs};
use reth_tracing::FileWorkerGuard;
use std::{net::TcpListener, path::PathBuf};
use tokio::process::Command;
use tracing::{debug, info, warn};

use crate::{
    benchmark::BenchmarkRunner, comparison::ComparisonGenerator, compilation::CompilationManager,
    git::GitManager, node::NodeManager,
};

/// Automated reth benchmark comparison between git references
#[derive(Debug, Parser)]
#[command(
    name = "reth-bench-compare",
    about = "Compare reth performance between two git references (branches or tags)",
    version
)]
pub struct Args {
    /// Git reference (branch or tag) to use as baseline for comparison
    #[arg(long, value_name = "REF")]
    pub baseline_ref: String,

    /// Git reference (branch or tag) to compare against the baseline
    #[arg(long, value_name = "REF")]
    pub feature_ref: String,

    #[command(flatten)]
    pub datadir: DatadirArgs,

    /// Number of blocks to benchmark
    #[arg(long, value_name = "N", default_value = "100")]
    pub blocks: u64,

    /// RPC endpoint for fetching block data
    #[arg(long, value_name = "URL")]
    pub rpc_url: Option<String>,

    /// JWT secret file path
    ///
    /// If not provided, defaults to `<datadir>/<chain>/jwt.hex`.
    /// If the file doesn't exist, it will be created automatically.
    #[arg(long, value_name = "PATH")]
    pub jwt_secret: Option<PathBuf>,

    /// Output directory for benchmark results
    #[arg(long, value_name = "PATH", default_value = "./reth-bench-compare")]
    pub output_dir: String,

    /// Skip git branch validation (useful for testing)
    #[arg(long)]
    pub skip_git_validation: bool,

    /// Port for reth metrics endpoint
    #[arg(long, value_name = "PORT", default_value = "5005")]
    pub metrics_port: u16,

    /// The chain this node is running.
    ///
    /// Possible values are either a built-in chain name or numeric chain ID.
    #[arg(
        long,
        value_name = "CHAIN",
        default_value = "mainnet",
        required = false
    )]
    pub chain: Chain,

    /// Run reth binary with sudo (for elevated privileges)
    #[arg(long)]
    pub sudo: bool,

    /// Generate comparison charts using Python script
    #[arg(long)]
    pub draw: bool,

    /// Enable CPU profiling with samply during benchmark runs
    #[arg(long)]
    pub profile: bool,

    /// Wait time between engine API calls (passed to reth-bench)
    #[arg(long, value_name = "DURATION")]
    pub wait_time: Option<String>,

    /// Number of blocks to run for cache warmup after clearing caches.
    /// If not specified, defaults to the same as --blocks
    #[arg(long, value_name = "N")]
    pub warmup_blocks: Option<u64>,

    /// Disable filesystem cache clearing before warmup phase.
    /// By default, filesystem caches are cleared before warmup to ensure consistent benchmarks.
    #[arg(long)]
    pub no_clear_cache: bool,

    #[command(flatten)]
    pub logs: LogArgs,

    /// Additional arguments to pass to baseline reth node command
    ///
    /// Example: `--baseline-args "--debug.tip 0xabc..."`
    #[arg(long, value_name = "ARGS")]
    pub baseline_args: Option<String>,

    /// Additional arguments to pass to feature reth node command
    ///
    /// Example: `--feature-args "--debug.tip 0xdef..."`
    #[arg(long, value_name = "ARGS")]
    pub feature_args: Option<String>,

    /// Additional arguments to pass to reth node command (applied to both baseline and feature)
    ///
    /// All arguments after `--` will be passed directly to the reth node command.
    /// Example: `reth-bench-compare --baseline-ref main --feature-ref pr/123 -- --debug.tip 0xabc...`
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub reth_args: Vec<String>,
}

impl Args {
    /// Initializes tracing with the configured options.
    pub fn init_tracing(&self) -> Result<Option<FileWorkerGuard>> {
        let guard = self.logs.init_tracing()?;
        Ok(guard)
    }

    /// Get the default RPC URL for a given chain
    fn get_default_rpc_url(chain: &Chain) -> &'static str {
        match chain.id() {
            1 => "https://reth-ethereum.ithaca.xyz/rpc", // mainnet
            8453 => "https://base-mainnet.rpc.ithaca.xyz", // base
            84532 => "https://base-sepolia.rpc.ithaca.xyz", // base-sepolia
            27082 => "https://rpc.hoodi.ethpandaops.io", // hoodi
            _ => "https://reth-ethereum.ithaca.xyz/rpc", // fallback to mainnet
        }
    }

    /// Get the RPC URL, using chain-specific default if not provided
    pub fn get_rpc_url(&self) -> String {
        self.rpc_url.clone().unwrap_or_else(|| {
            Self::get_default_rpc_url(&self.chain).to_string()
        })
    }

    /// Get the JWT secret path - either provided or derived from datadir
    pub fn jwt_secret_path(&self) -> PathBuf {
        match &self.jwt_secret {
            Some(path) => {
                let jwt_secret_str = path.to_string_lossy();
                let expanded = shellexpand::tilde(&jwt_secret_str);
                PathBuf::from(expanded.as_ref())
            }
            None => {
                // Use the same logic as reth: <datadir>/<chain>/jwt.hex
                let chain_path = self.datadir.clone().resolve_datadir(self.chain);
                chain_path.jwt()
            }
        }
    }

    /// Get the resolved datadir path using the chain
    pub fn datadir_path(&self) -> PathBuf {
        let chain_path = self.datadir.clone().resolve_datadir(self.chain);
        chain_path.data_dir().to_path_buf()
    }

    /// Get the expanded output directory path
    pub fn output_dir_path(&self) -> PathBuf {
        let expanded = shellexpand::tilde(&self.output_dir);
        PathBuf::from(expanded.as_ref())
    }

    /// Get the effective warmup blocks value - either specified or defaults to blocks
    pub fn get_warmup_blocks(&self) -> u64 {
        self.warmup_blocks.unwrap_or(self.blocks)
    }
}

/// Validate that the RPC endpoint chain ID matches the specified chain
async fn validate_rpc_chain_id(rpc_url: &str, expected_chain: &Chain) -> Result<()> {
    // Create Alloy provider
    let url = rpc_url
        .parse()
        .map_err(|e| eyre!("Invalid RPC URL '{}': {}", rpc_url, e))?;
    let provider = ProviderBuilder::new().connect_http(url);

    // Query chain ID using Alloy
    let rpc_chain_id = provider.get_chain_id().await.map_err(|e| {
        eyre!(
            "Failed to get chain ID from RPC endpoint {}: {:?}",
            rpc_url,
            e
        )
    })?;

    let expected_chain_id = expected_chain.id();

    if rpc_chain_id != expected_chain_id {
        return Err(eyre!(
            "RPC endpoint chain ID mismatch!\n\
            Expected: {} (chain: {})\n\
            Found: {} at RPC endpoint: {}\n\n\
            Please use an RPC endpoint for the correct network or change the --chain argument.",
            expected_chain_id,
            expected_chain,
            rpc_chain_id,
            rpc_url
        ));
    }

    info!("Validated RPC endpoint chain ID");
    Ok(())
}

/// Main comparison workflow execution
pub async fn run_comparison(args: Args, _ctx: CliContext) -> Result<()> {
    // Create a new process group for this process and all its children
    #[cfg(unix)]
    {
        use nix::unistd::{getpid, setpgid};
        if let Err(e) = setpgid(getpid(), getpid()) {
            warn!("Failed to create process group: {e}");
        }
    }

    info!(
        "Starting benchmark comparison between '{}' and '{}'",
        args.baseline_ref, args.feature_ref
    );

    if args.sudo {
        info!("Running in sudo mode - reth commands will use elevated privileges");
    }

    // Initialize Git manager
    let git_manager = GitManager::new()?;
    // Fetch all branches, tags, and commits
    git_manager.fetch_all()?;

    // Initialize compilation manager
    let output_dir = args.output_dir_path();
    let compilation_manager = CompilationManager::new(
        git_manager.repo_root().to_string(),
        output_dir.clone(),
        git_manager.clone(),
    )?;
    // Initialize node manager
    let mut node_manager = NodeManager::new(&args);

    let benchmark_runner = BenchmarkRunner::new(&args);
    let mut comparison_generator = ComparisonGenerator::new(&args);

    // Store original git state for restoration
    let original_ref = git_manager.get_current_ref()?;
    info!("Current git reference: {}", original_ref);

    // Validate git state
    if !args.skip_git_validation {
        git_manager.validate_clean_state()?;
        git_manager.validate_refs(&[&args.baseline_ref, &args.feature_ref])?;
    }

    // Validate RPC endpoint chain ID matches the specified chain
    let rpc_url = args.get_rpc_url();
    validate_rpc_chain_id(&rpc_url, &args.chain).await?;

    // Setup signal handling for cleanup
    let git_manager_cleanup = git_manager.clone();
    let original_ref_cleanup = original_ref.clone();
    ctrlc::set_handler(move || {
        eprintln!("Received interrupt signal, cleaning up...");

        // Send SIGTERM to entire process group to ensure all children exit
        #[cfg(unix)]
        {
            use nix::{
                sys::signal::{kill, Signal},
                unistd::Pid,
            };

            // Send SIGTERM to our process group (negative PID = process group)
            let current_pid = std::process::id() as i32;
            let pgid = Pid::from_raw(-current_pid);
            if let Err(e) = kill(pgid, Signal::SIGTERM) {
                eprintln!("Failed to send SIGTERM to process group: {e}");
            }
        }

        // Give a moment for any ongoing git operations to complete
        std::thread::sleep(std::time::Duration::from_millis(200));

        if let Err(e) = git_manager_cleanup.switch_ref(&original_ref_cleanup) {
            eprintln!("Failed to restore original git reference: {e}");
            eprintln!("You may need to manually run: git checkout {original_ref_cleanup}");
        }
        std::process::exit(1);
    })?;

    let result = run_benchmark_workflow(
        &git_manager,
        &compilation_manager,
        &mut node_manager,
        &benchmark_runner,
        &mut comparison_generator,
        &args,
    )
    .await;

    // Always restore original git reference
    info!("Restoring original git reference: {}", original_ref);
    git_manager.switch_ref(&original_ref)?;

    // Handle any errors from the workflow
    result?;

    Ok(())
}

/// Parse a string of arguments into a vector of strings
fn parse_args_string(args_str: &str) -> Vec<String> {
    shlex::split(args_str).unwrap_or_else(|| {
        // Fallback to simple whitespace splitting if shlex fails
        args_str.split_whitespace().map(|s| s.to_string()).collect()
    })
}

/// Run compilation phase for both baseline and feature binaries
async fn run_compilation_phase(
    git_manager: &GitManager,
    compilation_manager: &CompilationManager,
    args: &Args,
    is_optimism: bool,
) -> Result<(String, String)> {
    info!("=== Running compilation phase ===");
    
    // Ensure required tools are available (only need to check once)
    compilation_manager.ensure_reth_bench_available()?;
    if args.profile {
        compilation_manager.ensure_samply_available()?;
    }
    
    let refs = [&args.baseline_ref, &args.feature_ref];
    let ref_types = ["baseline", "feature"];
    
    // First, resolve all refs to commits using a HashMap to avoid race conditions where a ref is
    // pushed to mid-run.
    let mut ref_commits = std::collections::HashMap::new();
    for &git_ref in refs.iter() {
        if !ref_commits.contains_key(git_ref) {
            git_manager.switch_ref(git_ref)?;
            let commit = git_manager.get_current_commit()?;
            ref_commits.insert(git_ref.to_string(), commit);
            info!("Reference {} resolves to commit: {}", git_ref, &ref_commits[git_ref][..8]);
        }
    }
    
    // Now compile each ref using the resolved commits
    for (i, &git_ref) in refs.iter().enumerate() {
        let ref_type = ref_types[i];
        let commit = &ref_commits[git_ref];
        
        info!("Compiling {} binary for reference: {} (commit: {})", ref_type, git_ref, &commit[..8]);
        
        // Switch to target reference
        git_manager.switch_ref(git_ref)?;
        
        // Compile reth (with caching)
        compilation_manager.compile_reth(commit, is_optimism)?;
        
        info!("Completed compilation for {} reference", ref_type);
    }
    
    let baseline_commit = ref_commits[&args.baseline_ref].clone();
    let feature_commit = ref_commits[&args.feature_ref].clone();
    
    info!("Compilation phase completed");
    Ok((baseline_commit, feature_commit))
}

/// Run warmup phase to warm up caches before benchmarking
async fn run_warmup_phase(
    git_manager: &GitManager,
    compilation_manager: &CompilationManager,
    node_manager: &mut NodeManager,
    benchmark_runner: &BenchmarkRunner,
    args: &Args,
    is_optimism: bool,
    baseline_commit: &str,
) -> Result<()> {
    info!("=== Running warmup phase ===");
    
    // Use baseline for warmup
    let warmup_ref = &args.baseline_ref;
    
    // Switch to baseline reference
    git_manager.switch_ref(warmup_ref)?;
    
    // Get the cached binary path for baseline (should already be compiled)
    let binary_path = compilation_manager.get_cached_binary_path_for_commit(baseline_commit, is_optimism);
    
    // Verify the cached binary exists
    if !binary_path.exists() {
        return Err(eyre!(
            "Cached baseline binary not found at {:?}. Compilation phase should have created it.",
            binary_path
        ));
    }
    
    info!("Using cached baseline binary for warmup (commit: {})", &baseline_commit[..8]);
    
    // Get baseline additional arguments for warmup
    let additional_args = args.baseline_args.as_ref().map(|s| parse_args_string(s)).unwrap_or_default();
    
    // Start reth node for warmup
    let mut node_process = node_manager.start_node(&binary_path, warmup_ref, "warmup", &additional_args).await?;
    
    // Wait for node to be ready and get its current tip
    let current_tip = node_manager.wait_for_node_ready_and_get_tip().await?;
    info!("Warmup node is ready at tip: {}", current_tip);
    
    // Store the tip we'll unwind back to
    let original_tip = current_tip;

    // Clear filesystem caches before warmup run only (unless disabled)
    if !args.no_clear_cache {
        BenchmarkRunner::clear_fs_caches().await?;
    } else {
        info!("Skipping filesystem cache clearing (--no-clear-cache flag set)");
    }

    // Run warmup to warm up caches
    benchmark_runner
        .run_warmup(current_tip)
        .await?;
    
    // Stop node before unwinding (node must be stopped to release database lock)
    node_manager.stop_node(&mut node_process).await?;
    
    // Unwind back to starting block after warmup
    node_manager.unwind_to_block(original_tip).await?;
    
    info!("Warmup phase completed");
    Ok(())
}

/// Execute the complete benchmark workflow for both branches
async fn run_benchmark_workflow(
    git_manager: &GitManager,
    compilation_manager: &CompilationManager,
    node_manager: &mut NodeManager,
    benchmark_runner: &BenchmarkRunner,
    comparison_generator: &mut ComparisonGenerator,
    args: &Args,
) -> Result<()> {
    // Detect if this is an Optimism chain once at the beginning
    let rpc_url = args.get_rpc_url();
    let is_optimism = compilation_manager.detect_optimism_chain(&rpc_url).await?;
    
    // Run compilation phase for both binaries
    let (baseline_commit, feature_commit) = run_compilation_phase(git_manager, compilation_manager, args, is_optimism).await?;
    
    // Run warmup phase before benchmarking
    run_warmup_phase(git_manager, compilation_manager, node_manager, benchmark_runner, args, is_optimism, &baseline_commit).await?;
    
    let refs = [&args.baseline_ref, &args.feature_ref];
    let ref_types = ["baseline", "feature"];
    let commits = [&baseline_commit, &feature_commit];

    for (i, &git_ref) in refs.iter().enumerate() {
        let ref_type = ref_types[i];
        let commit = commits[i];
        info!("=== Processing {} reference: {} ===", ref_type, git_ref);

        // Switch to target reference
        git_manager.switch_ref(git_ref)?;

        // Get the cached binary path for this git reference (should already be compiled)
        let binary_path = compilation_manager.get_cached_binary_path_for_commit(commit, is_optimism);
        
        // Verify the cached binary exists
        if !binary_path.exists() {
            return Err(eyre!(
                "Cached {} binary not found at {:?}. Compilation phase should have created it.",
                ref_type, binary_path
            ));
        }
        
        info!("Using cached {} binary (commit: {})", ref_type, &commit[..8]);

        // Get reference-specific additional arguments
        let additional_args = match ref_type {
            "baseline" => args.baseline_args.as_ref().map(|s| parse_args_string(s)).unwrap_or_default(),
            "feature" => args.feature_args.as_ref().map(|s| parse_args_string(s)).unwrap_or_default(),
            _ => Vec::new(),
        };

        // Start reth node
        let mut node_process = node_manager.start_node(&binary_path, git_ref, ref_type, &additional_args).await?;

        // Wait for node to be ready and get its current tip (wherever it is)
        let current_tip = node_manager.wait_for_node_ready_and_get_tip().await?;
        info!("Node is ready at tip: {}", current_tip);

        // Store the tip we'll unwind back to
        let original_tip = current_tip;

        // Calculate benchmark range
        // Note: reth-bench has an off-by-one error where it consumes the first block
        // of the range, so we add 1 to compensate and get exactly args.blocks blocks
        let from_block = original_tip;
        let to_block = original_tip + args.blocks;

        // Run benchmark
        let output_dir = comparison_generator.get_ref_output_dir(ref_type);

        // Capture start timestamp for the benchmark run
        let benchmark_start = chrono::Utc::now();

        // Run benchmark (comparison logic is handled separately by ComparisonGenerator)
        benchmark_runner
            .run_benchmark(from_block, to_block, &output_dir)
            .await?;

        // Capture end timestamp for the benchmark run
        let benchmark_end = chrono::Utc::now();

        // Stop node
        node_manager.stop_node(&mut node_process).await?;

        // Unwind back to original tip
        node_manager.unwind_to_block(original_tip).await?;

        // Store results for comparison
        comparison_generator.add_ref_results(ref_type, &output_dir)?;

        // Set the benchmark run timestamps
        comparison_generator.set_ref_timestamps(ref_type, benchmark_start, benchmark_end)?;

        info!("Completed {} reference benchmark", ref_type);
    }

    // Generate comparison report
    comparison_generator.generate_comparison_report().await?;

    // Generate charts if requested
    if args.draw {
        generate_comparison_charts(comparison_generator, args).await?;
    }

    // Start samply servers if profiling was enabled
    if args.profile {
        start_samply_servers(args).await?;
    }

    Ok(())
}

/// Generate comparison charts using the Python script
async fn generate_comparison_charts(
    comparison_generator: &ComparisonGenerator,
    _args: &Args,
) -> Result<()> {
    info!("Generating comparison charts with Python script...");

    let baseline_output_dir = comparison_generator.get_ref_output_dir("baseline");
    let feature_output_dir = comparison_generator.get_ref_output_dir("feature");

    let baseline_csv = baseline_output_dir.join("combined_latency.csv");
    let feature_csv = feature_output_dir.join("combined_latency.csv");

    // Check if CSV files exist
    if !baseline_csv.exists() {
        return Err(eyre!("Baseline CSV not found: {:?}", baseline_csv));
    }
    if !feature_csv.exists() {
        return Err(eyre!("Feature CSV not found: {:?}", feature_csv));
    }

    let output_dir = comparison_generator.get_output_dir();
    let chart_output = output_dir.join("latency_comparison.png");

    let script_path = "bin/reth-bench/scripts/compare_newpayload_latency.py";

    info!("Running Python comparison script with uv...");
    let mut cmd = Command::new("uv");
    cmd.args([
        "run",
        script_path,
        &baseline_csv.to_string_lossy(),
        &feature_csv.to_string_lossy(),
        "-o",
        &chart_output.to_string_lossy(),
    ]);

    // Set process group for consistent signal handling
    #[cfg(unix)]
    {
        cmd.process_group(0);
    }

    let output = cmd.output().await.map_err(|e| {
        eyre!(
            "Failed to execute Python script with uv: {}. Make sure uv is installed.",
            e
        )
    })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        return Err(eyre!(
            "Python script failed with exit code {:?}:\nstdout: {}\nstderr: {}",
            output.status.code(),
            stdout,
            stderr
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    if !stdout.trim().is_empty() {
        info!("Python script output:\n{}", stdout);
    }

    info!("Comparison chart generated: {:?}", chart_output);
    Ok(())
}

/// Start samply servers for viewing profiles
async fn start_samply_servers(args: &Args) -> Result<()> {
    info!("Starting samply servers for profile viewing...");

    let output_dir = args.output_dir_path();
    let profiles_dir = output_dir.join("profiles");

    // Build profile paths
    let baseline_profile = profiles_dir.join("baseline.json.gz");
    let feature_profile = profiles_dir.join("feature.json.gz");

    // Check if profiles exist
    if !baseline_profile.exists() {
        warn!("Baseline profile not found: {:?}", baseline_profile);
        return Ok(());
    }
    if !feature_profile.exists() {
        warn!("Feature profile not found: {:?}", feature_profile);
        return Ok(());
    }

    // Find two consecutive available ports starting from 3000
    let (baseline_port, feature_port) = find_consecutive_ports(3000)?;
    info!(
        "Found available ports: {} and {}",
        baseline_port, feature_port
    );

    // Get samply path
    let samply_path = get_samply_path().await?;

    // Start baseline server
    info!(
        "Starting samply server for baseline '{}' on port {}",
        args.baseline_ref, baseline_port
    );
    let mut baseline_cmd = Command::new(&samply_path);
    baseline_cmd
        .args([
            "load",
            "--port",
            &baseline_port.to_string(),
            &baseline_profile.to_string_lossy(),
        ])
        .kill_on_drop(true);

    // Set process group for consistent signal handling
    #[cfg(unix)]
    {
        baseline_cmd.process_group(0);
    }

    // Conditionally pipe output based on log level
    if tracing::enabled!(tracing::Level::DEBUG) {
        baseline_cmd
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
    } else {
        baseline_cmd
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
    }

    // Debug log the command
    debug!("Executing samply load command: {:?}", baseline_cmd);

    let mut baseline_child = baseline_cmd
        .spawn()
        .wrap_err("Failed to start samply server for baseline")?;

    // Stream baseline samply output if debug logging is enabled
    if tracing::enabled!(tracing::Level::DEBUG) {
        if let Some(stdout) = baseline_child.stdout.take() {
            tokio::spawn(async move {
                use tokio::io::{AsyncBufReadExt, BufReader};
                let reader = BufReader::new(stdout);
                let mut lines = reader.lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    debug!("[SAMPLY-BASELINE] {}", line);
                }
            });
        }

        if let Some(stderr) = baseline_child.stderr.take() {
            tokio::spawn(async move {
                use tokio::io::{AsyncBufReadExt, BufReader};
                let reader = BufReader::new(stderr);
                let mut lines = reader.lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    debug!("[SAMPLY-BASELINE] {}", line);
                }
            });
        }
    }

    // Start feature server
    info!(
        "Starting samply server for feature '{}' on port {}",
        args.feature_ref, feature_port
    );
    let mut feature_cmd = Command::new(&samply_path);
    feature_cmd
        .args([
            "load",
            "--port",
            &feature_port.to_string(),
            &feature_profile.to_string_lossy(),
        ])
        .kill_on_drop(true);

    // Set process group for consistent signal handling
    #[cfg(unix)]
    {
        feature_cmd.process_group(0);
    }

    // Conditionally pipe output based on log level
    if tracing::enabled!(tracing::Level::DEBUG) {
        feature_cmd
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
    } else {
        feature_cmd
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
    }

    // Debug log the command
    debug!("Executing samply load command: {:?}", feature_cmd);

    let mut feature_child = feature_cmd
        .spawn()
        .wrap_err("Failed to start samply server for feature")?;

    // Stream feature samply output if debug logging is enabled
    if tracing::enabled!(tracing::Level::DEBUG) {
        if let Some(stdout) = feature_child.stdout.take() {
            tokio::spawn(async move {
                use tokio::io::{AsyncBufReadExt, BufReader};
                let reader = BufReader::new(stdout);
                let mut lines = reader.lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    debug!("[SAMPLY-FEATURE] {}", line);
                }
            });
        }

        if let Some(stderr) = feature_child.stderr.take() {
            tokio::spawn(async move {
                use tokio::io::{AsyncBufReadExt, BufReader};
                let reader = BufReader::new(stderr);
                let mut lines = reader.lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    debug!("[SAMPLY-FEATURE] {}", line);
                }
            });
        }
    }

    // Give servers time to start
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    // Print access information
    println!("\n=== SAMPLY PROFILE SERVERS STARTED ===");
    println!(
        "Baseline '{}': http://127.0.0.1:{}",
        args.baseline_ref, baseline_port
    );
    println!(
        "Feature  '{}': http://127.0.0.1:{}",
        args.feature_ref, feature_port
    );
    println!("\nOpen the URLs in your browser to view the profiles.");
    println!("Press Ctrl+C to stop the servers and exit.");
    println!("=========================================\n");

    // Wait for Ctrl+C or process termination
    let ctrl_c = tokio::signal::ctrl_c();
    let baseline_wait = baseline_child.wait();
    let feature_wait = feature_child.wait();

    tokio::select! {
        _ = ctrl_c => {
            info!("Received Ctrl+C, shutting down samply servers...");
        }
        result = baseline_wait => {
            match result {
                Ok(status) => info!("Baseline samply server exited with status: {}", status),
                Err(e) => warn!("Baseline samply server error: {}", e),
            }
        }
        result = feature_wait => {
            match result {
                Ok(status) => info!("Feature samply server exited with status: {}", status),
                Err(e) => warn!("Feature samply server error: {}", e),
            }
        }
    }

    // Ensure both processes are terminated
    let _ = baseline_child.kill().await;
    let _ = feature_child.kill().await;

    info!("Samply servers stopped.");
    Ok(())
}

/// Find two consecutive available ports starting from the given port
fn find_consecutive_ports(start_port: u16) -> Result<(u16, u16)> {
    for port in start_port..=65533 {
        // Check if both port and port+1 are available
        if is_port_available(port) && is_port_available(port + 1) {
            return Ok((port, port + 1));
        }
    }
    Err(eyre!(
        "Could not find two consecutive available ports starting from {}",
        start_port
    ))
}

/// Check if a port is available by attempting to bind to it
fn is_port_available(port: u16) -> bool {
    TcpListener::bind(("127.0.0.1", port)).is_ok()
}

/// Get the absolute path to samply using 'which' command
async fn get_samply_path() -> Result<String> {
    let output = Command::new("which")
        .arg("samply")
        .output()
        .await
        .wrap_err("Failed to execute 'which samply' command")?;

    if !output.status.success() {
        return Err(eyre!("samply not found in PATH"));
    }

    let samply_path = String::from_utf8(output.stdout)
        .wrap_err("samply path is not valid UTF-8")?
        .trim()
        .to_string();

    if samply_path.is_empty() {
        return Err(eyre!("which samply returned empty path"));
    }

    Ok(samply_path)
}
