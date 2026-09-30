use std::f32::NAN;

use crate::decoders::*;
use crate::decoders::tiff::*;
use crate::decoders::basics::*;

#[derive(Debug, Clone)]
pub struct Rw2Decoder<'a> {
  buffer: &'a [u8],
  rawloader: &'a RawLoader,
  tiff: TiffIFD<'a>,
}

impl<'a> Rw2Decoder<'a> {
  pub fn new(buf: &'a [u8], tiff: TiffIFD<'a>, rawloader: &'a RawLoader) -> Rw2Decoder<'a> {
    Rw2Decoder {
      buffer: buf,
      tiff: tiff,
      rawloader: rawloader,
    }
  }
}

impl<'a> Decoder for Rw2Decoder<'a> {
  fn image(&self, dummy: bool) -> Result<RawImage,String> {
    let width: usize;
    let height: usize;
    let image = {
      let data = self.tiff.find_ifds_with_tag(Tag::PanaOffsets);
      if data.len() > 0 {
        let raw = data[0];
        width = fetch_tag!(raw, Tag::PanaWidth).get_usize(0);
        height = fetch_tag!(raw, Tag::PanaLength).get_usize(0);
        let offset = fetch_tag!(raw, Tag::PanaOffsets).get_usize(0);
        let src = &self.buffer[offset..];
        // Validate the RW2 generation *before* decoding: the format version
        // selects the decoder, and each decoder validates its own tags.
        let rawformat = if raw.has_entry(Tag::PanaRawFormat) {
          fetch_tag!(raw, Tag::PanaRawFormat).get_usize(0)
        } else {
          4 // pre-V8 files predate the RawFormat tag
        };
        match rawformat {
          // V8 strip offsets are absolute file offsets, so pass the
          // whole buffer rather than the raw section.
          8 => Rw2Decoder::decode_panasonic_v8(self.buffer, raw, width, height)?,
          4..=7 => {
            Rw2Decoder::check_panasonic_buf(src, width, height)?;
            Rw2Decoder::decode_panasonic(src, width, height, true, dummy)
          }
          v => return Err(format!("RW2: unsupported raw format version {}", v)),
        }
      } else {
        let raw = fetch_ifd!(&self.tiff, Tag::StripOffsets);
        width = fetch_tag!(raw, Tag::PanaWidth).get_usize(0);
        height = fetch_tag!(raw, Tag::PanaLength).get_usize(0);
        let offset = fetch_tag!(raw, Tag::StripOffsets).get_usize(0);
        let src = &self.buffer[offset..];

        if src.len() >= width*height*2 {
          decode_12le_unpacked_left_aligned(src, width, height, dummy)
        } else if src.len() >= width*height*3/2 {
          decode_12le_wcontrol(src, width, height, dummy)
        } else {
          Rw2Decoder::check_panasonic_buf(src, width, height)?;
          Rw2Decoder::decode_panasonic(src, width, height, false, dummy)
        }
      }
    };

    let mode = {
      let ratio = width*100/height;
      if ratio < 125 {
        "1:1"
      } else if ratio < 145 {
        "4:3"
      } else if ratio < 165 {
        "3:2"
      } else {
        "16:9"
      }
    };
    let camera = self.rawloader.check_supported_with_mode(&self.tiff, mode)?;

    ok_image(camera, width, height, self.get_wb()?, image)
  }
}

impl<'a> Rw2Decoder<'a> {
  fn get_wb(&self) -> Result<[f32;4], String> {
    if self.tiff.has_entry(Tag::PanaWBsR) && self.tiff.has_entry(Tag::PanaWBsB) {
      let r = fetch_tag!(self.tiff, Tag::PanaWBsR).get_u32(0) as f32;
      let b = fetch_tag!(self.tiff, Tag::PanaWBsB).get_u32(0) as f32;
      Ok([r, 256.0, b, NAN])
    } else if self.tiff.has_entry(Tag::PanaWBs2R)
           && self.tiff.has_entry(Tag::PanaWBs2G)
           && self.tiff.has_entry(Tag::PanaWBs2B) {
      let r = fetch_tag!(self.tiff, Tag::PanaWBs2R).get_u32(0) as f32;
      let g = fetch_tag!(self.tiff, Tag::PanaWBs2G).get_u32(0) as f32;
      let b = fetch_tag!(self.tiff, Tag::PanaWBs2B).get_u32(0) as f32;
      Ok([r, g, b, NAN])
    } else {
      Err("Couldn't find WB".to_string())
    }
  }

