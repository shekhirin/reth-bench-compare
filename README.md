# reth-bench-compare

Automated performance comparison tool for reth across different git references.

## Overview

This tool automates the process of comparing reth performance between two git references (branches, tags, or commits) by:

1. **Building reth** from each reference with optimized settings
2. **Running identical benchmarks** on the same block ranges  
3. **Generating detailed comparison reports** with metrics and visualizations

## Features

- ✅ **Automated Workflow**: Handles git operations, compilation, and benchmarking
- ✅ **Binary Caching**: Reuses compiled binaries across runs to save time
- ✅ **Git Safety**: Restores original git state on completion or interruption
- ✅ **CPU Profiling**: Optional integration with samply profiler
- ✅ **Visual Reports**: Generates charts and CSV data for analysis
- ✅ **Process Management**: Ensures all child processes exit cleanly
- ✅ **Sync Validation**: Waits for node to be ready and not syncing
- ✅ **Chain Support**: Works with mainnet, sepolia, holesky, etc.

## Installation

### Prerequisites

- Rust toolchain (1.86+)
- Git
- Make
- Python 3 + uv (for chart generation, optional)
- samply (for CPU profiling, optional - auto-installed if needed)

### Build from Source

```bash
git clone <this-repository>
cd reth-bench-compare
cargo build --release
```

## Usage

### Basic Usage

```bash
reth-bench-compare \
  --baseline-ref main \
  --feature-ref feature-branch \
  --blocks 100 \
  --datadir /path/to/reth/datadir
```

### Advanced Options

```bash
reth-bench-compare \
  --baseline-ref v1.0.0 \
  --feature-ref my-optimization \
  --blocks 500 \
  --chain sepolia \
  --datadir ~/.local/share/reth/sepolia \
  --rpc-url https://rpc.sepolia.org \
  --output-dir ./my-benchmark \
  --profile \
  --draw \
  --wait-time 100ms \
  -vvv
```

### Key Options

- `--baseline-ref` / `--feature-ref`: Git references to compare (branch/tag/commit)
- `--blocks`: Number of blocks to benchmark (default: 100)
- `--chain`: Blockchain network (mainnet, sepolia, holesky, etc.)
- `--datadir`: Path to reth data directory
- `--rpc-url`: External RPC endpoint for fetching blocks
- `--profile`: Enable CPU profiling with samply
- `--draw`: Generate comparison charts
- `--wait-time`: Delay between engine API calls (passed to reth-bench)
- `--sudo`: Run reth with elevated privileges
- `-v/-vv/-vvv`: Increase verbosity (debug output with -vvv)

### Passing Additional Arguments to Reth

You can pass additional arguments to the `reth node` command by using `--` followed by the arguments:

```bash
reth-bench-compare \
  --baseline-ref main \
  --feature-ref feature-branch \
  --blocks 100 \
  -- \
  --debug.tip 0xabc123... \
  --engine.legacy
```

All arguments after `--` will be passed directly to both reth node instances.

## Output Structure

```
reth-bench-compare/
├── bin/                           # Cached compiled binaries
│   ├── reth_main                  # Baseline binary
│   └── reth_feature-branch        # Feature binary
├── profiles/                      # CPU profiling data (if --profile)
│   ├── main.json.gz              # Baseline profile
│   └── feature-branch.json.gz    # Feature profile  
└── results/                       # Benchmark results
    └── 2024-01-15_14-30-00/      # Timestamped results
        ├── baseline/              # Baseline reference data
        │   └── combined_latency.csv
        ├── feature/               # Feature reference data  
        │   └── combined_latency.csv
        ├── comparison_report.json # Detailed comparison
        └── latency_comparison.png # Visual chart (if --draw)
```

## Performance Metrics

The tool measures and compares:

- **NewPayload latency**: Time to process new payload (the primary metric for block processing performance)
- **Gas processed per second**: Throughput metric
- **Blocks processed per second**: Block processing rate

Note: ForkchoiceUpdated (FCU) latency is not reported as it's consistently ~1-2ms and not affected by block processing optimizations.

## How It Works

### For Each Reference:

1. **Git Operations**: Switch to target reference and pull latest changes
2. **Compilation**: Build reth with profiling optimizations (cached by commit hash)
3. **Node Startup**: Launch reth node and wait for readiness + sync completion
4. **Benchmarking**: Run reth-bench on identical block ranges
5. **Cleanup**: Stop node gracefully and unwind to original state
6. **Analysis**: Generate comparison reports and optional visualizations

### Safety Features

- **Git State Restoration**: Returns to original branch/tag/commit on exit
- **Process Group Management**: Ensures all child processes exit cleanly  
- **Graceful Shutdown**: Handles Ctrl+C and SIGTERM signals properly
- **Validation**: Checks for uncommitted changes (allows untracked files)
- **Error Recovery**: Handles process failures and provides clear guidance

## Prerequisites

- **Clean Git State**: No uncommitted changes to tracked files (untracked files OK)
- **Valid References**: Git references must exist as branches, tags, or commits
- **Chain Access**: RPC endpoint must match the specified chain
- **Storage Space**: Sufficient disk space for multiple reth builds and data

## CPU Profiling

Enable CPU profiling with `--profile`:

```bash
reth-bench-compare --baseline-ref main --feature-ref opt --profile
```

This will:
- Install samply automatically if not found
- Generate .json.gz profile files for each reference
- Start local servers to view profiles in Firefox Profiler
- Display URLs for browser access

## Chart Generation

Generate visual comparisons with `--draw`:

```bash
reth-bench-compare --baseline-ref main --feature-ref opt --draw
```

Requires Python 3 + uv. The script will:
- Install required packages (pandas, matplotlib, numpy)
- Generate latency_comparison.png chart
- Show performance differences visually

## Troubleshooting

### Common Issues

**Git Validation Fails**:
- Commit or stash uncommitted changes to tracked files
- Untracked files are allowed and don't need to be cleaned

**Compilation Errors**:
- Ensure you have the latest Rust toolchain
- Check that both git references build successfully

**Node Startup Fails**:
- Verify RPC endpoint is accessible and matches chain
- Check JWT secret file permissions
- Ensure sufficient disk space

**Benchmark Errors**:
- Make sure reth-bench is available in PATH
- Verify node is fully synced before benchmarking

### Debug Output

Use `-vvv` for detailed logs:

```bash
reth-bench-compare --baseline-ref main --feature-ref opt -vvv
```

This shows:
- All command executions with Debug output
- Git operations and status
- Node startup and sync progress  
- Samply server output (if profiling)
- Detailed error information

### Recovery

If the tool is interrupted:
- Git state is automatically restored
- Child processes are terminated cleanly
- You can manually run: `git checkout <original-ref>`

## Examples

### Quick Performance Test
```bash
reth-bench-compare --baseline-ref main --feature-ref my-fix --blocks 50
```

### Comprehensive Analysis
```bash
reth-bench-compare \
  --baseline-ref v1.0.0 \
  --feature-ref experimental \
  --blocks 1000 \
  --profile \
  --draw \
  --chain sepolia \
  --output-dir ./performance-analysis \
  -vv
```

### Tag Comparison
```bash
reth-bench-compare \
  --baseline-ref v1.4.0 \
  --feature-ref v1.5.0 \
  --blocks 200 \
  --draw
```

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.