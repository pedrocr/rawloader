# rawloader

[![Build Status](https://travis-ci.org/pedrocr/rawloader.svg?branch=master)](https://travis-ci.org/pedrocr/rawloader)
[![Crates.io](https://img.shields.io/crates/v/rawloader.svg)](https://crates.io/crates/rawloader)

This is a rust library to extract the raw data and some metadata from digital camera images. Given an image in a supported format and camera you will be able to get everything needed to process the image:

  * Identification of the camera that produced the image (both the EXIF name and a cleaned up name)
  * The raw pixels themselves, exactly as encoded by the camera
  * The number of pixels to crop on the top, right, bottom, left of the image to only use the actual image area
  * The black and white points of each of the color channels
  * The multipliers to apply to the color channels for the white balance
  * A conversion matrix between the camera color space and XYZ
  * The description of the bayer pattern itself so you'll know which pixels are which color

Current State
-------------

The library is still a work in process with the following formats already implemented:
  * Minolta MRW
  * Sony ARW, SRF and SR2
  * Mamiya MEF
  * Olympus ORF
  * Samsung SRW
  * Epson ERF
  * Kodak KDC
  * Kodak DCS
  * Panasonic RW2 (also used by Leica)
  * Fuji RAF
  * Kodak DCR
  * Adobe DNG (the "good parts"<sup>1</sup>)
  * Pentax PEF
  * Canon CRW
  * "Naked" files<sup>2</sup>
  * Leaf IIQ
  * Hasselblad 3FR
  * Nikon NRW
  * Nikon NEF
  * Leaf MOS
  * Canon CR2
  * ARRI's ARI

<sup>1</sup> DNG is a 101 page overambitious spec that tries to be an interchange format for processed images, complete with image transformation operations. We just implement enough of the spec so that actual raw files from DNG producing cameras or the Adobe DNG converter can be read.

<sup>2</sup> Files that are just the raw data itself with no metadata whatsoever. The most common of these are the files generated by the Canon CHDK hacked firmware. Later versions produced actual DNG files but the first ones just did a dump of the raw data next to the JPG and assumed the user would use the JPG for the metadata. We match them by the filesize itself which means that if you feed rawloader with a file that has the exact same bytecount as these files you'll get a nice garbage output...

Usage
-----

Here's a simple sample program that uses this library:

```rust
use std::env;
use std::fs::File;
use std::io::prelude::*;
use std::io::BufWriter;

fn main() {
  let args: Vec<_> = env::args().collect();
  if args.len() != 2 {
    println!("Usage: {} <file>", args[0]);
    std::process::exit(2);
  }
  let file = &args[1];
  let image = rawloader::decode_file(file).unwrap();

  // Write out the image as a grayscale PPM
  let mut f = BufWriter::new(File::create(format!("{}.ppm",file)).unwrap());
  let preamble = format!("P6 {} {} {}\n", image.width, image.height, 65535).into_bytes();
  f.write_all(&preamble).unwrap();
  if let rawloader::RawImageData::Integer(data) = image.data {
    for pix in data {
      // Do an extremely crude "demosaic" by setting R=G=B
      let pixhigh = (pix>>8) as u8;
      let pixlow  = (pix&0x0f) as u8;
      f.write_all(&[pixhigh, pixlow, pixhigh, pixlow, pixhigh, pixlow]).unwrap()
    }
  } else {
    eprintln!("Don't know how to process non-integer raw files");
  }
}
```

Contributing
------------

Bug reports and pull requests welcome at https://github.com/pedrocr/rawloader

Meet us at #chimper on irc.freenode.net if you need to discuss a feature or issue in detail or even just for general chat.