  /// Decoder for Panasonic RW2 raw format 8 ("V8"), used by the S5 II /
  /// S5 II X / S1R II / G9 II generation. V8 is Huffman-coded strips of
  /// 2x2 CFA tiles, similar to lossless JPEG: each strip starts from a
  /// predictor given in the metadata and every decoded symbol is a
  /// difference to the running prediction. Port of rawspeed's
  /// PanasonicV8Decompressor (LGPL-2.1).
  ///
  /// All generation-specific tags are validated before any decoding starts.
  pub(crate) fn decode_panasonic_v8(src: &[u8], raw: &TiffIFD, width: usize, height: usize) -> Result<Vec<u16>, String> {
    let e = raw.get_endian();

    if raw.has_entry(Tag::PanaBitsPerSample) {
      let bps = fetch_tag!(raw, Tag::PanaBitsPerSample).get_usize(0);
      if bps > 16 {
        return Err(format!("RW2 V8: unexpected bits per sample: {}", bps));
      }
    }

    // The gamma curve and shift-down tables are identity in every known file;
    // refuse anything else rather than silently mis-decoding.
    let gamma_points = pana_vec_u32(raw, Tag::PanaV8GammaPoints)?;
    let gamma_slopes = pana_vec_u32(raw, Tag::PanaV8GammaSlopes)?;
    if gamma_points.iter().any(|&p| p != 0) || gamma_slopes.iter().any(|&s| s != 65536) {
      return Err("RW2 V8: non-identity gamma curve encountered".to_string());
    }
    let shift_down = pana_vec_u16(raw, Tag::PanaV8HufShift)?;
    if shift_down.iter().any(|&s| s != 0) {
      return Err("RW2 V8: non-zero shift-down value encountered".to_string());
    }
    let clip = fetch_tag!(raw, Tag::PanaV8ClipVal).get_u32(0);
    if clip != 0xffff {
      return Err(format!("RW2 V8: unexpected clip value {}", clip));
    }

    // Huffman table: u16 symbol count, then per symbol (u16 len, u16 code).
    let lut = build_v8_lut(fetch_tag!(raw, Tag::PanaV8HufTable).get_data(), e)?;

    // Strip geometry.
    let strips_h = fetch_tag!(raw, Tag::PanaV8StripsH).get_usize(0);
    let strips_v = fetch_tag!(raw, Tag::PanaV8StripsV).get_usize(0);
    let total = strips_h * strips_v;
    let byte_offsets = pana_vec_u32(raw, Tag::PanaV8StripOff)?;
    let line_offsets = pana_vec_u32(raw, Tag::PanaV8StripLines)?;
    let bit_lengths = pana_vec_u32(raw, Tag::PanaV8StripBits)?;
    let strip_widths = pana_vec_u16(raw, Tag::PanaV8StripWidth)?;
    let strip_heights = pana_vec_u16(raw, Tag::PanaV8StripHeight)?;
    if total == 0 || byte_offsets.len() < total || line_offsets.len() < total
      || bit_lengths.len() < total || strip_widths.len() < total || strip_heights.len() < total {
      return Err("RW2 V8: strip tables are inconsistent".to_string());
    }

    let mut rects: Vec<(usize, usize, usize, usize)> = Vec::with_capacity(total);
    for i in 0..total {
      let x = (line_offsets[i] & 0xffff) as usize;
      let y = (line_offsets[i] >> 16) as usize;
      let w = strip_widths[i] as usize;
      let h = strip_heights[i] as usize;
      if x % 2 != 0 || y % 2 != 0 || w % 2 != 0 || h % 2 != 0
        || x + w > width || y + h > height || w == 0 || h == 0 {
        return Err("RW2 V8: invalid strip tile".to_string());
      }
      rects.push((x, y, w, h));
    }
    check_v8_grid(width, height, &rects)?;

    // Initial prediction for the 2x2 CFA tile, row-major [R, G1, G2, B].
    let init_pred = [
      fetch_tag!(raw, Tag::PanaV8InitPredR).get_u32(0) as u16,
      fetch_tag!(raw, Tag::PanaV8InitPredG1).get_u32(0) as u16,
      fetch_tag!(raw, Tag::PanaV8InitPredG2).get_u32(0) as u16,
      fetch_tag!(raw, Tag::PanaV8InitPredB).get_u32(0) as u16,
    ];

    let mut out = vec![0u16; width * height];
    for (i, &(x, y, w, h)) in rects.iter().enumerate() {
      let size = ((bit_lengths[i] + 7) / 8) as usize;
      let off = byte_offsets[i] as usize;
      if off + size > src.len() {
        return Err("RW2 V8: strip data out of bounds".to_string());
      }
      let strip = &src[off..off + size];
      let mut pump = RevMsbPump::new(strip);
      let mut pred = init_pred;
      for rg in 0..h / 2 {
        for blk in 0..w / 2 {
          for j in 0..2 {
            for i2 in 0..2 {
              let diff = pump.next_diff(&lut)?;
              let v = (pred[j * 2 + i2] as i32 + diff).clamp(0, 65535) as u16;
              pred[j * 2 + i2] = v;
              out[(y + rg * 2 + j) * width + x + blk * 2 + i2] = v;
            }
          }
        }
        // At the end of a row group, restart prediction from the first
        // 2x2 tile of the row group just decoded.
        for j in 0..2 {
          for i2 in 0..2 {
            pred[j * 2 + i2] = out[(y + rg * 2 + j) * width + x + i2];
          }
        }
      }
    }
    Ok(out)
  }

