# Session history upload process benchmarks

The ignored `benchmark::uploader_benchmark` test measures preparation, resume, encoding, canonical bytes, and spool bytes in the upload process. It runs the same binary through a baseline-style full capture and an unchanged-source resume, so the result includes the work that matters to capture latency.

Build and run it from a clean checkout with the release profile:

```sh
UPLOADER_BENCH_INPUT=/path/to/corpus.jsonl \
  cargo test -p maestro-session-history --release \
  benchmark::uploader_benchmark -- --ignored --nocapture
```

If `UPLOADER_BENCH_INPUT` is omitted, the benchmark creates a deterministic 500-entry tool-result fixture. A corpus supplied by a developer is read only; benchmark output contains aggregate sizes and timings plus canonical segment hashes, never transcript contents. Use three runs per version and keep the compiler, profile, and machine constant when comparing results.

The optimized path is expected to preserve the canonical segment hashes from the baseline. The benchmark is deliberately not an SLA: it is a repeatable guard against parse/redaction/serialization regressions. Network latency, server throughput, and allocator RSS need separate production measurements.

The compression matrix is the ignored `compression::compression_matrix` test. It requires `UPLOADER_BENCH_INPUT` to name a transcript corpus; it has no
default fixture. It splits canonical segments at the production limit, and compares `Zstandard` levels 1/3/6 with `gzip` levels 1/6. Level 1 is selected by the upload process because it gave the best size/time tradeoff on the varied tool-result fixture; the threshold and result are workload-sensitive.
