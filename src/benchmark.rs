//! Benchmark execution using reth-bench.

use crate::cli::Args;
use eyre::{eyre, Result, WrapErr};
use std::{
    path::Path,
    sync::{Arc, Mutex},
};
use tokio::{
    fs::File as AsyncFile,
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::Command,
};
use tracing::{debug, error, info, warn};

/// Manages benchmark execution using reth-bench
pub struct BenchmarkRunner {
    rpc_url: String,
    jwt_secret: String,
    wait_time: Option<String>,
    warmup_blocks: u64,
}

impl BenchmarkRunner {
    /// Create a new BenchmarkRunner from CLI arguments
    pub fn new(args: &Args) -> Self {
        Self {
            rpc_url: args.get_rpc_url(),
            jwt_secret: args.jwt_secret_path().to_string_lossy().to_string(),
            wait_time: args.wait_time.clone(),
            warmup_blocks: args.get_warmup_blocks(),
        }
    }

    /// Clear filesystem caches (page cache, dentries, and inodes)
    pub async fn clear_fs_caches() -> Result<()> {
        info!("Clearing filesystem caches...");
        
        // First sync to ensure all pending writes are flushed
        let sync_output = Command::new("sync")
            .output()
            .await
            .wrap_err("Failed to execute sync command")?;
            
        if !sync_output.status.success() {
            return Err(eyre!("sync command failed"));
        }
        
        // Drop caches - requires sudo/root permissions
        // 3 = drop pagecache, dentries, and inodes
        let drop_caches_cmd = Command::new("sudo")
            .args(["-n", "sh", "-c", "echo 3 > /proc/sys/vm/drop_caches"])
            .output()
            .await;
            
        match drop_caches_cmd {
            Ok(output) if output.status.success() => {
                info!("Successfully cleared filesystem caches");
                Ok(())
            }
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                if stderr.contains("sudo: a password is required") {
                    warn!("Unable to clear filesystem caches: sudo password required");
                    warn!("For optimal benchmarking, configure passwordless sudo for cache clearing:");
                    warn!("  echo '$USER ALL=(ALL) NOPASSWD: /bin/sh -c echo\\\\ [0-9]\\\\ \\\\>\\\\ /proc/sys/vm/drop_caches' | sudo tee /etc/sudoers.d/drop_caches");
                    Ok(())
                } else {
                    Err(eyre!("Failed to clear filesystem caches: {}", stderr))
                }
            }
            Err(e) => {
                warn!("Unable to clear filesystem caches: {}", e);
                Ok(())
            }
        }
    }

    /// Run a warmup benchmark for cache warming
    pub async fn run_warmup(&self, from_block: u64) -> Result<()> {
        let to_block = from_block + self.warmup_blocks;
        info!(
            "Running warmup benchmark from block {} to {} ({} blocks)",
            from_block, to_block, self.warmup_blocks
        );

        // Build the reth-bench command for warmup (no output flag)
        let mut cmd = Command::new("reth-bench");
        cmd.args([
            "new-payload-fcu",
            "--rpc-url",
            &self.rpc_url,
            "--jwt-secret",
            &self.jwt_secret,
            "--from",
            &from_block.to_string(),
            "--to",
            &to_block.to_string(),
        ]);

        // Add wait-time argument if provided
        if let Some(ref wait_time) = self.wait_time {
            cmd.args(["--wait-time", wait_time]);
        }

        cmd.stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);

        // Set process group for consistent signal handling
        #[cfg(unix)]
        {
            cmd.process_group(0);
        }

        debug!("Executing warmup reth-bench command: {:?}", cmd);

        // Execute the warmup benchmark
        let mut child = cmd.spawn().wrap_err("Failed to start warmup reth-bench process")?;

        // Stream output at debug level
        if let Some(stdout) = child.stdout.take() {
            tokio::spawn(async move {
                let reader = BufReader::new(stdout);
                let mut lines = reader.lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    debug!("[WARMUP] {}", line);
                }
            });
        }

        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let reader = BufReader::new(stderr);
                let mut lines = reader.lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    debug!("[WARMUP] {}", line);
                }
            });
        }

        let status = child
            .wait()
            .await
            .wrap_err("Failed to wait for warmup reth-bench")?;

        if !status.success() {
            return Err(eyre!(
                "Warmup reth-bench failed with exit code: {:?}",
                status.code()
            ));
        }

        info!("Warmup completed successfully");
        Ok(())
    }

    /// Run a benchmark for the specified block range
    pub async fn run_benchmark(
        &self,
        from_block: u64,
        to_block: u64,
        output_dir: &Path,
    ) -> Result<()> {
        info!(
            "Running benchmark from block {} to {} (output: {:?})",
            from_block, to_block, output_dir
        );

        // Ensure output directory exists
        std::fs::create_dir_all(output_dir)
            .wrap_err_with(|| format!("Failed to create output directory: {output_dir:?}"))?;

        // Create log file path for reth-bench output
        let log_file_path = output_dir.join("reth_bench.log");
        info!("reth-bench logs will be saved to: {:?}", log_file_path);

        // Build the reth-bench command
        let mut cmd = Command::new("reth-bench");
        cmd.args([
            "new-payload-fcu",
            "--rpc-url",
            &self.rpc_url,
            "--jwt-secret",
            &self.jwt_secret,
            "--from",
            &from_block.to_string(),
            "--to",
            &to_block.to_string(),
            "--output",
            &output_dir.to_string_lossy(),
        ]);

        // Add wait-time argument if provided
        if let Some(ref wait_time) = self.wait_time {
            cmd.args(["--wait-time", wait_time]);
        }

        cmd.stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);

        // Set process group for consistent signal handling
        #[cfg(unix)]
        {
            cmd.process_group(0);
        }

        // Debug log the command
        debug!("Executing reth-bench command: {:?}", cmd);

        // Execute the benchmark
        let mut child = cmd.spawn().wrap_err("Failed to start reth-bench process")?;

        // Capture stdout and stderr for error reporting
        let stdout_lines = Arc::new(Mutex::new(Vec::new()));
        let stderr_lines = Arc::new(Mutex::new(Vec::new()));

        // Stream stdout with prefix at debug level, capture for error reporting, and write to log file
        if let Some(stdout) = child.stdout.take() {
            let stdout_lines_clone = stdout_lines.clone();
            let log_file = AsyncFile::create(&log_file_path).await
                .wrap_err(format!("Failed to create log file: {:?}", log_file_path))?;
            tokio::spawn(async move {
                let reader = BufReader::new(stdout);
                let mut lines = reader.lines();
                let mut log_file = log_file;
                while let Ok(Some(line)) = lines.next_line().await {
                    debug!("[RETH-BENCH] {}", line);
                    if let Ok(mut captured) = stdout_lines_clone.lock() {
                        captured.push(line.clone());
                    }
                    // Write to log file (reth-bench output already has timestamps if needed)
                    let log_line = format!("{}\n", line);
                    if let Err(e) = log_file.write_all(log_line.as_bytes()).await {
                        debug!("Failed to write to log file: {}", e);
                    }
                }
            });
        }

        // Stream stderr with prefix at debug level, capture for error reporting, and write to log file
        if let Some(stderr) = child.stderr.take() {
            let stderr_lines_clone = stderr_lines.clone();
            let log_file = AsyncFile::options()
                .create(true)
                .append(true)
                .open(&log_file_path)
                .await
                .wrap_err(format!("Failed to open log file for stderr: {:?}", log_file_path))?;
            tokio::spawn(async move {
                let reader = BufReader::new(stderr);
                let mut lines = reader.lines();
                let mut log_file = log_file;
                while let Ok(Some(line)) = lines.next_line().await {
                    debug!("[RETH-BENCH] {}", line);
                    if let Ok(mut captured) = stderr_lines_clone.lock() {
                        captured.push(line.clone());
                    }
                    // Write to log file (reth-bench output already has timestamps if needed)
                    let log_line = format!("{}\n", line);
                    if let Err(e) = log_file.write_all(log_line.as_bytes()).await {
                        debug!("Failed to write to log file: {}", e);
                    }
                }
            });
        }

        let status = child
            .wait()
            .await
            .wrap_err("Failed to wait for reth-bench")?;

        if !status.success() {
            // Print all captured output when command fails
            error!("reth-bench failed with exit code: {:?}", status.code());

            if let Ok(stdout) = stdout_lines.lock() {
                if !stdout.is_empty() {
                    error!("reth-bench stdout:");
                    for line in stdout.iter() {
                        error!("  {}", line);
                    }
                }
            }

            if let Ok(stderr) = stderr_lines.lock() {
                if !stderr.is_empty() {
                    error!("reth-bench stderr:");
                    for line in stderr.iter() {
                        error!("  {}", line);
                    }
                }
            }

            return Err(eyre!(
                "reth-bench failed with exit code: {:?}",
                status.code()
            ));
        }

        info!("Benchmark completed");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::Args;
    use clap::Parser;

    #[test]
    fn test_wait_time_argument_passed() {
        // Test with wait-time provided
        let args = Args::try_parse_from([
            "reth-bench-compare",
            "--baseline-ref",
            "main",
            "--feature-ref",
            "test",
            "--wait-time",
            "200ms",
        ])
        .unwrap();

        let runner = BenchmarkRunner::new(&args);
        assert_eq!(runner.wait_time, Some("200ms".to_string()));

        // Test without wait-time
        let args = Args::try_parse_from([
            "reth-bench-compare",
            "--baseline-ref",
            "main",
            "--feature-ref",
            "test",
        ])
        .unwrap();

        let runner = BenchmarkRunner::new(&args);
        assert_eq!(runner.wait_time, None);
    }

    #[test]
    fn test_warmup_blocks_defaults_to_blocks() {
        // Test that warmup_blocks defaults to blocks value when not specified
        let args = Args::try_parse_from([
            "reth-bench-compare",
            "--baseline-ref",
            "main",
            "--feature-ref",
            "test",
            "--blocks",
            "50",
        ])
        .unwrap();

        let runner = BenchmarkRunner::new(&args);
        assert_eq!(runner.warmup_blocks, 50);

        // Test that explicit warmup_blocks overrides the default
        let args = Args::try_parse_from([
            "reth-bench-compare",
            "--baseline-ref",
            "main",
            "--feature-ref",
            "test",
            "--blocks",
            "50",
            "--warmup-blocks",
            "25",
        ])
        .unwrap();

        let runner = BenchmarkRunner::new(&args);
        assert_eq!(runner.warmup_blocks, 25);
    }

    #[test]
    fn test_default_rpc_urls() {
        // Test mainnet default
        let args = Args::try_parse_from([
            "reth-bench-compare",
            "--baseline-ref",
            "main",
            "--feature-ref",
            "test",
            "--chain",
            "mainnet",
        ])
        .unwrap();
        let runner = BenchmarkRunner::new(&args);
        assert_eq!(runner.rpc_url, "https://reth-ethereum.ithaca.xyz/rpc");

        // Test base default
        let args = Args::try_parse_from([
            "reth-bench-compare",
            "--baseline-ref",
            "main",
            "--feature-ref",
            "test",
            "--chain",
            "base",
        ])
        .unwrap();
        let runner = BenchmarkRunner::new(&args);
        assert_eq!(runner.rpc_url, "https://base-mainnet.rpc.ithaca.xyz");

        // Test base-sepolia default
        let args = Args::try_parse_from([
            "reth-bench-compare",
            "--baseline-ref",
            "main",
            "--feature-ref",
            "test",
            "--chain",
            "base-sepolia",
        ])
        .unwrap();
        let runner = BenchmarkRunner::new(&args);
        assert_eq!(runner.rpc_url, "https://base-sepolia.rpc.ithaca.xyz");

        // Test custom RPC URL overrides default
        let args = Args::try_parse_from([
            "reth-bench-compare",
            "--baseline-ref",
            "main",
            "--feature-ref",
            "test",
            "--chain",
            "base",
            "--rpc-url",
            "https://custom-rpc.example.com",
        ])
        .unwrap();
        let runner = BenchmarkRunner::new(&args);
        assert_eq!(runner.rpc_url, "https://custom-rpc.example.com");
    }
}