  /// Minimum raw-section size `decode_panasonic` can address. Its bit-pump
  /// offsets assume the classic 9/8-bytes-per-pixel packing; smaller buffers
  /// (e.g. the ~1 byte/pixel compression written by the S5 II / G9 II
  /// generation) would be sliced out of bounds on the decode threads. The
  /// bound is the block-aligned offset of the last 5-row group, so it never
  /// rejects files that decode fine today.
  pub(crate) fn panasonic_min_buf_size(width: usize, height: usize) -> usize {
    let row = height.saturating_sub(5);
    let skip = ((width * row * 9) + (width/14 * 2 * row)) / 8;
    (skip / 0x4000) * 0x4000
  }

  fn check_panasonic_buf(src: &[u8], width: usize, height: usize) -> Result<(),String> {
    if src.len() < Rw2Decoder::panasonic_min_buf_size(width, height) {
      Err(format!("RW2: raw section is {} bytes, too small for {}x{}; unsupported compression variant", src.len(), width, height))
    } else {
      Ok(())
    }
  }

  pub(crate) fn decode_panasonic(buf: &[u8], width: usize, height: usize, split: bool, dummy: bool) -> Vec<u16> {
    decode_threaded_multiline(width, height, 5, dummy, &(|out: &mut [u16], row| {
      let skip = ((width * row * 9) + (width/14 * 2 * row)) / 8;
      let blocks = skip / 0x4000;
      let src = &buf[blocks*0x4000..];
      let mut pump = BitPumpPanasonic::new(src, split);
      for _ in 0..(skip % 0x4000) {
        pump.get_bits(8);
      }

      let mut sh: i32 = 0;
      for out in out.chunks_exact_mut(14) {
        let mut pred: [i32;2] = [0,0];
        let mut nonz: [i32;2] = [0,0];

        for i in 0..14 {
          if (i % 3) == 2 {
            sh = 4 >> (3 - pump.get_bits(2));
          }
          if nonz[i & 1] != 0 {
            let j = pump.get_bits(8) as i32;
            if j != 0 {
              pred[i & 1] -= 0x80 << sh;
              if pred[i & 1] < 0 || sh == 4 {
                pred[i & 1] &= !(-1 << sh);
              }
              pred[i & 1] += j << sh;
            }
          } else {
            nonz[i & 1] = pump.get_bits(8) as i32;
            if nonz[i & 1] != 0 || i > 11 {
              pred[i & 1] = nonz[i & 1] << 4 | (pump.get_bits(4) as i32);
            }
          }
          out[i] = pred[i & 1] as u16;
        }
      }
    }))
  }
}

pub struct BitPumpPanasonic<'a> {
  buffer: &'a [u8],
  pos: usize,
  nbits: u32,
  split: bool,
}

