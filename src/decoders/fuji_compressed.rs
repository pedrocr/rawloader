/// Fuji compressed RAF decompressor.
///
/// Based on RawSpeed's FujiDecompressor.cpp by:
///   Alexey Danilchenko, Alex Tutubalin, Uwe Müssel, Roman Lebedev
/// Licensed under LGPL-2.1+

use crate::decoders::basics::*;

// Line buffer row indices (18 total = 5R + 8G + 5B)
const R0: usize = 0;
const R2: usize = 2;
const R4: usize = 4;
const G0: usize = 5;
const G2: usize = 7;
const G7: usize = 12;
const B0: usize = 13;
const B2: usize = 15;
const B4: usize = 17;
const LTOTAL: usize = 18;

// Per-row color assignments (RGGB CFA applied to 6 rows):
// Row 0: R→R2, G→G2 | Row 1: G→G3, B→B2 | Row 2: R→R3, G→G4
// Row 3: G→G5, B→B3 | Row 4: R→R4, G→G6 | Row 5: G→G7, B→B4
const ROW_LINES: [(usize, usize); 6] = [
    (R2, G2), (G2+1, B2), (R2+1, G2+2),
    (G2+3, B2+1), (R2+2, G2+4), (G2+5, B2+2),
];

// After each row: which colors to extend (0=R, 1=G, 2=B)
const ROW_EXTENDS: [(usize, usize); 6] = [
    (0, 1), (1, 2), (0, 1), (1, 2), (0, 1), (1, 2),
];

// X-Trans CFA at phase (0,0) — must match the sensor layout.
// 0=R, 1=G, 2=B
const XTRANS_CFA: [[u8; 6]; 6] = [
    [1, 1, 0, 1, 1, 2],  // G G R G G B
    [1, 1, 2, 1, 1, 0],  // G G B G G R
    [2, 0, 1, 0, 2, 1],  // B R G R B G
    [1, 1, 2, 1, 1, 0],  // G G B G G R
    [1, 1, 0, 1, 1, 2],  // G G R G G B
    [0, 2, 1, 2, 0, 1],  // R B G B R G
];

// Color line ranges: (start, count) for R, G, B
const COLOR_RANGES: [(usize, usize); 3] = [(R0, 5), (G0, 8), (B0, 5)];

// ---------------------------------------------------------------------------
// Fuji header
// ---------------------------------------------------------------------------
struct FujiHeader {
    raw_bits: usize,
    raw_height: usize,
    raw_width: usize,
    block_size: usize,
    blocks_in_row: usize,
    total_lines: usize,
}

// ---------------------------------------------------------------------------
// Fuji params
// ---------------------------------------------------------------------------
struct FujiParams {
    q_table: Vec<i8>,
    q_point: [i32; 5],
    max_bits: i32,
    min_value: i32,
    raw_bits: i32,
    total_values: i32,
    max_diff: i32,
    line_width: usize,
}

fn get_gradient(q_point: &[i32; 5], cur_val: i32) -> i8 {
    let v = cur_val - q_point[4];
    let av = v.abs();
    let mut grad: i32 = 0;
    if av > 0 { grad = 1; }
    if av >= q_point[1] { grad = 2; }
    if av >= q_point[2] { grad = 3; }
    if av >= q_point[3] { grad = 4; }
    if v < 0 { grad = -grad; }
    grad as i8
}

impl FujiParams {
    fn new(header: &FujiHeader) -> Result<FujiParams, String> {
        let line_width = (header.block_size * 2) / 3;
        let q_point: [i32; 5] = [0, 0x12, 0x43, 0x114, (1i32 << header.raw_bits) - 1];
        let min_value = 0x40i32;

        let n = 2 * (1 << header.raw_bits);
        let mut q_table = vec![0i8; n];
        for i in 0..n {
            q_table[i] = get_gradient(&q_point, i as i32);
        }

        let (total_values, raw_bits, max_bits, max_diff) = match q_point[4] {
            0x3FFF => (0x4000, 14, 56, 256),
            0xFFFF => (0x10000, 16, 64, 1024),
            _ => return Err(format!("Unsupported raw_bits for compression: {}", header.raw_bits)),
        };

        Ok(FujiParams { q_table, q_point, max_bits, min_value, raw_bits, total_values, max_diff, line_width })
    }

    #[inline(always)]
    fn quant_gradient(&self, v1: i32, v2: i32) -> i32 {
        let idx1 = (self.q_point[4] + v1) as usize;
        let idx2 = (self.q_point[4] + v2) as usize;
        9 * (self.q_table[idx1] as i32) + (self.q_table[idx2] as i32)
    }
}

// ---------------------------------------------------------------------------
// Gradient pair (adaptive statistics)
// ---------------------------------------------------------------------------
#[derive(Clone, Copy)]
struct GradPair {
    value1: i32,
    value2: i32,
}

