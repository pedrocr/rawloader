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
    lossless: bool,
}

// ---------------------------------------------------------------------------
// Quantization table (one per gradient level)
// ---------------------------------------------------------------------------
#[derive(Clone)]
struct FujiQTable {
    q_table: Vec<i8>,
    q_base: i32,
    raw_bits: i32,
    total_values: i32,
    max_grad: i32,
    q_grad_mult: i32,
}

// ---------------------------------------------------------------------------
// Fuji params
// ---------------------------------------------------------------------------
struct FujiParams {
    qt: [FujiQTable; 4],  // qt[0] = main, qt[1..3] = lossy sub-tables
    max_bits: i32,
    min_value: i32,
    max_value: i32,
    line_width: usize,
    lossless: bool,
}

fn log2ceil(mut val: i32) -> i32 {
    let mut result = 0;
    val -= 1;
    if val > 0 {
        loop {
            result += 1;
            val >>= 1;
            if val == 0 { break; }
        }
    }
    result
}

fn setup_qlut(max_value: i32, qp: &[i32; 5]) -> Vec<i8> {
    let n = (2 * max_value + 1) as usize;
    let mut qt = vec![0i8; n];
    for (i, entry) in qt.iter_mut().enumerate() {
        let cur_val = i as i32 - max_value;
        *entry = if cur_val <= -qp[3] { -4 }
        else if cur_val <= -qp[2] { -3 }
        else if cur_val <= -qp[1] { -2 }
        else if cur_val < -qp[0] { -1 }
        else if cur_val <= qp[0] { 0 }
        else if cur_val < qp[1] { 1 }
        else if cur_val < qp[2] { 2 }
        else if cur_val < qp[3] { 3 }
        else { 4 };
    }
    qt
}

fn make_main_qtable(max_value: i32, q_base: i32) -> (FujiQTable, i32) {
    let max_val_p1 = max_value + 1;
    let mut qp = [0i32; 5];
    qp[0] = q_base;
    qp[1] = 3 * q_base + 0x12;
    qp[2] = 5 * q_base + 0x43;
    qp[3] = 7 * q_base + 0x114;
    qp[4] = max_value;
    if qp[1] >= max_val_p1 || qp[1] < q_base + 1 { qp[1] = q_base + 1; }
    if qp[2] < qp[1] || qp[2] >= max_val_p1 { qp[2] = qp[1]; }
    if qp[3] < qp[2] || qp[3] >= max_val_p1 { qp[3] = qp[2]; }

    let total_values = (qp[4] + 2 * q_base) / (2 * q_base + 1) + 1;
    let raw_bits = log2ceil(total_values);
    let max_bits = 4 * log2ceil(qp[4] + 1);
    let q_table = setup_qlut(max_value, &qp);

    (FujiQTable {
        q_table,
        q_base,
        raw_bits,
        total_values,
        max_grad: 0,
        q_grad_mult: 9,
    }, max_bits)
}

fn empty_qtable() -> FujiQTable {
    FujiQTable { q_table: Vec::new(), q_base: 0, raw_bits: 0, total_values: 0, max_grad: 0, q_grad_mult: 0 }
}

