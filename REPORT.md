# range-replay v0.1 measurement report

- Date: 2026-08-19
- Scope: one machine-specific, warm-page-cache comparison
- Status: accepted terminal `v0.1` report
- Source commit: [`29f862bbfef6a0996c8027919650534c8f4bdba7`](https://github.com/robinber/range-replay/commit/29f862bbfef6a0996c8027919650534c8f4bdba7)

## Executive conclusion

On this two-vCPU KVM guest, for this purpose-built warm-cache runner:

- `io_uring` at submitted depth 1 completed every fixed comparison workload
  faster than the synchronous `pread` reference. Median useful-throughput
  gains ranged from 1.18x to 1.88x.
- Increasing `io_uring` submitted depth beyond 1 did not produce a consistent
  benefit. Depth 4 or 16 was marginally best only for two mostly sequential
  workloads; depth 1 was best for the other four. At 1 MiB, depth 16 fell back
  to approximately `pread` throughput.
- Read granularity and physical-operation count mattered more than deeper
  submission. Moving from separate 4 KiB reads to fixed groups of 16 reduced
  operation count by 16x and improved useful throughput by 10.58x for `pread`
  and 6.35x for `io_uring` depth 1, despite reading 240 MiB of extra data.
- Once reads were grouped, `pread` and `io_uring` depth 1 were effectively tied
  on useful throughput: 210.34 versus 205.83 MiB/s, a 2.2% difference in favor
  of `pread` in this run.
- Process CPU time tracked wall time closely, the complete commands reported
  99% CPU use, no major page faults, and zero filesystem input blocks. These
  are CPU- and warm-cache-path observations, not a cold-NVMe benchmark.

The practical lesson for tensor-loading-like work is bounded: avoid issuing a
large number of tiny reads when a known layout permits safe grouping, but count
the resulting read amplification explicitly. Use `io_uring` when its execution
model or measured depth-1 path helps; do not assume that deeper queues help a
warm, CPU-bound workload. A simple `pread` path remains competitive once each
physical operation carries enough useful work.

## Questions and scope

The experiment answers five terminal `v0.1` questions:

1. Do the Linux `pread` and `io_uring` backends return identical logical bytes?
2. How do 4 KiB, 64 KiB, and 1 MiB ranges behave under one fixed 256 MiB
   logical payload?
3. Does `io_uring` submitted depth 4 or 16 improve over the shared depth-1
   baseline?
4. How does a deterministic sparse pattern compare with the mostly sequential
   pattern under the same logical and physical byte counts?
5. When does replacing separate 4 KiB reads with fewer 124 KiB physical reads
   pay for 240 MiB of over-read?

This report does not test cold-cache behavior, `O_DIRECT`, a physical NVMe
device outside the guest abstraction, multiple machines, GPU execution, model
inference, adaptive coalescing, or an auto-tuner.

## Provenance

### Source and binaries

| Item | Value |
| --- | --- |
| Git commit | `29f862bbfef6a0996c8027919650534c8f4bdba7` |
| Git state during both runs | clean `main`, equal to `origin/main` |
| Runner | `range-replay-measure` release build with `--locked` |
| Runner SHA-256 | `2a9277205e5e46440ce32a4a3e73ac1ffed20ca1639f382812d81c2745a28a55` |
| Rust | `rustc 1.97.0 (2d8144b78 2026-07-07)` |
| Cargo | `cargo 1.97.0 (c980f4866 2026-06-30)` |
| Target | `x86_64-unknown-linux-gnu` |
| LLVM | `22.1.6` |

### Machine

| Item | Value |
| --- | --- |
| Host | `srv1110288` |
| OS | Ubuntu 24.04.4 LTS |
| Kernel | `6.8.0-90-generic` |
| Virtualization | KVM |
| CPU | 2 vCPU, AMD EPYC 7543P 32-Core Processor |
| Memory | 7.8 GiB RAM, 8.0 GiB swap, no swap used during capture |
| Block device | 100 GiB virtual `/dev/sda`, non-rotational flag |
| Filesystem containing `/tmp` | ext4 on `/dev/sda1` |
| `kernel.io_uring_disabled` | `0` |
| `CLK_TCK` | 100 Hz |

### Dataset and immutable observations

The source file was one fully allocated 2 GiB regular file:

| Item | Value |
| --- | --- |
| Path during capture | `/tmp/range-replay-vps.1fRZZs/data-2g.bin` |
| Size | 2,147,483,648 bytes |
| Dataset SHA-256 | `6aa21c6c63f49b6424d03aedcef43c49c336515479b33be79e7894b18323cb64` |
| Largest required offset span, matrix | 805,502,975 bytes |
| Largest required offset span, coalescing | 536,866,816 bytes |

The dataset-generation command was not preserved. This is a provenance limit:
the recorded output checksums can be audited from the included TSV files, but
cannot be regenerated without the original 2 GiB payload. The comparative
procedure itself can be repeated with any sufficiently large regular file;
equal bytes and equal checksums should still hold across backends, while the
checksum values will reflect the replacement payload.

The release raw artifacts are unchanged copies of runner output and GNU
`time -v` output:

| Artifact | SHA-256 |
| --- | --- |
| [`results/v0.1/vps-comparison.tsv`](results/v0.1/vps-comparison.tsv) | `d19b13dfb372ab95b6069fbf54558f0081065ca358c9f82b91d04d14baf83e41` |
| [`results/v0.1/vps-comparison-time.txt`](results/v0.1/vps-comparison-time.txt) | `b8ed0b59204a58e4f3caf1d9c58f1f00720cd43e597a9c21a0b484c27c150e28` |
| [`results/v0.1/vps-coalescing.tsv`](results/v0.1/vps-coalescing.tsv) | `3469435fac9e32a06e2d5b8e6ac7bcd33b48c38b468eeb5f9bc921cb62978390` |
| [`results/v0.1/vps-coalescing-time.txt`](results/v0.1/vps-coalescing-time.txt) | `54c0c02d78d0ac68bba368a47c5d83aee6d748f970eb3afdb95f153899bba3be` |

## Method

### Fixed comparison matrix

Each workload requested exactly 256 MiB of logical data. The runner fixed the
following axes in source before execution:

- range sizes: 4 KiB, 64 KiB, and 1 MiB;
- access patterns: mostly sequential with one-byte gaps, and deterministic
  scattered offsets distributed across four times the logical span;
- `pread` depth 1, then `io_uring` submitted depths 1, 4, and 16;
- physical read size equal to logical range size;
- byte budget equal to 16 physical reads for that workload;
- one warm-up for every workload/backend row;
- eight measured repetitions with rotated backend order.

The canonical plan sorts by offset. “Scattered” means sparse positions in the
file, not randomized issue order. Logical bytes equal physical bytes in every
matrix row, so the matrix has no read amplification.

### Fixed coalescing experiment

The useful payload was 256 MiB represented by 65,536 useful 4 KiB blocks, each
separated from the next by a 4 KiB gap:

- `separate_4k`: 65,536 physical operations, 256 MiB read, zero over-read;
- `grouped_16`: 4,096 physical operations of 124 KiB, 496 MiB read, 240 MiB
  over-read.

Both layouts used the same 1,984 KiB byte budget, `pread` and `io_uring` depth
1, one warm-up, eight measured repetitions, and balanced row order. Grouping
reduced operation count by 93.75% while increasing physical bytes by 93.75%,
for a 1.9375x physical-to-useful byte ratio.

### Timing and aggregation

The comparison timing interval includes backend-facade construction and
execution, including a fresh ring for each `io_uring` row, plus logical output
assembly. Planning, reference comparison, hashing, metric collection, and TSV
rendering are outside the interval.

The coalescing timing interval includes backend execution, reconstruction of
the contiguous useful payload, and destruction of the physical outputs.

Every table below reports the median of eight measured repetitions. “Wall” is
the completion latency of the complete 256 MiB workload. “Amortized
microseconds per operation” is wall time divided by physical-operation count;
for depth greater than 1 it is not an individual I/O latency distribution.
CPU seconds come from process user and system ticks at 100 Hz. Values shorter
than a few hundred milliseconds therefore have coarse CPU precision.

No cache drop was attempted. The declared condition is
`warm_after_one_warmup_per_row`.

## Correctness and data audit

The matrix contains 216 data rows: 24 warm-ups and 192 measured rows. The
coalescing experiment contains 36 data rows: 4 warm-ups and 32 measured rows.
For every declared group, repetitions are exactly 1 through 8 after one
warm-up numbered 0.

All 252 rows report successful byte equality. Logical bytes, physical bytes,
operation count, byte budget, and submitted depth match the fixed protocol.
Every throughput value equals `floor(bytes * 1_000_000_000 / elapsed_ns)`, and
every CPU value equals `(user_ticks + system_ticks) / 100` seconds.

Each matrix workload produced exactly one checksum across both backends, all
depths, the warm-up, and every measured repetition:

| Workload | SHA-256 of complete logical output |
| --- | --- |
| Sequential 4 KiB | `7b65eec1ea0999a2c529a7c855c0d8223a23dd4c377fe818d7b6a6a666a83768` |
| Scattered 4 KiB | `dbbd46991ef730a6aedf93c1ff1a66654f7f368c3e8c51f5e12dd392cc904507` |
| Sequential 64 KiB | `1227dbe3e71ed7fab25e8d7ab81ad761ca7d1dd6e176ab1e212a72f6bf41e978` |
| Scattered 64 KiB | `6824892756d0a7d8eeaad512a2daacb58cca034b45bcbf7736367cea7609b954` |
| Sequential 1 MiB | `0326de2caf761b038cc65b8371320c72c8a03d553d2d7d6cf1da2a4bd970e93f` |
| Scattered 1 MiB | `0d55130b37f2b46098cc6e7bf6f5ba5f04cf7db500bb2f51c13731d71ecbb86e` |

The coalescing experiment produced the same useful-payload checksum in all 36
rows: `02699e97191473dd9b3f8c0cdf63b91d6fb70ae2fdf872f3c54fa6b00b8a6e8b`.

## Comparison results

Throughput is useful MiB/s. CPU is median process CPU seconds. Speedup compares
the row's median throughput with `pread` for the same workload.

| Workload | Backend/depth | Operations | Median wall | MiB/s | Amortized us/op | CPU s | Speedup |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Sequential 4 KiB | `pread/1` | 65,536 | 13.824 s | 18.55 | 210.94 | 13.82 | 1.00x |
| Sequential 4 KiB | `io_uring/1` | 65,536 | 7.501 s | 34.14 | 114.45 | 7.51 | 1.84x |
| Sequential 4 KiB | `io_uring/4` | 65,536 | 7.779 s | 32.92 | 118.69 | 7.78 | 1.77x |
| Sequential 4 KiB | `io_uring/16` | 65,536 | 7.404 s | 34.58 | 112.97 | 7.40 | 1.86x |
| Scattered 4 KiB | `pread/1` | 65,536 | 13.851 s | 18.49 | 211.35 | 13.85 | 1.00x |
| Scattered 4 KiB | `io_uring/1` | 65,536 | 7.367 s | 34.75 | 112.41 | 7.37 | 1.88x |
| Scattered 4 KiB | `io_uring/4` | 65,536 | 7.429 s | 34.46 | 113.36 | 7.43 | 1.86x |
| Scattered 4 KiB | `io_uring/16` | 65,536 | 7.640 s | 33.52 | 116.57 | 7.64 | 1.81x |
| Sequential 64 KiB | `pread/1` | 4,096 | 0.140 s | 1,823.03 | 34.28 | 0.14 | 1.00x |
| Sequential 64 KiB | `io_uring/1` | 4,096 | 0.119 s | 2,149.52 | 29.08 | 0.12 | 1.18x |
| Sequential 64 KiB | `io_uring/4` | 4,096 | 0.119 s | 2,157.69 | 28.97 | 0.12 | 1.18x |
| Sequential 64 KiB | `io_uring/16` | 4,096 | 0.120 s | 2,126.76 | 29.39 | 0.13 | 1.17x |
| Scattered 64 KiB | `pread/1` | 4,096 | 0.179 s | 1,428.51 | 43.77 | 0.19 | 1.00x |
| Scattered 64 KiB | `io_uring/1` | 4,096 | 0.139 s | 1,847.11 | 33.89 | 0.15 | 1.29x |
| Scattered 64 KiB | `io_uring/4` | 4,096 | 0.160 s | 1,616.49 | 38.97 | 0.16 | 1.13x |
| Scattered 64 KiB | `io_uring/16` | 4,096 | 0.157 s | 1,642.04 | 38.25 | 0.16 | 1.15x |
| Sequential 1 MiB | `pread/1` | 256 | 0.096 s | 2,668.57 | 374.73 | 0.10 | 1.00x |
| Sequential 1 MiB | `io_uring/1` | 256 | 0.080 s | 3,205.99 | 311.93 | 0.08 | 1.20x |
| Sequential 1 MiB | `io_uring/4` | 256 | 0.082 s | 3,133.44 | 319.19 | 0.09 | 1.17x |
| Sequential 1 MiB | `io_uring/16` | 256 | 0.097 s | 2,642.08 | 378.61 | 0.10 | 0.99x |
| Scattered 1 MiB | `pread/1` | 256 | 0.089 s | 2,872.36 | 348.29 | 0.09 | 1.00x |
| Scattered 1 MiB | `io_uring/1` | 256 | 0.073 s | 3,509.05 | 285.00 | 0.08 | 1.22x |
| Scattered 1 MiB | `io_uring/4` | 256 | 0.082 s | 3,104.29 | 322.22 | 0.08 | 1.08x |
| Scattered 1 MiB | `io_uring/16` | 256 | 0.089 s | 2,865.89 | 348.94 | 0.09 | 1.00x |

### Backend observations

At 4 KiB, `io_uring` approximately halved complete-workload wall time relative
to `pread`, but submitted depth had no stable direction: depth 16 was 1.3%
faster than depth 1 for the sequential pattern, while depth 1 was best for the
scattered pattern.

At 64 KiB, `io_uring` depth 1 improved median throughput by 18% sequentially
and 29% on the scattered workload. Depth 4 was only 0.4% above depth 1 for the
sequential workload and was 12.5% below it for the scattered workload.

At 1 MiB, depth 1 improved median throughput by 20% to 22%. Depth 4 reduced
that gain, and depth 16 matched or slightly trailed `pread`. This machine does
not support a general recommendation to increase submitted depth for larger
warm-cache reads.

### Range-size and access-pattern observations

The operation-count difference dominates cross-size results: the fixed 256 MiB
payload requires 65,536 operations at 4 KiB, 4,096 at 64 KiB, and 256 at 1
MiB. These rows intentionally change both operation count and range size, so
they identify an operating trend rather than isolate a single causal variable.

The sequential/scattered distinction was visible most clearly at 64 KiB, where
the scattered rows also showed substantial variation. For example,
`io_uring/4` ranged from 0.129 to 0.418 seconds and `pread` ranged from 0.133 to
0.368 seconds. At 4 KiB the two patterns were similar, and at 1 MiB the
scattered medians were slightly faster. Because offsets were sorted and pages
were warm, these results do not measure physical seek behavior.

## Coalescing results

Useful throughput counts only the same 256 MiB useful payload. Physical
throughput includes the inter-block gaps read by `grouped_16`.

| Backend | Layout | Operations | Physical | Over-read | Median wall | Useful MiB/s | Physical MiB/s | Amortized us/op | CPU s | Speedup |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `pread/1` | Separate 4 KiB | 65,536 | 256 MiB | 0 MiB | 12.881 s | 19.88 | 19.88 | 196.54 | 12.88 | 1.00x |
| `pread/1` | Grouped 16 | 4,096 | 496 MiB | 240 MiB | 1.217 s | 210.34 | 407.53 | 297.14 | 1.22 | 10.58x |
| `io_uring/1` | Separate 4 KiB | 65,536 | 256 MiB | 0 MiB | 7.903 s | 32.39 | 32.39 | 120.59 | 7.90 | 1.00x |
| `io_uring/1` | Grouped 16 | 4,096 | 496 MiB | 240 MiB | 1.244 s | 205.83 | 398.81 | 303.69 | 1.25 | 6.35x |

Each grouped operation took more wall time than one separate 4 KiB operation,
as expected for a 124 KiB read and useful-byte projection. The complete
workload was still much faster because the number of operations fell from
65,536 to 4,096. On this warm-cache workload, eliminating 61,440 operations was
worth reading and copying an additional 240 MiB.

That result is not a universal coalescing rule. If gaps are much larger, pages
are cold, storage bandwidth is scarce, memory pressure matters, or useful
blocks are not consumed together, the 1.9375x read amplification may outweigh
the saved operation overhead.

## CPU and cache evidence

GNU `time -v` recorded the complete process runs:

| Run | Wall | User | System | CPU | Major faults | Filesystem inputs | Max RSS |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Comparison | 11:28.53 | 662.21 s | 26.16 s | 99% | 0 | 0 | 548,948 KiB |
| Coalescing | 3:38.05 | 182.04 s | 35.96 s | 99% | 0 | 0 | 1,039,604 KiB |

The absence of reported filesystem input blocks, zero major faults, and CPU
time close to wall time support the declared warm-cache, CPU-bound
interpretation. They do not prove that every page was resident before every
row, nor do they characterize the host's physical storage.

The higher coalescing max RSS is consistent with retaining the 256 MiB useful
reference while handling larger physical outputs. The in-flight byte budget
still governs backend read buffers, not all process memory or final outputs.

## Exact commands

The canonical VPS runs used the following clean checkout and commands:

```bash
rr_report_dir=$(mktemp -d /tmp/range-replay-report.XXXXXX)
git clone --depth 1 --branch main \
  https://github.com/robinber/range-replay.git \
  "$rr_report_dir/repo"
test "$(git -C "$rr_report_dir/repo" rev-parse HEAD)" = \
  29f862bbfef6a0996c8027919650534c8f4bdba7

cargo +1.97.0 build \
  --manifest-path "$rr_report_dir/repo/Cargo.toml" \
  --release \
  --bin range-replay-measure \
  --locked

sha256sum \
  "$rr_report_dir/repo/target/release/range-replay-measure" \
  /tmp/range-replay-vps.1fRZZs/data-2g.bin
getconf CLK_TCK

/usr/bin/time -v \
  -o "$rr_report_dir/comparison-time.txt" \
  "$rr_report_dir/repo/target/release/range-replay-measure" \
  --clock-ticks-per-second 100 \
  /tmp/range-replay-vps.1fRZZs/data-2g.bin \
  > "$rr_report_dir/raw-comparison.tsv"

/usr/bin/time -v \
  -o "$rr_report_dir/coalescing-time.txt" \
  "$rr_report_dir/repo/target/release/range-replay-measure" \
  --clock-ticks-per-second 100 \
  --experiment coalescing \
  /tmp/range-replay-vps.1fRZZs/data-2g.bin \
  > "$rr_report_dir/raw-coalescing.tsv"
```

The actual generated directory was `/tmp/range-replay-report.MSo2T0`. Shell
variables above only abbreviate that exact path.

## Limitations

- One virtual machine, one kernel, one virtual block device, and one run date.
- Warm-page-cache behavior only; no portable cold-cache control and no
  `O_DIRECT`.
- The guest reports a non-rotational virtual disk, but the underlying host
  storage and contention are unknown.
- Eight measured repetitions per row are enough to expose gross effects, not
  to estimate stable population distributions or tail latency.
- CPU accounting is quantized at 10 ms. Short 1 MiB rows have especially coarse
  CPU measurements.
- Workload completion latency is measured; no per-I/O submission-to-completion
  latency histogram is collected.
- The runner includes output assembly and, for `io_uring`, fresh ring creation.
  It is an end-to-end backend-facade comparison, not a syscall microbenchmark.
- The scattered plan is sparse but sorted, and warm pages suppress physical
  seek effects.
- The original dataset-generation command is missing; exact dataset bytes are
  identified only by size and SHA-256.
- The coalescing experiment tests one fixed 4 KiB gap and group size 16. It is
  not an adaptive policy and does not identify a general threshold.
- Results are machine-specific observations, never portable defaults or a
  claim that either backend is the fastest possible implementation.

## Terminal v0.1 conclusion

For tensor-loading-like workloads on this machine, many tiny logical reads are
expensive even when their pages are warm. `io_uring` depth 1 reduced the
end-to-end overhead of the separate-read path, but deeper submission did not
reliably improve it. Larger physical reads made both backends dramatically more
effective by amortizing fixed per-operation work. Fixed coalescing helped when
its 16x operation reduction outweighed a 1.9375x physical-byte amplification;
once grouped, `pread` was as effective as `io_uring` depth 1 within the
resolution and variability of this experiment.

Therefore:

- prefer layouts and plans that avoid unnecessary tiny operations;
- treat coalescing as an explicit useful-bytes versus physical-bytes decision;
- measure `io_uring` depth rather than assuming that a deeper queue is better;
- retain a simple `pread` reference because it remains a strong correctness
  oracle and can be competitive for sufficiently large operations;
- remeasure on the actual target machine and cache condition before using any
  throughput number for capacity planning.

This accepted report completes the intended evidence and conclusion for the
bounded `v0.1` scope. Feature development in this repository is closed.
Interesting follow-up questions belong in a separate project decision, not a
post-`v0.1` range-replay roadmap.
