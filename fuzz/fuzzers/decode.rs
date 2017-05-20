#![no_main]
#[macro_use] extern crate libfuzzer_sys;
extern crate rawloader;

use std::io;
use std::panic;

fuzz_target!(|data: &[u8]| {
    let loader = rawloader::decoders::RawLoader::new();
    // Decode the input data, but ignore the result. Both a successful decode
    // and an error are fine, the point is catching hangs. Panics are not
    // considered bugs, so they are silenced.
    match panic::catch_unwind(|| loader.decode(&mut io::Cursor::new(data))) {
        Ok(..) => {}
        Err(..) => {}
    }
});