impl FujiParams {
    fn new(header: &FujiHeader) -> Result<FujiParams, String> {
        let line_width = (header.block_size * 2) / 3;
        let min_value = 0x40i32;
        let max_value = (1i32 << header.raw_bits) - 1;

        if header.lossless {
            let (qt0, max_bits) = make_main_qtable(max_value, 0);
            Ok(FujiParams {
                qt: [qt0, empty_qtable(), empty_qtable(), empty_qtable()],
                max_bits, min_value, max_value, line_width, lossless: true,
            })
        } else {
            // Lossy: qt[0] is the main table (re-initialized per line),
            // qt[1..3] are fixed sub-tables for small gradients.
            let (_, max_bits) = make_main_qtable(max_value, 0);

            // Sub-table 1: q_base=0
            let mut qp = [0i32; 5];
            qp[0] = 0; qp[4] = max_value;
            qp[1] = if max_value >= 0x12 { 0x12 } else { 1 };
            qp[2] = if max_value >= 0x43 { 0x43 } else { qp[1] };
            qp[3] = if max_value >= 0x114 { 0x114 } else { qp[2] };
            let qt1 = FujiQTable {
                q_table: setup_qlut(max_value, &qp),
                q_base: 0, max_grad: 5, q_grad_mult: 3,
                total_values: max_value + 1,
                raw_bits: log2ceil(max_value + 1),
            };

            // Sub-table 2: q_base=1
            qp[0] = 1;
            qp[1] = if max_value >= 0x15 { 0x15 } else { 2 };
            qp[2] = if max_value >= 0x48 { 0x48 } else { qp[1] };
            qp[3] = if max_value >= 0x11B { 0x11B } else { qp[2] };
            let tv2 = (max_value + 2) / 3 + 1;
            let qt2 = FujiQTable {
                q_table: setup_qlut(max_value, &qp),
                q_base: 1, max_grad: 6, q_grad_mult: 3,
                total_values: tv2,
                raw_bits: log2ceil(tv2),
            };

            // Sub-table 3: q_base=2
            qp[0] = 2;
            qp[1] = if max_value >= 0x18 { 0x18 } else { 3 };
            qp[2] = if max_value >= 0x4D { 0x4D } else { qp[1] };
            qp[3] = if max_value >= 0x122 { 0x122 } else { qp[2] };
            let tv3 = (max_value + 4) / 5 + 1;
            let qt3 = FujiQTable {
                q_table: setup_qlut(max_value, &qp),
                q_base: 2, max_grad: 7, q_grad_mult: 3,
                total_values: tv3,
                raw_bits: log2ceil(tv3),
            };

            Ok(FujiParams {
                qt: [empty_qtable(), qt1, qt2, qt3],
                max_bits, min_value, max_value, line_width, lossless: false,
            })
        }
    }

    fn reinit_main_qtable(&mut self, q_base: i32) {
        let (qt0, max_bits) = make_main_qtable(self.max_value, q_base);
        self.qt[0] = qt0;
        self.max_bits = max_bits;
    }

