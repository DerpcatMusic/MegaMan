# Measuring performance

Performance samples count only after the result passes its correctness oracle. Keep raw samples, source/build identity, fixture scope, host/filesystem details and observation boundaries with each result. Native UI latency, directory-model work, throughput, memory and distribution size are separate measurements.

## Directory model

The owned-fixture harness creates zero-byte files in a new directory, measures the initial directory worker/catalog delivery and repeated fresh same-path enumeration, and checks every ordered native name, full path, type and unknown size outside the timed interval. Selection samples measure 100,000 Clear/Select All pairs with optimization barriers, followed by an exact one-range/count oracle. Pair timings include the harness loop and count reads; they are not an isolated action cost.

Commit the harness source, then build with its full source identity:

```sh
MEGAMAN_BENCH_SOURCE=$(git rev-parse HEAD) cargo build --release --locked -p browser-core --example measure_directory
python3 scripts/measure-directory.py --binary /absolute/configured-target/release/examples/measure_directory --output /absolute/new-results-directory
```

Use the configured shared Cargo target directory. Retain the measured executable in your artifact directory before another build can replace it. The runner rejects a dirty source tree, a mismatched embedded source identity, a changed executable or an existing output directory. It preserves fixtures, raw observations and failures. Its parent output directory must already exist. Counts default to 1,000, 10,000 and 100,000, with 15 samples per count; `--entries` and `--samples` allow bounded explicit alternatives.

The initial sample includes worker construction/spawn; rescans include enumerate/sort/publication and observed polling acceptance. A 100-microsecond sleep between model polls adds scheduler/observation overhead. Fixture creation and correctness verification precede or follow timing. Files are freshly created, OS caches are not flushed, and repeated samples are warm. No native window, renderer or input-to-present timestamp participates. Background host load is not isolated. Do not infer cold-cache behavior, app startup, UI latency or superiority over another application from this harness.