impl<'a> BitPumpPanasonic<'a> {
  pub fn new(src: &'a [u8], split: bool) -> BitPumpPanasonic<'a> {
    BitPumpPanasonic {
      buffer: src,
      pos: 0,
      nbits: 0,
      split: split,
    }
  }
}

impl<'a> BitPump for BitPumpPanasonic<'a> {
  fn peek_bits(&mut self, num: u32) -> u32 {
    if num > self.nbits {
      self.nbits += 0x4000 * 8;
      self.pos += 0x4000;
    }
    let mut byte = (self.nbits-num) >> 3 ^ 0x3ff0;
    if self.split {
      byte = (byte + 0x4000 - 0x2008) % 0x4000;
    }
    let bits = LEu16(self.buffer, byte as usize + self.pos - 0x4000) as u32;
    (bits >> ((self.nbits-num) & 7)) & (0x0ffffffffu32 >> (32-num))
  }

  fn consume_bits(&mut self, num: u32) {
    self.nbits -= num;
  }
}

// --- Panasonic RW2 format 8 (V8) helpers -----------------------------------

/// V8 Huffman lookup entry. The default {7, 0} is the sentinel "no code".
#[derive(Clone, Copy)]
struct V8LutEntry {
  bitcount: u8,
  diffcat: u8,
}

const V8_SENTINEL: V8LutEntry = V8LutEntry { bitcount: 7, diffcat: 0 };

/// Panasonic stores V8 tag vectors as a u16 count followed by the values.
fn pana_vec_u16(raw: &TiffIFD, tag: Tag) -> Result<Vec<u16>, String> {
  let data = fetch_tag!(raw, tag).get_data();
  let e = raw.get_endian();
  if data.len() < 2 {
    return Err("RW2 V8: truncated tag vector".to_string());
  }
  let n = e.ru16(data, 0) as usize;
  if data.len() < 2 + n * 2 {
    return Err("RW2 V8: truncated tag vector".to_string());
  }
  Ok((0..n).map(|i| e.ru16(data, 2 + i * 2)).collect())
}

fn pana_vec_u32(raw: &TiffIFD, tag: Tag) -> Result<Vec<u32>, String> {
  let data = fetch_tag!(raw, tag).get_data();
  let e = raw.get_endian();
  if data.len() < 2 {
    return Err("RW2 V8: truncated tag vector".to_string());
  }
  let n = e.ru16(data, 0) as usize;
  if data.len() < 2 + n * 4 {
    return Err("RW2 V8: truncated tag vector".to_string());
  }
  Ok((0..n).map(|i| e.ru32(data, 2 + i * 4)).collect())
}

/// Build a 16-bit lookup table from the Huffman table in the file.
/// NOTE: the V8 code is not a prefix code — the first matching symbol wins.
fn build_v8_lut(data: &[u8], e: Endian) -> Result<Vec<V8LutEntry>, String> {
  if data.len() < 2 {
    return Err("RW2 V8: truncated Huffman table".to_string());
  }
  let num = e.ru16(data, 0) as usize;
  if num < 1 || num > 17 || data.len() < 2 + num * 4 {
    return Err(format!("RW2 V8: bad Huffman symbol count {}", num));
  }
  let mut table: Vec<(u8, u16, u16, u8)> = Vec::with_capacity(num); // (bitcount, symbol, mask, codevalue)
  for i in 0..num {
    let len = e.ru16(data, 2 + i * 4) as usize;
    let code = e.ru16(data, 4 + i * 4) as usize;
    if len < 1 || len > 16 || code >= (1 << len) {
      return Err("RW2 V8: bad Huffman symbol".to_string());
    }
    if len == 7 && i == 0 {
      return Err("RW2 V8: Huffman symbol collides with the sentinel".to_string());
    }
    let bitcount = len as u8;
    table.push((bitcount, (code << (16 - len)) as u16, (0xffffusize << (16 - len)) as u16, i as u8));
  }
  let mut lut = vec![V8_SENTINEL; 1 << 16];
  for li in 0..(1 << 16) {
    let v = li as u16;
    for &(bitcount, symbol, mask, codevalue) in &table {
      if v & mask == symbol {
        lut[li] = V8LutEntry { bitcount, diffcat: codevalue };
        break;
      }
    }
  }
  Ok(lut)
}

/// Verify the strips tile the whole image: each strip either continues the
/// current row or starts the next one, and the last strip ends exactly at
/// the bottom-right corner.
fn check_v8_grid(width: usize, height: usize, rects: &[(usize, usize, usize, usize)]) -> Result<(), String> {
  let (mut px, mut py, mut pw, mut ph) = rects[0];
  if px != 0 || py != 0 {
    return Err("RW2 V8: first strip is out of order".to_string());
  }
  let mut row_right = pw;
  for &(x, y, w, h) in &rects[1..] {
    if x == px + pw && y == py && h == ph {
      px = x; pw = w;
      row_right = x + w;
    } else if x == 0 && y == py + ph {
      if row_right != width {
        return Err("RW2 V8: strip row not fully filled".to_string());
      }
      px = x; py = y; pw = w; ph = h;
      row_right = w;
    } else {
      return Err("RW2 V8: invalid strip tiling".to_string());
    }
  }
  let &(x, y, w, h) = rects.last().unwrap();
  if x + w != width || y + h != height {
    return Err("RW2 V8: strips do not cover the whole image".to_string());
  }
  Ok(())
}

/// MSB-first bit pump that reverses the bit order within each input byte,
/// as V8 strips are written least-significant-bit first within each byte.
struct RevMsbPump<'a> {
  buf: &'a [u8],
  pos: usize,
  cache: u64,
  nbits: u32,
}

