use std::f32::NAN;

use crate::decoders::*;
use crate::decoders::tiff::*;
use crate::decoders::basics::*;

#[derive(Debug, Clone)]
pub struct RafDecoder<'a> {
  buffer: &'a [u8],
  rawloader: &'a RawLoader,
  tiff: TiffIFD<'a>,
}

impl<'a> RafDecoder<'a> {
  pub fn new(buf: &'a [u8], tiff: TiffIFD<'a>, rawloader: &'a RawLoader) -> RafDecoder<'a> {
    RafDecoder {
      buffer: buf,
      tiff: tiff,
      rawloader: rawloader,
    }
  }
}

impl<'a> Decoder for RafDecoder<'a> {
  fn image(&self, dummy: bool) -> Result<RawImage,String> {
    let camera = self.rawloader.check_supported(&self.tiff)?;
    let raw = fetch_ifd!(&self.tiff, Tag::RafOffsets);
    let (width,height) = if raw.has_entry(Tag::RafImageWidth) {
      (fetch_tag!(raw, Tag::RafImageWidth).get_usize(0),
       fetch_tag!(raw, Tag::RafImageLength).get_usize(0))
    } else {
      let sizes = fetch_tag!(raw, Tag::ImageWidth);
      (sizes.get_usize(1), sizes.get_usize(0))
    };
    let offset = fetch_tag!(raw, Tag::RafOffsets).get_usize(0) + raw.start_offset();
    let bps = match raw.find_entry(Tag::RafBitsPerSample) {
      Some(val) => val.get_u32(0) as usize,
      None      => 16,
    };
    let src = &self.buffer[offset..];

    let image = if camera.find_hint("double_width") {
      // Some fuji SuperCCD cameras include a second raw image next to the first one
      // that is identical but darker to the first. The two combined can produce
      // a higher dynamic range image. Right now we're ignoring it.
      decode_16le_skiplines(src, width, height, dummy)
    } else if camera.find_hint("jpeg32") {
      decode_12be_msb32(src, width, height, dummy)
    } else {
      if src.len() < bps*width*height/8 {
        // Compressed RAF — find the Fuji compressed header (0x4953 signature)
        let mut comp_offset = None;
        for i in 0..src.len().saturating_sub(16).min(100000) {
          if src[i] == 0x49 && src[i+1] == 0x53 && (src[i+2] == 0 || src[i+2] == 1) && (src[i+3] == 0 || src[i+3] == 16) {
            comp_offset = Some(i);
            break;
          }
        }
        return match comp_offset {
          Some(off) => {
            let image = fuji_compressed::decode_fuji_compressed(&src[off..], width, height, dummy)?;
            let mut camera = camera;
            // The Fuji compressed decompressor always outputs pixels at a fixed
            // CFA phase (matching rawpy/libraw), which may differ from the phase
            // in rawloader's camera TOML. Override the CFA to match the
            // decompressor's output so downstream WB/demosaic use correct channels.
            camera.cfa = cfa::CFA::new("GGRGGBGGBGGRBRGRBGGGBGGRGGRGGBRBGBRG");
            // The decompressed output includes the full sensor with zero-filled
            // borders (optical black padding) that the TOML crops may not account
            // for. Detect and skip zero rows/columns so they don't produce noise.
            if !dummy {
              let mut top_skip = 0;
              for y in 0..height.min(24) {
                if image[y * width..(y + 1) * width].iter().all(|&v| v == 0) {
                  top_skip = y + 1;
                } else {
                  break;
                }
              }
              let mut bot_skip = 0;
              for y in (height.saturating_sub(24)..height).rev() {
                if image[y * width..(y + 1) * width].iter().all(|&v| v == 0) {
                  bot_skip = height - y;
                } else {
                  break;
                }
              }
              // Sample every 6th row for column checks (aligned to CFA period).
              let sample_rows: Vec<usize> = (0..height).step_by(6).collect();
              let mut right_skip = 0;
              for x in (width.saturating_sub(256)..width).rev() {
                if sample_rows.iter().all(|&y| image[y * width + x] == 0) {
                  right_skip = width - x;
                } else {
                  break;
                }
              }
              let mut left_skip = 0;
              for x in 0..width.min(256) {
                if sample_rows.iter().all(|&y| image[y * width + x] == 0) {
                  left_skip = x + 1;
                } else {
                  break;
                }
              }
              camera.crops[0] = top_skip;
              camera.crops[1] = right_skip;
              camera.crops[2] = bot_skip;
              camera.crops[3] = left_skip;
            }
            ok_image(camera, width, height, self.get_wb()?, image)
          },
          None => Err("RAF: Compressed but could not find Fuji compressed header".to_string()),
        };
      }
      match bps {
        12 => decode_12le(src, width, height, dummy),
        14 => decode_14le_unpacked(src, width, height, dummy),
        16 => {
          if self.tiff.little_endian() {
            decode_16le(src, width, height, dummy)
          } else {
            decode_16be(src, width, height, dummy)
          }
        },
        _ => {return Err(format!("RAF: Don't know how to decode bps {}", bps).to_string());},
      }
    };

    if camera.find_hint("fuji_rotation") || camera.find_hint("fuji_rotation_alt") {
      let (width, height, image) = RafDecoder::rotate_image(&image, &camera, width, height, dummy);
      Ok(RawImage {
        make: camera.make.clone(),
        model: camera.model.clone(),
        clean_make: camera.clean_make.clone(),
        clean_model: camera.clean_model.clone(),
        width: width,
        height: height,
        cpp: 1,
        wb_coeffs: self.get_wb()?,
        data: RawImageData::Integer(image),
        blacklevels: camera.blacklevels,
        whitelevels: camera.whitelevels,
        xyz_to_cam: camera.xyz_to_cam,
        cfa: camera.cfa.clone(),
        crops: [0,0,0,0],
        blackareas: Vec::new(),
        orientation: camera.orientation,
      })
    } else {
      ok_image(camera, width, height, self.get_wb()?, image)
    }
  }
}

impl<'a> RafDecoder<'a> {
  fn get_wb(&self) -> Result<[f32;4], String> {
    match self.tiff.find_entry(Tag::RafWBGRB) {
      Some(levels) => Ok([levels.get_f32(1), levels.get_f32(0), levels.get_f32(2), NAN]),
      None => {
        let levels = fetch_tag!(self.tiff, Tag::RafOldWB);
        Ok([levels.get_f32(1), levels.get_f32(0), levels.get_f32(3), NAN])
      },
    }
  }

