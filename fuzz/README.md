# Fuzzing

To fuzz this crate:

    cargo install cargo-fuzz
    RUSTFLAGS="-Cpanic=unwind" cargo +nightly fuzzer run decode
    ^C

Selecting a nightly toolchain with `+nightly` is a feature of
[rustup](https://rustup.rs/).

Just fuzzing without seeding the corpus will likely not discover anything. The
number of program paths covered (the `cov` value in the output) will be small.
To seed the corpus, copy a few raw files into `fuzz/corpus/decode`. Now running
`cargo +nightly fuzzer run decode` should quickly discover more paths.

It can be useful to have small inputs to reproduce a hang. To do so, limit the
input length:

    RUSTFLAGS="-Cpanic=unwind" cargo +nightly fuzzer run decode -- -max_len=1024

At some point the fuzzer might not discover new program paths any more, because
a file that is too small cannot trigger all paths. Increase `max_len` to
continue searching for more complex examples.
