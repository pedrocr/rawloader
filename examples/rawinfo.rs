fn main() -> Result<(), rawloader::RawLoaderError> {
  let args: Vec<_> = std::env::args().collect();
  if args.len() != 2 {
    println!("Usage: {} <file>", args[0]);
    std::process::exit(2);
  }
  let image = rawloader::decode_file(&args[1])?;

  println!("make: {}", image.make);
  println!("model: {}", image.model);
  println!("clean_make: {}", image.clean_make);
  println!("clean_model: {}", image.clean_model);
  println!("width: {}", image.width);
  println!("height: {}", image.height);
  println!("cpp: {}", image.cpp);
  println!("wb_coeffs: {:?}", image.wb_coeffs);
  println!("whitelevels: {:?}", image.whitelevels);
  println!("blacklevels: {:?}", image.blacklevels);
  println!("xyz_to_cam: {:?}", image.xyz_to_cam);
  println!("cfa: {}", image.cfa);
  println!("crops: {:?}", image.crops);
  println!("blackareas: {:?}", image.blackareas);
  println!("orientation: {:?}", image.orientation);
  
  use sha2::{Sha256, Digest};
  let mut hasher = Sha256::new();
  match image.data {
    rawloader::RawImageData::Integer(data) => {
      for val in data { hasher.update(val.to_le_bytes()); }
    },
    rawloader::RawImageData::Float(data) => {
      for val in data { hasher.update(val.to_le_bytes()); }
    },
  };
  println!("sha256data: {}", hex::encode(hasher.finalize()));

  Ok(())
}
