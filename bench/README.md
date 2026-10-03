# bench

The bench runs pyrographer against real hardware and keeps a transcript of each run.

Every other test in this repository runs against a scripted transport: bytes in, bytes out, no
device. That makes the codecs and the verbs testable without hardware, but it cannot stand in
for a pass against a device that can refuse.

## The Block backend

`blockbench` is the harness for the Block backend. It is an example in `pyrographer-core`, so it
calls the library's own `block::open`, `verbs::dump`, `verbs::plan_write`, `verbs::flash` and
`verbs::verify` directly.

The bench measures pyrographer against the platform. A separate probe, kept outside this
repository, measures what Linux itself allows on a raw block device.

Run the bench with one command:

    ./bench/run-block-bench.sh [size]

The script prompts for a password once. It creates a scratch file in `/tmp`, binds a loop device
to it through udisks, and runs four passes. It tees the transcript to
`bench/reports/block-<timestamp>.log`. On exit, it deletes the loop device and the scratch file,
whether or not the passes succeeded.

| Pass | Privilege | What it proves |
| --- | --- | --- |
| 1. inventory | none | that geometry, bus, mounts and the running-system refusal are all readable with nothing opened |
| 2. a GPT read back | root | that `verbs::partitions` parses a table written by another tool, over a block device |
| 3. read and write | root | the open, `dump`, the range checks on a live device, `plan_write`, `flash` with its per-window read-back, `verify`, and a second open that finds the written bytes still present |
| 4. the mounted case | root | that `O_EXCL` refuses a device the kernel holds, the claim the whole backend depends on |

Each check prints `ok` or `FAIL` as it runs, and again in a summary. If any check fails, the
harness exits non-zero.

### Running it by hand

The harness takes the device as an argument, and a write requires the device to be named twice:

    cargo build --release --example blockbench
    ./target/release/examples/blockbench                       # the inventory
    sudo ./target/release/examples/blockbench --target /dev/loopN \
        --confirm /dev/loopN --write

`--help` lists the other options. Before a write, the harness applies three guards:

- The target and `--confirm` must name the same device.
- A device larger than `--max-bytes` (8 GiB by default) is refused, so a mistyped device name
  cannot reach a real disk.
- A device with anything mounted on it is refused, unless `--allow-mounted` is passed.

The backend's own refusal of every disk the running system depends on applies before all three
guards, and has no override.

### Limits of a loop device

A loop device has the block layer's semantics (`O_EXCL`, `O_DIRECT`, partition scanning, the
page cache), and the passes test those semantics. Its backing store is a file in the host's own
page cache. A durability result measured here therefore applies to the API path, not to a
physical medium. To test durability on a physical medium, run the same harness against a USB
stick. `--max-bytes` exists to allow that run.