// ---------------------------------------------------------------------------
// Bitstream
// ---------------------------------------------------------------------------
#[inline(always)]
fn fuji_zerobits(pump: &mut BitPumpMSB) -> i32 {
    let mut count: i32 = 0;
    loop {
        let batch = pump.peek_bits(32);
        let zeros = batch.leading_zeros() as i32;
        count += zeros;
        if zeros < 32 {
            pump.consume_bits((zeros + 1) as u32);
            break;
        }
        pump.consume_bits(32);
    }
    count
}

#[inline(always)]
fn bit_diff(value1: i32, value2: i32) -> i32 {
    if value1 <= 0 { return 0; }
    if value2 <= 0 { return 15; }
    let lz1 = (value1 as u32).leading_zeros() as i32;
    let lz2 = (value2 as u32).leading_zeros() as i32;
    let mut dec_bits = (lz2 - lz1).max(0);
    if (value2 << dec_bits) < value1 {
        dec_bits += 1;
    }
    dec_bits.min(15)
}

#[inline(always)]
fn fuji_decode_sample(
    pump: &mut BitPumpMSB,
    params: &FujiParams,
    grad: i32,
    interp_val: i32,
    grads: &mut [GradPair; 41],
) -> u16 {
    let gradient = grad.unsigned_abs() as usize;
    let sample_bits = fuji_zerobits(pump);

    let (code_bits, code_delta): (i32, i32);
    if sample_bits < params.max_bits - params.raw_bits - 1 {
        code_bits = bit_diff(grads[gradient].value1, grads[gradient].value2);
        code_delta = sample_bits << code_bits;
    } else {
        code_bits = params.raw_bits;
        code_delta = 1;
    }

    let mut code = if code_bits > 0 { pump.get_bits(code_bits as u32) as i32 } else { 0 };
    code += code_delta;

    // Zigzag decode
    code = if code & 1 != 0 { -1 - code / 2 } else { code / 2 };

    // Update gradient statistics
    grads[gradient].value1 += code.abs();
    if grads[gradient].value2 == params.min_value {
        grads[gradient].value1 >>= 1;
        grads[gradient].value2 >>= 1;
    }
    grads[gradient].value2 += 1;

    // Apply code to interpolation value
    let mut result = if grad < 0 { interp_val - code } else { interp_val + code };

    if result < 0 {
        result += params.total_values;
    } else if result > params.q_point[4] {
        result -= params.total_values;
    }

    result.max(0).min(params.q_point[4]) as u16
}

// ---------------------------------------------------------------------------
// Line buffer access
// ---------------------------------------------------------------------------
#[inline(always)]
fn l(lines: &[i32], stride: usize, row: usize, col: usize) -> i32 {
    lines[row * stride + col]
}

#[inline(always)]
fn set_l(lines: &mut [i32], stride: usize, row: usize, col: usize, val: u16) {
    lines[row * stride + col] = val as i32;
}

// ---------------------------------------------------------------------------
// Interpolation (prediction from neighbors)
// ---------------------------------------------------------------------------
#[inline(always)]
fn interpolation_even_inner(
    lines: &[i32], stride: usize, params: &FujiParams, c: usize, col: usize,
) -> (i32, i32) {
    let rb = l(lines, stride, c - 1, 1 + 2 * col);
    let rc = l(lines, stride, c - 1, 2 * col);
    let rd = l(lines, stride, c - 1, 2 * col + 2);
    let rf = l(lines, stride, c - 2, 1 + 2 * col);

    let d_rc_rb = (rc - rb).abs();
    let d_rf_rb = (rf - rb).abs();
    let d_rd_rb = (rd - rb).abs();

    let term0 = 2 * rb;
    let (term1, term2);
    if d_rc_rb > d_rf_rb.max(d_rd_rb) {
        term1 = rf;
        term2 = rd;
    } else {
        term1 = if d_rd_rb > d_rc_rb.max(d_rf_rb) { rf } else { rd };
        term2 = rc;
    }

    let interp_val = (term0 + term1 + term2) >> 2;
    let grad = params.quant_gradient(rb - rf, rc - rb);
    (grad, interp_val)
}

#[inline(always)]
fn interpolation_odd_inner(
    lines: &[i32], stride: usize, params: &FujiParams, c: usize, col: usize,
) -> (i32, i32) {
    let ra = l(lines, stride, c, 1 + 2 * col);
    let rb = l(lines, stride, c - 1, 1 + 2 * col + 1);
    let rc = l(lines, stride, c - 1, 1 + 2 * col);
    let rd = l(lines, stride, c - 1, 1 + 2 * (col + 1));
    let rg = l(lines, stride, c, 1 + 2 * (col + 1));

    let mut interp_val = ra + rg;
    let mn = rc.min(rd);
    let mx = rc.max(rd);
    if rb < mn || rb > mx {
        interp_val += 2 * rb;
        interp_val >>= 1;
    }
    interp_val >>= 1;

    let grad = params.quant_gradient(rb - rc, rc - ra);
    (grad, interp_val)
}