impl<'a> RevMsbPump<'a> {
  fn new(buf: &'a [u8]) -> Self {
    RevMsbPump { buf, pos: 0, cache: 0, nbits: 0 }
  }

  fn fill(&mut self) {
    while self.nbits <= 32 {
      let mut v: u32 = 0;
      for k in 0..4 {
        let b = if self.pos + k < self.buf.len() { self.buf[self.pos + k] } else { 0 };
        v = (v << 8) | b.reverse_bits() as u32;
      }
      self.pos += 4;
      self.cache = (self.cache << 32) | v as u64;
      self.nbits += 32;
    }
  }

  fn peek(&self, n: u32) -> u32 {
    debug_assert!(n <= self.nbits);
    ((self.cache >> (self.nbits - n)) & ((1u64 << n) - 1)) as u32
  }

  fn skip(&mut self, n: u32) {
    self.nbits -= n;
  }

  fn get(&mut self, n: u32) -> u32 {
    let v = self.peek(n);
    self.skip(n);
    v
  }

  /// Decode one Huffman symbol: the code gives the difference category
  /// (magnitude), then that many extra bits encode the signed difference
  /// (JPEG-style extend).
  fn next_diff(&mut self, lut: &[V8LutEntry]) -> Result<i32, String> {
    self.fill();
    let e = lut[self.peek(16) as usize];
    if e.bitcount == V8_SENTINEL.bitcount && e.diffcat == V8_SENTINEL.diffcat {
      return Err("RW2 V8: invalid Huffman code in strip".to_string());
    }
    self.skip(e.bitcount as u32);
    if e.diffcat == 0 {
      return Ok(0);
    }
    let len = e.diffcat as u32;
    let d = self.get(len) as i32;
    Ok(if d < (1 << (len - 1)) { d - (1 << len) + 1 } else { d })
  }
}