  fn rotate_image(src: &[u16], camera: &Camera, width: usize, height: usize, dummy: bool) -> (usize, usize, Vec<u16>) {
    let x = camera.crops[3];
    let y = camera.crops[0];
    let cropwidth = width - camera.crops[1] - x;
    let cropheight = height - camera.crops[2] - y;

    if camera.find_hint("fuji_rotation_alt") {
      let rotatedwidth = cropheight + cropwidth/2;
      let rotatedheight = rotatedwidth-1;

      let mut out: Vec<u16> = alloc_image_plain!(rotatedwidth, rotatedheight, dummy);
      if !dummy {
        for row in 0..cropheight {
          let inb = &src[(row+y)*width+x..];
          for col in 0..cropwidth {
            let out_row = rotatedwidth - (cropheight + 1 - row + (col >> 1));
            let out_col = ((col+1) >> 1) + row;
            out[out_row*rotatedwidth+out_col] = inb[col];
          }
        }
      }

      (rotatedwidth, rotatedheight, out)
    } else {
      let rotatedwidth = cropwidth + cropheight/2;
      let rotatedheight = rotatedwidth-1;

      let mut out: Vec<u16> = alloc_image_plain!(rotatedwidth, rotatedheight, dummy);
      if !dummy {
        for row in 0..cropheight {
          let inb = &src[(row+y)*width+x..];
          for col in 0..cropwidth {
            let out_row = cropwidth - 1 - col + (row>>1);
            let out_col = ((row+1) >> 1) + col;
            out[out_row*rotatedwidth+out_col] = inb[col];
          }
        }
      }

      (rotatedwidth, rotatedheight, out)
    }
  }
}