// ---------------------------------------------------------------------------
// X-Trans interpolation pattern
// ---------------------------------------------------------------------------
#[inline(always)]
fn is_interpolation(row: usize, comp: usize, i: usize) -> bool {
    if comp == 0 {
        row == 0 || row == 5 || (row == 2 && i % 2 == 0) || (row == 4 && i % 2 != 0)
    } else {
        row == 1 || row == 2 || (row == 3 && i % 2 != 0) || (row == 5 && i % 2 == 0)
    }
}

// ---------------------------------------------------------------------------
// Extend helper columns
// ---------------------------------------------------------------------------
fn extend_generic(lines: &mut [i32], stride: usize, start: usize, end: usize) {
    for i in start..=end {
        lines[i * stride] = lines[(i - 1) * stride + 1];
        lines[i * stride + stride - 1] = lines[(i - 1) * stride + stride - 2];
    }
}

#[inline(always)]
fn extend_color(lines: &mut [i32], stride: usize, color: usize) {
    match color {
        0 => extend_generic(lines, stride, R2, R4),
        1 => extend_generic(lines, stride, G2, G7),
        2 => extend_generic(lines, stride, B2, B4),
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Block decode (6 rows of one MCU line)
// ---------------------------------------------------------------------------
fn xtrans_decode_block(
    pump: &mut BitPumpMSB,
    params: &FujiParams,
    lines: &mut [i32],
    stride: usize,
    grad_even: &mut [[GradPair; 41]; 3],
    grad_odd: &mut [[GradPair; 41]; 3],
) {
    let half_lw = params.line_width / 2;

    for row in 0..6usize {
        let (c0, c1) = ROW_LINES[row];
        let grad_idx = row % 3;

        let mut col_even = [0usize; 2];
        let mut col_odd = [0usize; 2];

        for i in 0..(half_lw + 4) {
            // Decode even pixels
            if i < half_lw {
                for comp in 0..2usize {
                    let c = if comp == 0 { c0 } else { c1 };
                    let col = col_even[comp];

                    let sample = if is_interpolation(row, comp, i) {
                        let (_, interp_val) = interpolation_even_inner(lines, stride, params, c, col);
                        interp_val.max(0).min(params.q_point[4]) as u16
                    } else {
                        let (grad, interp_val) = interpolation_even_inner(lines, stride, params, c, col);
                        fuji_decode_sample(pump, params, grad, interp_val, &mut grad_even[grad_idx])
                    };

                    set_l(lines, stride, c, 1 + 2 * col, sample);
                    col_even[comp] += 1;
                }
            }

            // Decode odd pixels (start 4 positions behind even)
            if i >= 4 {
                for comp in 0..2usize {
                    let c = if comp == 0 { c0 } else { c1 };
                    let col = col_odd[comp];

                    let (grad, interp_val) = interpolation_odd_inner(lines, stride, params, c, col);
                    let sample = fuji_decode_sample(pump, params, grad, interp_val, &mut grad_odd[grad_idx]);

                    set_l(lines, stride, c, 1 + 2 * col + 1, sample);
                    col_odd[comp] += 1;
                }
            }
        }

        // Extend helper columns
        let (ext0, ext1) = ROW_EXTENDS[row];
        extend_color(lines, stride, ext0);
        extend_color(lines, stride, ext1);
    }
}

// ---------------------------------------------------------------------------
// Copy decoded lines to output image
// ---------------------------------------------------------------------------
#[inline(always)]
fn xtrans_col_index(img_col: usize) -> usize {
    (((img_col * 2 / 3) & 0x7FFFFFFE) | ((img_col % 3) & 1)) + ((img_col % 3) >> 1)
}

fn copy_line_to_xtrans(
    lines: &[i32],
    stride: usize,
    strip_width: usize,
    strip_offset_x: usize,
    cur_line: usize,
    out: &mut [u16],
    out_width: usize,
    out_height: usize,
) {
    let num_mcus_x = strip_width / 6;

    for mcu_x in 0..num_mcus_x {
        for mcu_row in 0..6usize {
            let out_y = 6 * cur_line + mcu_row;
            if out_y >= out_height { continue; }

            for mcu_col in 0..6usize {
                let img_col = 6 * mcu_x + mcu_col;
                let out_x = strip_offset_x + img_col;
                if out_x >= out_width { continue; }

                let color = XTRANS_CFA[mcu_row][mcu_col];
                let row = match color {
                    0 => R2 + (mcu_row >> 1),       // RED
                    1 => G2 + mcu_row,               // GREEN
                    _ => B2 + (mcu_row >> 1),        // BLUE
                };

                let buf_col = 1 + xtrans_col_index(img_col);
                let val = lines[row * stride + buf_col];
                out[out_y * out_width + out_x] = val as u16;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Strip decoder
// ---------------------------------------------------------------------------
fn decode_strip(
    src: &[u8],
    header: &FujiHeader,
    params: &FujiParams,
    strip_width: usize,
    strip_offset_x: usize,
    out: &mut [u16],
    out_width: usize,
    out_height: usize,
) {
    let stride = params.line_width + 2;
    let mut lines = vec![0i32; LTOTAL * stride];

    let init = GradPair { value1: params.max_diff, value2: 1 };
    let mut grad_even = [[init; 41]; 3];
    let mut grad_odd = [[init; 41]; 3];

    // Pad source for safe BitPump reads near end
    let mut padded = src.to_vec();
    padded.extend_from_slice(&[0u8; 16]);
    let mut pump = BitPumpMSB::new(&padded);

    for cur_line in 0..header.total_lines {
        if cur_line > 0 {
            // Rotate: last 2 lines of each color → first 2
            for &(start, count) in &COLOR_RANGES {
                let src_off = (start + count - 2) * stride;
                let dst_off = start * stride;
                for i in 0..(2 * stride) {
                    lines[dst_off + i] = lines[src_off + i];
                }
            }
            // Set helper column for first decoded line
            for &(start, _) in &COLOR_RANGES {
                let row = start + 2;
                let prev = start + 1;
                lines[row * stride + stride - 1] = lines[prev * stride + stride - 2];
            }
        }

        xtrans_decode_block(&mut pump, params, &mut lines, stride, &mut grad_even, &mut grad_odd);
        copy_line_to_xtrans(&lines, stride, strip_width, strip_offset_x, cur_line, out, out_width, out_height);
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------
pub fn decode_fuji_compressed(src: &[u8], width: usize, height: usize, dummy: bool) -> Result<Vec<u16>, String> {
    if dummy {
        return Ok(vec![0; 1]);
    }

    // Parse header (big-endian, 16 bytes)
    if src.len() < 16 {
        return Err("RAF compressed: data too short".to_string());
    }

    let signature = BEu16(src, 0);
    let version = src[2];
    let raw_type = src[3];

    if signature != 0x4953 || version != 1 {
        return Err(format!("RAF compressed: bad header sig=0x{:04x} ver={}", signature, version));
    }
    if raw_type != 16 {
        return Err(format!("RAF compressed: only X-Trans supported, got raw_type={}", raw_type));
    }

    let raw_bits = src[4] as usize;
    let raw_height = BEu16(src, 5) as usize;
    let raw_width = BEu16(src, 9) as usize;
    let block_size = BEu16(src, 11) as usize;
    let blocks_in_row = src[13] as usize;
    let total_lines = BEu16(src, 14) as usize;

    if block_size == 0 || blocks_in_row == 0 || total_lines == 0 {
        return Err("RAF compressed: invalid header dimensions".to_string());
    }

    let header = FujiHeader {
        raw_bits, raw_height, raw_width, block_size, blocks_in_row, total_lines,
    };
    let params = FujiParams::new(&header)?;

    // Read block sizes
    let bs_off = 16;
    let mut block_sizes = Vec::with_capacity(blocks_in_row);
    for i in 0..blocks_in_row {
        let off = bs_off + i * 4;
        if off + 4 > src.len() {
            return Err("RAF compressed: data too short for block sizes".to_string());
        }
        block_sizes.push(BEu32(src, off) as usize);
    }

    // Compute strip data start (with alignment padding)
    let raw_offset = 4 * blocks_in_row;
    let padding = if raw_offset & 0xC != 0 { 0x10 - (raw_offset & 0xC) } else { 0 };
    let mut data_off = 16 + raw_offset + padding;

    // Allocate output
    let mut out = vec![0u16; width * height];

    // Decode each strip
    for block in 0..blocks_in_row {
        let strip_size = block_sizes[block];
        let strip_end = data_off + strip_size;
        if strip_end > src.len() {
            return Err("RAF compressed: strip data extends beyond buffer".to_string());
        }

        let strip_src = &src[data_off..strip_end];
        let strip_width = if block + 1 < blocks_in_row {
            block_size
        } else {
            raw_width - block_size * block
        };
        let strip_offset_x = block_size * block;

        decode_strip(strip_src, &header, &params, strip_width, strip_offset_x, &mut out, width, height);

        data_off = strip_end;
    }

    Ok(out)
}
