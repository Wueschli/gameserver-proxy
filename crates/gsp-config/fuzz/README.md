# gsp-config fuzz targets

`cargo-fuzz` (libFuzzer) harnesses for the parsers that see **untrusted input**:

| target | what it hits |
|--------|--------------|
| `extract_sni` | `gsp_config::extract_sni` — the hand-rolled TLS ClientHello reader run on the TCP peek buffer. Must never panic. |
| `route_match` | `ListenerConfig::route_for` / `first_packet_recognised` with a fuzzed first-bytes buffer against a fixed matcher set (`first_bytes`, `sni`, `always`) — covers `Matcher::matches`, `extract_sni`, `HostPattern`. |
| `parse_config` | `gsp_config::parse_str` on arbitrary UTF-8 — the whole validation path (matcher / CIDR / byte-spec / country-code parsing, cross-field checks). Must return `Ok`/`Err`, never panic. |

## Running

Needs a **nightly** toolchain (libFuzzer sanitizer flags) and `cargo-fuzz`:

```sh
rustup toolchain install nightly
cargo install cargo-fuzz

cd crates/gsp-config
for t in extract_sni route_match parse_config; do
  mkdir -p "fuzz/corpus/$t"
  cargo +nightly fuzz run "$t" "fuzz/corpus/$t" "fuzz/seeds/$t" -- -max_total_time=60
done
```

The first path (`fuzz/corpus/<t>`, git-ignored) is where libFuzzer writes new
inputs; `fuzz/seeds/<t>` is a read-only set of committed starter inputs. `make
fuzz` (`FUZZ_TIME=<sec>`) does exactly this. Crashes land in
`fuzz/artifacts/<target>/`; reproduce with
`cargo +nightly fuzz run <target> fuzz/artifacts/<target>/<crash-file>`.

In CI the `fuzz` job runs a 45 s smoke pass per target on every push / PR; when
it finds a crash the step fails and the crashing inputs are uploaded as the
`fuzz-artifacts` bundle on the workflow run (download, drop under
`fuzz/artifacts/<target>/`, and reproduce with the command above).

`fuzz/corpus/`, `artifacts/`, `coverage/` and `target/` are git-ignored.

This crate is a standalone workspace (`[workspace]` in its `Cargo.toml`) so its
sanitizer build never touches the main build.
