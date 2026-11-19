# Codec fuzzing and libubus comparison

With a nightly Rust toolchain and [cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz), run the codec target from the repository root.

```sh
cargo +nightly fuzz run codec -- -dict=fuzz/codec.dict -max_len=16384 -max_total_time=60
```

The target exercises blobmsg decoding, re-encoding, and ubus frame and attribute parsing. Accepted tables must have a stable wire encoding after re-encoding. LibFuzzer stores the working corpus and any failure artifacts under `fuzz/`.

The Docker suite runs a seeded C libubus peer against the Rust provider and a C echo provider using upstream libubus on the same daemon. It compares raw blobmsg replies and final status. The default run uses 512 cases with UTF-8 and Latin-1 field names, positional arrays, and arbitrary string and numeric payloads.

```sh
UBUS_DIFFERENTIAL_CASES=10000 tests/ubusd/run.sh master
```

The runner also accepts `24.10` and `25.12`. A reported case index is reproducible with any run that includes that index.
