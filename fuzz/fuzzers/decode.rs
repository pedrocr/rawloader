#![no_main]
#[macro_use] extern crate libfuzzer_sys;
extern crate rawloader;

use std::io;

fuzz_target!(|data: &[u8]| {
    let loader = rawloader::decoders::RawLoader::new();
    let mut input = io::Cursor::new(data);
    // Decode the input data, but ignore the result. Both a successful decode
    // and an error are fine, the point is catching panics.
    match loader.decode(&mut input) {
        Ok(..) => {}
        Err(..) => {}
    }
});