    #[inline(always)]
    fn quant_gradient(&self, qt: &FujiQTable, v1: i32, v2: i32) -> i32 {
        let idx1 = (self.max_value + v1) as usize;
        let idx2 = (self.max_value + v2) as usize;
        qt.q_grad_mult * (qt.q_table[idx1] as i32) + (qt.q_table[idx2] as i32)
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

// Gradient arrays for one row-group: main grads + 3 lossy sub-grad arrays
#[derive(Clone)]
struct FujiGrads {
    grads: [GradPair; 41],
    lossy_grads: [[GradPair; 5]; 3],
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

// Select which q-table and grad array to use for even samples (lossy mode)
#[inline(always)]
fn select_qtable_even<'a, 'b>(
    params: &'a FujiParams,
    fg: &'b mut FujiGrads,
    diff_sum: i32,
) -> (&'a FujiQTable, &'b mut [GradPair]) {
    for i in 1..4 {
        if params.qt[0].q_base >= i as i32 && diff_sum <= params.qt[i].max_grad {
            return (&params.qt[i], &mut fg.lossy_grads[i - 1]);
        }
    }
    (&params.qt[0], &mut fg.grads)
}

// Select which q-table and grad array to use for odd samples (lossy mode)
#[inline(always)]
fn select_qtable_odd<'a, 'b>(
    params: &'a FujiParams,
    fg: &'b mut FujiGrads,
    diff_sum: i32,
) -> (&'a FujiQTable, &'b mut [GradPair]) {
    for i in 1..4 {
        if params.qt[0].q_base >= i as i32 && diff_sum <= params.qt[i].max_grad {
            return (&params.qt[i], &mut fg.lossy_grads[i - 1]);
        }
    }
    (&params.qt[0], &mut fg.grads)
}

#[inline(always)]
fn fuji_decode_sample(
    pump: &mut BitPumpMSB,
    params: &FujiParams,
    qt: &FujiQTable,
    grad: i32,
    interp_val: i32,
    grads: &mut [GradPair],
) -> u16 {
    let gradient = grad.unsigned_abs() as usize;
    let sample_bits = fuji_zerobits(pump);

    let (code_bits, code_delta): (i32, i32);
    if sample_bits < params.max_bits - qt.raw_bits - 1 {
        code_bits = bit_diff(grads[gradient].value1, grads[gradient].value2);
        code_delta = sample_bits << code_bits;
    } else {
        code_bits = qt.raw_bits;
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

    let q_mult = 2 * qt.q_base + 1;

    // Apply code to interpolation value
    let mut result = if grad < 0 { interp_val - code * q_mult } else { interp_val + code * q_mult };

    if result < -qt.q_base {
        result += qt.total_values * q_mult;
    } else if result > qt.q_base + params.max_value {
        result -= qt.total_values * q_mult;
    }

    result.max(0).min(params.max_value) as u16
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
// Even pixel neighbors. Returns (interp_val, grad, diff_rf_rb, diff_rc_rb)
#[inline(always)]
fn even_neighbors(
    lines: &[i32], stride: usize, c: usize, col: usize,
) -> (i32, i32, i32, i32, i32) {
    let rb = l(lines, stride, c - 1, 1 + 2 * col);
    let rc = l(lines, stride, c - 1, 2 * col);
    let rd = l(lines, stride, c - 1, 2 * col + 2);
    let rf = l(lines, stride, c - 2, 1 + 2 * col);

    let d_rc_rb = (rc - rb).abs();
    let d_rf_rb = (rf - rb).abs();
    let d_rd_rb = (rd - rb).abs();

    let (term1, term2);
    if d_rc_rb > d_rf_rb.max(d_rd_rb) {
        term1 = rf; term2 = rd;
    } else {
        term1 = if d_rd_rb > d_rc_rb.max(d_rf_rb) { rf } else { rd };
        term2 = rc;
    }

    let interp_val = (2 * rb + term1 + term2) >> 2;
    // v1 = Rb - Rf, v2 = Rc - Rb, diff_sum = |Rf-Rb| + |Rc-Rb|
    (interp_val, rb - rf, rc - rb, d_rf_rb, d_rc_rb)
}

// Odd pixel neighbors. Returns (interp_val, v1, v2, diff_sum)
#[inline(always)]
fn odd_neighbors(
    lines: &[i32], stride: usize, c: usize, col: usize,
) -> (i32, i32, i32, i32) {
    let ra = l(lines, stride, c, 1 + 2 * col);
    let rb = l(lines, stride, c - 1, 1 + 2 * col + 1);
    let rc = l(lines, stride, c - 1, 1 + 2 * col);
    let rd = l(lines, stride, c - 1, 1 + 2 * (col + 1));
    let rg = l(lines, stride, c, 1 + 2 * (col + 1));

    let mut interp_val = ra + rg;
    if rb < rc.min(rd) || rb > rc.max(rd) {
        interp_val += 2 * rb;
        interp_val >>= 1;
    }
    interp_val >>= 1;

    // v1 = Rb - Rc, v2 = Rc - Ra, diff_sum = |Rb-Rc| + |Rc-Ra|
    let diff_rb_rc = (rb - rc).abs();
    let diff_rc_ra = (rc - ra).abs();
    (interp_val, rb - rc, rc - ra, diff_rb_rc + diff_rc_ra)
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
    grad_even: &mut [FujiGrads; 3],
    grad_odd: &mut [FujiGrads; 3],
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
                        let (interp_val, _, _, _, _) = even_neighbors(lines, stride, c, col);
                        interp_val.max(0).min(params.max_value) as u16
                    } else {
                        let (interp_val, v1, v2, d_rf_rb, d_rc_rb) = even_neighbors(lines, stride, c, col);
                        if params.lossless {
                            let grad = params.quant_gradient(&params.qt[0], v1, v2);
                            fuji_decode_sample(pump, params, &params.qt[0], grad, interp_val, &mut grad_even[grad_idx].grads)
                        } else {
                            let diff_sum = d_rf_rb + d_rc_rb;
                            let (qt, grads) = select_qtable_even(params, &mut grad_even[grad_idx], diff_sum);
                            let grad = params.quant_gradient(qt, v1, v2);
                            fuji_decode_sample(pump, params, qt, grad, interp_val, grads)
                        }
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

                    let sample = if params.lossless {
                        let (interp_val, v1, v2, _) = odd_neighbors(lines, stride, c, col);
                        let grad = params.quant_gradient(&params.qt[0], v1, v2);
                        fuji_decode_sample(pump, params, &params.qt[0], grad, interp_val, &mut grad_odd[grad_idx].grads)
                    } else {
                        let (interp_val, v1, v2, diff_sum) = odd_neighbors(lines, stride, c, col);
                        let (qt, grads) = select_qtable_odd(params, &mut grad_odd[grad_idx], diff_sum);
                        let grad = params.quant_gradient(qt, v1, v2);
                        fuji_decode_sample(pump, params, qt, grad, interp_val, grads)
                    };

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
// Initialize main gradient arrays
// ---------------------------------------------------------------------------
fn init_main_grads(params: &FujiParams, grad_even: &mut [FujiGrads; 3], grad_odd: &mut [FujiGrads; 3]) {
    let max_diff = 2.max((params.qt[0].total_values + 0x20) >> 6);
    let init = GradPair { value1: max_diff, value2: 1 };
    for j in 0..3 {
        grad_even[j].grads = [init; 41];
        grad_odd[j].grads = [init; 41];
    }
}

fn init_lossy_grads(params: &FujiParams, grad_even: &mut [FujiGrads; 3], grad_odd: &mut [FujiGrads; 3]) {
    for k in 0..3 {
        let max_diff = 2.max((params.qt[k + 1].total_values + 0x20) >> 6);
        let init = GradPair { value1: max_diff, value2: 1 };
        for j in 0..3 {
            grad_even[j].lossy_grads[k] = [init; 5];
            grad_odd[j].lossy_grads[k] = [init; 5];
        }
    }
}

// ---------------------------------------------------------------------------
// Strip decoder
// ---------------------------------------------------------------------------
fn decode_strip(
    src: &[u8],
    header: &FujiHeader,
    params: &mut FujiParams,
    q_bases: Option<&[u8]>,
    strip_width: usize,
    strip_offset_x: usize,
    out: &mut [u16],
    out_width: usize,
    out_height: usize,
) {
    let stride = params.line_width + 2;
    let mut lines = vec![0i32; LTOTAL * stride];

    let init_gp = GradPair { value1: 0, value2: 0 };
    let init_fg = FujiGrads { grads: [init_gp; 41], lossy_grads: [[init_gp; 5]; 3] };
    let mut grad_even = [init_fg.clone(), init_fg.clone(), init_fg.clone()];
    let mut grad_odd = [init_fg.clone(), init_fg.clone(), init_fg.clone()];

    if params.lossless {
        init_main_grads(params, &mut grad_even, &mut grad_odd);
    } else {
        init_lossy_grads(params, &mut grad_even, &mut grad_odd);
    }

    // Pad source for safe BitPump reads near end
    let mut padded = src.to_vec();
    padded.extend_from_slice(&[0u8; 16]);
    let mut pump = BitPumpMSB::new(&padded);

    for cur_line in 0..header.total_lines {
        // For lossy: re-init main qtable and grads when q_base changes
        if !params.lossless {
            let q_base = q_bases.map_or(0, |qb| qb[cur_line] as i32);
            if cur_line == 0 || q_base != params.qt[0].q_base {
                params.reinit_main_qtable(q_base);
                init_main_grads(params, &mut grad_even, &mut grad_odd);
            }
        }

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

    if signature != 0x4953 {
        return Err(format!("RAF compressed: bad header sig=0x{:04x}", signature));
    }
    let lossless = match version {
        1 => true,
        0 => false,
        _ => return Err(format!("RAF compressed: unknown version={}", version)),
    };
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
        raw_bits, raw_height, raw_width, block_size, blocks_in_row, total_lines, lossless,
    };
    let mut params = FujiParams::new(&header)?;

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

    // Compute offset after block sizes (with alignment padding)
    let raw_offset = 4 * blocks_in_row;
    let padding = if raw_offset & 0xF != 0 { 0x10 - (raw_offset & 0xF) } else { 0 };
    let mut data_off = 16 + raw_offset + padding;

    // Read q_bases for lossy
    let q_bases: Option<Vec<u8>> = if !lossless {
        let line_step = (total_lines + 0xF) & !0xF;
        let total_q_bases = blocks_in_row * line_step;
        if data_off + total_q_bases > src.len() {
            return Err("RAF compressed: data too short for q_bases".to_string());
        }
        let qb = src[data_off..data_off + total_q_bases].to_vec();
        data_off += total_q_bases;
        Some(qb)
    } else {
        None
    };

    // Allocate output
    let mut out = vec![0u16; width * height];

    let line_step = (total_lines + 0xF) & !0xF;

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

        let strip_q_bases = q_bases.as_ref().map(|qb| &qb[block * line_step..]);

        decode_strip(strip_src, &header, &mut params, strip_q_bases, strip_width, strip_offset_x, &mut out, width, height);

        data_off = strip_end;
    }

    Ok(out)
}
