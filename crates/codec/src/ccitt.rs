//! CCITTFaxDecode: ITU-T T.4 (Group 3, one- and two-dimensional) and T.6
//! (Group 4) decoding, plus a Group 4 encoder for building image streams.
//!
//! The decoder follows the row/EOL state machine of xpdf's `CCITTFaxStream`
//! (the ancestor of pdf.js's decoder) for the `EncodedByteAlign`,
//! `EndOfLine`, and `EndOfBlock` corner cases, and MuPDF's conventions for
//! damaged and missing rows. Independent MuPDF renders serve as the oracle.

use std::sync::OnceLock;

use crate::bits::{BitReader, BitWriter};
use crate::error::{CodecError, Result, check_dimensions};
use crate::image::BilevelImage;

const CODEC: &str = "ccitt";

/// The `/DecodeParms` of a `/CCITTFaxDecode` filter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CcittParams {
    /// `/K`: negative = pure two-dimensional (Group 4), zero = pure
    /// one-dimensional (Group 3 MH), positive = mixed (Group 3 MR, each
    /// EOL is followed by a tag bit selecting 1-D or 2-D for the next row).
    pub k: i32,
    /// `/Columns` (default 1728).
    pub columns: u32,
    /// `/Rows`; 0 means unknown, decode until end of data or EOFB/RTC.
    pub rows: u32,
    /// `/EncodedByteAlign`.
    pub encoded_byte_align: bool,
    /// `/EndOfLine`: EOL codes are present before each row.
    pub end_of_line: bool,
    /// `/EndOfBlock` (default true): the data ends with EOFB/RTC rather than
    /// after `rows` rows.
    pub end_of_block: bool,
    /// `/BlackIs1`: emit 1 bits for black pixels instead of 0 bits.
    pub black_is_1: bool,
    /// `/DamagedRowsBeforeError`: damaged rows tolerated before decoding stops.
    pub damaged_rows_before_error: u32,
}

impl Default for CcittParams {
    fn default() -> Self {
        Self {
            k: 0,
            columns: 1728,
            rows: 0,
            encoded_byte_align: false,
            end_of_line: false,
            end_of_block: true,
            black_is_1: false,
            damaged_rows_before_error: 0,
        }
    }
}

/// Result of [`decode_ccitt`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CcittImage {
    /// Packed rows in the PDF sample convention (see [`decode_ccitt`]).
    pub image: BilevelImage,
    /// Rows recovered from the data, damaged ones included; less than
    /// `image.height` when the data ended early or too many rows were damaged.
    pub rows_decoded: u32,
    /// Rows that hit an invalid code; their remainder is white.
    pub damaged_rows: u32,
}

/// Decodes a CCITT stream to packed 1-bit rows.
///
/// Output follows the PDF sample convention: with `black_is_1` false (the
/// default) black pixels are 0 bits, otherwise 1 bits; rows are padded to a
/// byte. When `rows` is non-zero the image has exactly that many rows and
/// rows missing from the data are zero bytes (black unless `black_is_1`),
/// which is how MuPDF pads a truncated image stream. When `rows` is zero
/// the height is the number of rows found.
///
/// A row that hits an invalid code keeps the pixels decoded so far and is
/// white for the rest. Decoding stops once more than
/// `damaged_rows_before_error` rows are damaged, returning what was decoded.
/// An error is returned only when the parameters are invalid or no row could
/// be started.
pub fn decode_ccitt(data: &[u8], params: &CcittParams) -> Result<CcittImage> {
    if params.columns == 0 || params.columns > crate::error::MAX_DIMENSION {
        return Err(CodecError::invalid(
            CODEC,
            format!("Columns {}", params.columns),
        ));
    }
    if params.rows > 0 {
        check_dimensions(CODEC, params.columns, params.rows)?;
    }
    let columns = params.columns;
    let stride = (columns as usize).div_ceil(8);
    let white_byte = if params.black_is_1 { 0x00 } else { 0xFF };
    let n = columns as usize;

    let mut dec = RowDecoder {
        r: BitReader::new(data),
        columns,
        coding: vec![columns; n + 2],
        coding_pos: 0,
        reference: vec![columns; n + 3],
        err: false,
        tables: tables(),
    };

    let mut out: Vec<u8> = Vec::new();
    let mut rows_done: u32 = 0;
    let mut damaged: u32 = 0;
    let mut next_2d = params.k < 0;

    // Leading fill and an optional first EOL (with its tag bit when K > 0).
    while !dec.r.at_end() && dec.r.peek(12) == 0 {
        dec.r.skip(1);
    }
    if dec.r.peek(12) == 1 {
        dec.r.skip(12);
    }
    if params.k > 0 {
        next_2d = dec.r.read(1) == 0;
    }

    'rows: loop {
        if params.rows > 0 && rows_done >= params.rows {
            break;
        }
        if dec.r.at_end() {
            break;
        }
        dec.err = false;
        let status = if next_2d {
            dec.decode_2d_row()
        } else {
            dec.decode_1d_row()
        };
        let started = dec.coding_pos > 0 || dec.coding[0] > 0;
        let (stop, emit) = match status {
            RowStatus::Complete => (false, true),
            RowStatus::Eof => (true, started),
            RowStatus::Damaged => {
                damaged += 1;
                (damaged > params.damaged_rows_before_error, started)
            }
        };
        if emit {
            dec.finish_row();
            out.resize(out.len() + stride, white_byte);
            let row = out.len() - stride;
            dec.paint_row(&mut out[row..], params.black_is_1);
            rows_done += 1;
        }
        if stop {
            break;
        }

        // Between rows: EOL, byte alignment, tag bit, EOFB/RTC, resync.
        if !params.end_of_block && params.rows > 0 && rows_done >= params.rows {
            break;
        }
        let mut got_eol = false;
        if params.end_of_line || !params.encoded_byte_align {
            // Skip fill bits (any bits when EndOfLine promises an EOL).
            while !dec.r.at_end() {
                let code = dec.r.peek(12);
                if code == 1 || (!params.end_of_line && code != 0) {
                    break;
                }
                dec.r.skip(1);
            }
            if dec.r.peek(12) == 1 {
                dec.r.skip(12);
                got_eol = true;
            }
        }
        if params.encoded_byte_align && !got_eol {
            dec.r.align_byte();
            // No row starts with eleven zero bits, so an EOL here is real.
            if dec.r.peek(12) == 1 {
                dec.r.skip(12);
                got_eol = true;
            }
        }
        if dec.r.at_end() {
            break;
        }
        if params.k > 0 {
            next_2d = dec.r.read(1) == 0;
        }
        if params.end_of_block && got_eol {
            if dec.r.peek(12) == 1 {
                // EOFB (two EOLs) or RTC (six EOLs, each with a tag when K > 0).
                break;
            }
        } else if dec.err && params.end_of_line {
            // Resynchronise on the next EOL after a damaged row.
            loop {
                if dec.r.at_end() {
                    break 'rows;
                }
                let code = dec.r.peek(13);
                if code >> 1 == 1 {
                    dec.r.skip(12);
                    if params.k > 0 {
                        dec.r.skip(1);
                        next_2d = code & 1 == 0;
                    }
                    break;
                }
                dec.r.skip(1);
            }
        }
    }

    if rows_done == 0 {
        return Err(CodecError::malformed(CODEC, "no rows decoded"));
    }
    let height = if params.rows > 0 {
        params.rows
    } else {
        rows_done
    };
    out.resize(stride * height as usize, 0x00);
    let mut image = BilevelImage {
        width: columns,
        height,
        data: out,
    };
    image.clear_padding();
    Ok(CcittImage {
        image,
        rows_decoded: rows_done,
        damaged_rows: damaged,
    })
}

enum RowStatus {
    Complete,
    /// The data ended inside the row.
    Eof,
    /// An invalid code was met; the rest of the row is white.
    Damaged,
}

enum Mode {
    Pass,
    Horizontal,
    Vertical(i32),
    /// An EOL code: the row ends here (short rows are padded white).
    Eol,
    Eof,
    Bad,
}

struct RowDecoder<'a> {
    r: BitReader<'a>,
    columns: u32,
    /// Changing elements of the row being decoded; `coding[i]` is where run
    /// `i` ends (even = white run, odd = black run).
    coding: Vec<u32>,
    coding_pos: usize,
    /// Changing elements of the previous row, terminated by `columns`.
    reference: Vec<u32>,
    err: bool,
    tables: &'static Tables,
}

impl RowDecoder<'_> {
    fn add_pixels(&mut self, a1: u32, black: bool) {
        let mut a1 = a1;
        if a1 > self.coding[self.coding_pos] {
            if a1 > self.columns {
                self.err = true;
                a1 = self.columns;
            }
            if (self.coding_pos & 1 == 1) != black {
                self.coding_pos += 1;
            }
            self.coding[self.coding_pos] = a1;
        }
    }

    fn add_pixels_neg(&mut self, a1: i64, black: bool) {
        let cur = i64::from(self.coding[self.coding_pos]);
        if a1 > cur {
            self.add_pixels(a1.min(i64::from(self.columns)) as u32, black);
            if a1 > i64::from(self.columns) {
                self.err = true;
            }
        } else if a1 < cur {
            let a1 = if a1 < 0 {
                self.err = true;
                0
            } else {
                a1 as u32
            };
            while self.coding_pos > 0 && a1 < self.coding[self.coding_pos - 1] {
                self.coding_pos -= 1;
            }
            self.coding[self.coding_pos] = a1;
        }
    }

    /// Ends a row early: the remainder is white.
    fn finish_row(&mut self) {
        let columns = self.columns;
        if self.coding[self.coding_pos] < columns {
            self.add_pixels(columns, false);
        }
    }

    /// True when the remaining bits are fewer than a code and all zero.
    fn at_padding(&self) -> bool {
        self.r.bits_left() < 12 && self.r.peek(12) == 0
    }

    fn mode(&mut self) -> Mode {
        let bits = self.r.peek(7);
        let (len, mode) = match bits {
            0b1000000..=0b1111111 => (1, Mode::Vertical(0)),
            0b0110000..=0b0111111 => (3, Mode::Vertical(1)),
            0b0100000..=0b0101111 => (3, Mode::Vertical(-1)),
            0b0010000..=0b0011111 => (3, Mode::Horizontal),
            0b0001000..=0b0001111 => (4, Mode::Pass),
            0b0000110..=0b0000111 => (6, Mode::Vertical(2)),
            0b0000100..=0b0000101 => (6, Mode::Vertical(-2)),
            0b0000011 => (7, Mode::Vertical(3)),
            0b0000010 => (7, Mode::Vertical(-3)),
            _ => {
                // 0000001 = extension (uncompressed mode, unsupported);
                // 0000000 = EOL, fill, or end of data.
                return if self.r.peek(12) == 1 {
                    Mode::Eol
                } else if self.at_padding() {
                    Mode::Eof
                } else {
                    Mode::Bad
                };
            }
        };
        self.r.skip(len);
        mode
    }

    /// One run-length code of the given colour, or `None` for an invalid code.
    fn run_code(&mut self, black: bool) -> Option<u32> {
        let (lut, bits) = if black {
            (&self.tables.black_lut[..], 13)
        } else {
            (&self.tables.white_lut[..], 12)
        };
        let entry = lut[self.r.peek(bits) as usize];
        if entry.len == 0 {
            return None;
        }
        self.r.skip(u32::from(entry.len));
        Some(u32::from(entry.run))
    }

    /// A full run: makeup codes followed by a terminating code.
    fn run(&mut self, black: bool) -> std::result::Result<u32, RowStatus> {
        let mut total = 0u32;
        loop {
            match self.run_code(black) {
                Some(run) => {
                    total = total.saturating_add(run);
                    if run < 64 {
                        return Ok(total);
                    }
                }
                None => {
                    if total == 0 && self.r.peek(12) == 1 {
                        // EOL in place of a run: the row ends here.
                        return Err(RowStatus::Complete);
                    }
                    self.err = true;
                    return Err(if self.at_padding() {
                        RowStatus::Eof
                    } else {
                        RowStatus::Damaged
                    });
                }
            }
        }
    }

    fn decode_1d_row(&mut self) -> RowStatus {
        let columns = self.columns;
        self.coding[0] = 0;
        self.coding_pos = 0;
        let mut black = false;
        while self.coding[self.coding_pos] < columns {
            let run = match self.run(black) {
                Ok(run) => run,
                Err(status) => return status,
            };
            let a0 = self.coding[self.coding_pos];
            self.add_pixels(a0.saturating_add(run), black);
            black = !black;
        }
        if self.err {
            RowStatus::Damaged
        } else {
            RowStatus::Complete
        }
    }

    fn decode_2d_row(&mut self) -> RowStatus {
        let columns = self.columns;
        // The previous coding line becomes the reference line.
        let mut i = 0;
        while self.coding[i] < columns {
            self.reference[i] = self.coding[i];
            i += 1;
        }
        self.reference[i] = columns;
        self.reference[i + 1] = columns;
        self.coding[0] = 0;
        self.coding_pos = 0;
        let mut ref_pos = 0usize;
        let mut black = false;

        while self.coding[self.coding_pos] < columns {
            if ref_pos + 1 >= self.reference.len() {
                self.err = true;
                return RowStatus::Damaged;
            }
            match self.mode() {
                Mode::Pass => {
                    let b2 = self.reference[ref_pos + 1];
                    self.add_pixels(b2, black);
                    if b2 < columns {
                        ref_pos += 2;
                    }
                }
                Mode::Horizontal => {
                    let run1 = match self.run(black) {
                        Ok(run) => run,
                        Err(status) => return status,
                    };
                    let run2 = match self.run(!black) {
                        Ok(run) => run,
                        Err(status) => return status,
                    };
                    let a0 = self.coding[self.coding_pos];
                    self.add_pixels(a0.saturating_add(run1), black);
                    if self.coding[self.coding_pos] < columns {
                        let a1 = self.coding[self.coding_pos];
                        self.add_pixels(a1.saturating_add(run2), !black);
                    }
                    while self.reference[ref_pos] <= self.coding[self.coding_pos]
                        && self.reference[ref_pos] < columns
                    {
                        ref_pos += 2;
                    }
                }
                Mode::Vertical(delta) => {
                    let b1 = i64::from(self.reference[ref_pos]);
                    if delta >= 0 {
                        self.add_pixels((b1 + i64::from(delta)) as u32, black);
                    } else {
                        self.add_pixels_neg(b1 + i64::from(delta), black);
                    }
                    black = !black;
                    if self.coding[self.coding_pos] < columns {
                        if delta >= 0 {
                            ref_pos += 1;
                        } else if ref_pos > 0 {
                            ref_pos -= 1;
                        } else {
                            ref_pos += 1;
                        }
                        while self.reference[ref_pos] <= self.coding[self.coding_pos]
                            && self.reference[ref_pos] < columns
                        {
                            ref_pos += 2;
                        }
                    }
                }
                Mode::Eol => return RowStatus::Complete,
                Mode::Eof => {
                    self.err = true;
                    return RowStatus::Eof;
                }
                Mode::Bad => {
                    self.err = true;
                    return RowStatus::Damaged;
                }
            }
        }
        if self.err {
            RowStatus::Damaged
        } else {
            RowStatus::Complete
        }
    }

    /// Paints the black runs of the coding line into a row prefilled white.
    fn paint_row(&self, row: &mut [u8], black_is_1: bool) {
        let columns = self.columns;
        let mut start = 0u32;
        for (i, &end) in self.coding[..=self.coding_pos].iter().enumerate() {
            let end = end.min(columns);
            if i % 2 == 1 && end > start {
                fill_bits(row, start, end, black_is_1);
            }
            start = start.max(end);
        }
    }
}

/// Sets (`set` true) or clears bits `[from, to)` of a packed row.
fn fill_bits(row: &mut [u8], from: u32, to: u32, set: bool) {
    let mut x = from;
    while x < to {
        let byte = (x / 8) as usize;
        let bit = x % 8;
        let span = (8 - bit).min(to - x);
        let mask = (0xFFu8 >> bit) & (0xFFu8 << (8 - bit - span));
        if set {
            row[byte] |= mask;
        } else {
            row[byte] &= !mask;
        }
        x += span;
    }
}

#[derive(Clone, Copy, Default)]
struct LutEntry {
    len: u8,
    run: u16,
}

struct Tables {
    white_lut: Vec<LutEntry>,
    black_lut: Vec<LutEntry>,
    /// Terminating (0..=63) then makeup (64..=2560 step 64) codes: (len, code).
    white_codes: Vec<(u8, u16)>,
    black_codes: Vec<(u8, u16)>,
}

fn tables() -> &'static Tables {
    static TABLES: OnceLock<Tables> = OnceLock::new();
    TABLES.get_or_init(|| {
        let build = |codes: &[&str], bits: u32| {
            let mut lut = vec![LutEntry::default(); 1 << bits];
            let mut list = Vec::with_capacity(codes.len());
            for (idx, code) in codes.iter().enumerate() {
                let len = code.len() as u32;
                let value = u32::from_str_radix(code, 2).unwrap_or(0);
                let run = run_for_index(idx);
                list.push((len as u8, value as u16));
                let base = (value << (bits - len)) as usize;
                for entry in &mut lut[base..base + (1 << (bits - len))] {
                    *entry = LutEntry {
                        len: len as u8,
                        run,
                    };
                }
            }
            (lut, list)
        };
        let (white_lut, white_codes) = build(&WHITE_CODES, 12);
        let (black_lut, black_codes) = build(&BLACK_CODES, 13);
        Tables {
            white_lut,
            black_lut,
            white_codes,
            black_codes,
        }
    })
}

/// Run length of table index `idx`: 0..=63 terminating, then 64, 128, ... 2560.
fn run_for_index(idx: usize) -> u16 {
    if idx < 64 {
        idx as u16
    } else {
        ((idx - 63) * 64) as u16
    }
}

/// T.4 white run-length codes: terminating 0..=63, makeup 64..=1728, then
/// the extended makeup codes 1792..=2560 shared by both colours.
const WHITE_CODES: [&str; 104] = [
    "00110101",
    "000111",
    "0111",
    "1000",
    "1011",
    "1100",
    "1110",
    "1111",
    "10011",
    "10100",
    "00111",
    "01000",
    "001000",
    "000011",
    "110100",
    "110101",
    "101010",
    "101011",
    "0100111",
    "0001100",
    "0001000",
    "0010111",
    "0000011",
    "0000100",
    "0101000",
    "0101011",
    "0010011",
    "0100100",
    "0011000",
    "00000010",
    "00000011",
    "00011010",
    "00011011",
    "00010010",
    "00010011",
    "00010100",
    "00010101",
    "00010110",
    "00010111",
    "00101000",
    "00101001",
    "00101010",
    "00101011",
    "00101100",
    "00101101",
    "00000100",
    "00000101",
    "00001010",
    "00001011",
    "01010010",
    "01010011",
    "01010100",
    "01010101",
    "00100100",
    "00100101",
    "01011000",
    "01011001",
    "01011010",
    "01011011",
    "01001010",
    "01001011",
    "00110010",
    "00110011",
    "00110100",
    // makeup 64..=1728
    "11011",
    "10010",
    "010111",
    "0110111",
    "00110110",
    "00110111",
    "01100100",
    "01100101",
    "01101000",
    "01100111",
    "011001100",
    "011001101",
    "011010010",
    "011010011",
    "011010100",
    "011010101",
    "011010110",
    "011010111",
    "011011000",
    "011011001",
    "011011010",
    "011011011",
    "010011000",
    "010011001",
    "010011010",
    "011000",
    "010011011",
    // extended makeup 1792..=2560
    "00000001000",
    "00000001100",
    "00000001101",
    "000000010010",
    "000000010011",
    "000000010100",
    "000000010101",
    "000000010110",
    "000000010111",
    "000000011100",
    "000000011101",
    "000000011110",
    "000000011111",
];

/// T.4 black run-length codes in the same order as [`WHITE_CODES`].
const BLACK_CODES: [&str; 104] = [
    "0000110111",
    "010",
    "11",
    "10",
    "011",
    "0011",
    "0010",
    "00011",
    "000101",
    "000100",
    "0000100",
    "0000101",
    "0000111",
    "00000100",
    "00000111",
    "000011000",
    "0000010111",
    "0000011000",
    "0000001000",
    "00001100111",
    "00001101000",
    "00001101100",
    "00000110111",
    "00000101000",
    "00000010111",
    "00000011000",
    "000011001010",
    "000011001011",
    "000011001100",
    "000011001101",
    "000001101000",
    "000001101001",
    "000001101010",
    "000001101011",
    "000011010010",
    "000011010011",
    "000011010100",
    "000011010101",
    "000011010110",
    "000011010111",
    "000001101100",
    "000001101101",
    "000011011010",
    "000011011011",
    "000001010100",
    "000001010101",
    "000001010110",
    "000001010111",
    "000001100100",
    "000001100101",
    "000001010010",
    "000001010011",
    "000000100100",
    "000000110111",
    "000000111000",
    "000000100111",
    "000000101000",
    "000001011000",
    "000001011001",
    "000000101011",
    "000000101100",
    "000001011010",
    "000001100110",
    "000001100111",
    // makeup 64..=1728
    "0000001111",
    "000011001000",
    "000011001001",
    "000001011011",
    "000000110011",
    "000000110100",
    "000000110101",
    "0000001101100",
    "0000001101101",
    "0000001001010",
    "0000001001011",
    "0000001001100",
    "0000001001101",
    "0000001110010",
    "0000001110011",
    "0000001110100",
    "0000001110101",
    "0000001110110",
    "0000001110111",
    "0000001010010",
    "0000001010011",
    "0000001010100",
    "0000001010101",
    "0000001011010",
    "0000001011011",
    "0000001100100",
    "0000001100101",
    // extended makeup 1792..=2560
    "00000001000",
    "00000001100",
    "00000001101",
    "000000010010",
    "000000010011",
    "000000010100",
    "000000010101",
    "000000010110",
    "000000010111",
    "000000011100",
    "000000011101",
    "000000011110",
    "000000011111",
];

// ---------------------------------------------------------------------------
// Encoder
// ---------------------------------------------------------------------------

/// Changing elements of a packed row: positions where the colour differs from
/// the pixel to the left (the pixel left of column 0 is white), followed by
/// two `width` sentinels. Even indices change to black, odd to white.
fn changing_elements(row: &[u8], width: u32, one_is_black: bool, out: &mut Vec<u32>) {
    out.clear();
    let mut color = false;
    for x in 0..width {
        let bit = (row[(x / 8) as usize] >> (7 - x % 8)) & 1 == 1;
        let black = bit == one_is_black;
        if black != color {
            out.push(x);
            color = black;
        }
    }
    out.push(width);
    out.push(width);
}

/// Writes one run of `len` pixels in the given colour.
fn write_run(w: &mut BitWriter, tables: &Tables, mut len: u32, black: bool) {
    let codes = if black {
        &tables.black_codes
    } else {
        &tables.white_codes
    };
    while len >= 2560 {
        let (l, c) = codes[63 + 40];
        w.write(u32::from(c), u32::from(l));
        len -= 2560;
    }
    if len >= 64 {
        let (l, c) = codes[63 + (len / 64) as usize];
        w.write(u32::from(c), u32::from(l));
        len %= 64;
    }
    let (l, c) = codes[len as usize];
    w.write(u32::from(c), u32::from(l));
}

/// Encodes one row one-dimensionally (T.4 4.1): alternating white and black
/// runs starting with a possibly empty white run.
#[cfg(test)]
fn encode_1d_row(w: &mut BitWriter, tables: &Tables, coding: &[u32], width: u32) {
    let mut start = 0;
    let mut color = false;
    for &end in coding
        .iter()
        .take_while(|&&c| c < width)
        .chain(std::iter::once(&width))
    {
        write_run(w, tables, end - start, color);
        start = end;
        color = !color;
    }
}

/// Encodes one row two-dimensionally against `reference` (T.4 4.2 / T.6).
fn encode_2d_row(
    w: &mut BitWriter,
    tables: &Tables,
    coding: &[u32],
    reference: &[u32],
    width: u32,
) {
    let mut a0: i64 = -1;
    let mut color = false;
    loop {
        // a1: first changing element on the coding line right of a0 with the
        // colour opposite to a0's; a2: the one after it.
        let parity = usize::from(color);
        let mut i = parity;
        while i < coding.len() && i64::from(coding[i]) <= a0 {
            i += 2;
        }
        let a1 = coding.get(i).copied().unwrap_or(width);
        let a2 = coding.get(i + 1).copied().unwrap_or(width);
        // b1: first changing element on the reference line right of a0 with
        // the colour opposite to a0's; b2: the next one.
        let mut j = parity;
        while j < reference.len() && i64::from(reference[j]) <= a0 {
            j += 2;
        }
        let b1 = reference.get(j).copied().unwrap_or(width);
        let b2 = reference.get(j + 1).copied().unwrap_or(width);

        if b2 < a1 {
            w.write(0b0001, 4);
            a0 = i64::from(b2);
        } else {
            let delta = i64::from(a1) - i64::from(b1);
            if (-3..=3).contains(&delta) {
                let (code, len) = match delta {
                    0 => (0b1, 1),
                    1 => (0b011, 3),
                    2 => (0b000011, 6),
                    3 => (0b0000011, 7),
                    -1 => (0b010, 3),
                    -2 => (0b000010, 6),
                    _ => (0b0000010, 7),
                };
                w.write(code, len);
                a0 = i64::from(a1);
                color = !color;
            } else {
                w.write(0b001, 3);
                let start = a0.max(0) as u32;
                write_run(w, tables, a1 - start, color);
                write_run(w, tables, a2 - a1, !color);
                a0 = i64::from(a2);
            }
        }
        if a0 >= i64::from(width) {
            break;
        }
    }
}

/// Encodes packed 1-bit rows as a Group 4 (T.6, `/K -1`) stream ending with
/// EOFB. `one_is_black` states which bit value is black in `data`; the stream
/// decodes back with `black_is_1 = one_is_black` to identical bytes.
/// `data` must hold `height` rows of `ceil(width / 8)` bytes.
pub fn encode_ccitt_g4(
    data: &[u8],
    width: u32,
    height: u32,
    one_is_black: bool,
) -> Result<Vec<u8>> {
    check_dimensions(CODEC, width, height)?;
    crate::image::check_len(CODEC, data, width, height, 1, 1)?;
    let tables = tables();
    let stride = (width as usize).div_ceil(8);
    let mut w = BitWriter::new();
    let mut reference = vec![width, width];
    let mut coding = Vec::with_capacity(width as usize + 2);
    for row in data.chunks_exact(stride) {
        changing_elements(row, width, one_is_black, &mut coding);
        encode_2d_row(&mut w, tables, &coding, &reference, width);
        std::mem::swap(&mut reference, &mut coding);
    }
    w.write(1, 12);
    w.write(1, 12);
    Ok(w.finish())
}

#[cfg(test)]
pub(crate) mod test_encoder {
    //! Group 3 encoder used only to build decoder test vectors.

    use super::*;

    pub(crate) struct G3Options {
        pub k: i32,
        pub end_of_line: bool,
        pub byte_align: bool,
        pub rtc: bool,
    }

    /// Encodes packed rows (1 = black) as a Group 3 stream: `k == 0` is pure
    /// 1-D, `k > 0` writes a 1-D row followed by `k - 1` 2-D rows. With
    /// `end_of_line` every row is preceded by EOL (plus the tag bit when
    /// `k > 0`); with `byte_align` fill bits make the EOL end on a byte
    /// boundary, or without EOLs each row start on one.
    pub(crate) fn encode_g3(data: &[u8], width: u32, height: u32, opts: &G3Options) -> Vec<u8> {
        let tables = tables();
        let stride = (width as usize).div_ceil(8);
        let mut w = BitWriter::new();
        let mut reference = vec![width, width];
        let mut coding = Vec::with_capacity(width as usize + 2);
        let write_eol = |w: &mut BitWriter, tag: Option<bool>| {
            if opts.byte_align {
                let pad = (8 - (w.bit_len() + 12) % 8) % 8;
                w.write(0, pad as u32);
            }
            w.write(1, 12);
            if let Some(one_d) = tag {
                w.write(u32::from(one_d), 1);
            }
        };
        for (row_idx, row) in data.chunks_exact(stride).take(height as usize).enumerate() {
            let one_d = opts.k <= 0 || row_idx % opts.k as usize == 0;
            if opts.end_of_line {
                write_eol(&mut w, (opts.k > 0).then_some(one_d));
            } else {
                if opts.byte_align {
                    w.align();
                }
                if opts.k > 0 {
                    w.write(u32::from(one_d), 1);
                }
            }
            changing_elements(row, width, true, &mut coding);
            if one_d {
                encode_1d_row(&mut w, tables, &coding, width);
            } else {
                encode_2d_row(&mut w, tables, &coding, &reference, width);
            }
            std::mem::swap(&mut reference, &mut coding);
        }
        if opts.rtc {
            for _ in 0..6 {
                write_eol(&mut w, (opts.k > 0).then_some(true));
            }
        }
        w.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::test_encoder::{G3Options, encode_g3};
    use super::*;

    /// 40x12 test pattern (1 = black): a diagonal, a solid block, isolated
    /// pixels at both edges, one all-black row, and all-white rows.
    fn pattern() -> (Vec<u8>, u32, u32) {
        let (w, h) = (40u32, 12u32);
        let stride = (w as usize).div_ceil(8);
        let mut data = vec![0u8; stride * h as usize];
        let mut set =
            |x: u32, y: u32| data[y as usize * stride + (x / 8) as usize] |= 0x80 >> (x % 8);
        for i in 0..8 {
            set(i * 3, i);
        }
        for y in 2..6 {
            for x in 20..33 {
                set(x, y);
            }
        }
        set(0, 8);
        set(39, 8);
        for x in 0..w {
            set(x, 9);
        }
        set(38, 11);
        set(39, 11);
        (data, w, h)
    }

    fn params(k: i32, w: u32, h: u32) -> CcittParams {
        CcittParams {
            k,
            columns: w,
            rows: h,
            black_is_1: true,
            ..CcittParams::default()
        }
    }

    #[test]
    fn g4_round_trip_matches_source_bits() {
        let (data, w, h) = pattern();
        let encoded = encode_ccitt_g4(&data, w, h, true).unwrap();
        let decoded = decode_ccitt(&encoded, &params(-1, w, h)).unwrap();
        assert_eq!(decoded.image.data, data);
        assert_eq!((decoded.rows_decoded, decoded.damaged_rows), (h, 0));
    }

    #[test]
    fn g4_known_answer_for_hand_coded_rows() {
        // Row 0 against an all-white reference: horizontal mode, white 2 then
        // black 3, then V0 to the end (b1 = 8 = columns).
        // 001 + 0111 + 10 + 1 = 0010 1111 01 ; row 1 identical: V0 V0 V0
        // (1 1 1). EOFB follows. Bits: 0010111101 111 000000000001 000000000001
        let bits = "0010111101111000000000001000000000001";
        let mut w = BitWriter::new();
        for c in bits.chars() {
            w.write(u32::from(c == '1'), 1);
        }
        let stream = w.finish();
        let decoded = decode_ccitt(&stream, &params(-1, 8, 2)).unwrap();
        assert_eq!(decoded.image.data, vec![0b0011_1000, 0b0011_1000]);
        // Same stream with BlackIs1 false yields the complement.
        let mut p = params(-1, 8, 2);
        p.black_is_1 = false;
        let decoded = decode_ccitt(&stream, &p).unwrap();
        assert_eq!(decoded.image.data, vec![0b1100_0111, 0b1100_0111]);
    }

    #[test]
    fn g4_rows_unknown_stops_at_eofb() {
        let (data, w, h) = pattern();
        let mut encoded = encode_ccitt_g4(&data, w, h, true).unwrap();
        encoded.extend_from_slice(&[0x55; 8]);
        let decoded = decode_ccitt(&encoded, &params(-1, w, 0)).unwrap();
        assert_eq!(decoded.image.height, h);
        assert_eq!(decoded.image.data, data);
    }

    #[test]
    fn g4_missing_rows_are_zero_bytes_and_counted() {
        let (data, w, h) = pattern();
        let stride = (w as usize).div_ceil(8);
        let encoded = encode_ccitt_g4(&data[..stride * 5], w, 5, true).unwrap();
        let decoded = decode_ccitt(&encoded, &params(-1, w, h)).unwrap();
        assert_eq!(decoded.rows_decoded, 5);
        assert_eq!(&decoded.image.data[..stride * 5], &data[..stride * 5]);
        assert!(decoded.image.data[stride * 5..].iter().all(|&b| b == 0));
        assert_eq!(decoded.image.height, h);
    }

    #[test]
    fn g4_truncated_data_keeps_completed_rows() {
        let (data, w, h) = pattern();
        let stride = (w as usize).div_ceil(8);
        let encoded = encode_ccitt_g4(&data, w, h, true).unwrap();
        let cut = &encoded[..encoded.len() / 2];
        let decoded = decode_ccitt(cut, &params(-1, w, h)).unwrap();
        let full = decoded.rows_decoded as usize - 1;
        assert!(full >= 1 && full < h as usize);
        assert_eq!(&decoded.image.data[..stride * full], &data[..stride * full]);
    }

    macro_rules! g3_round_trip {
        ($name:ident, k = $k:expr, eol = $eol:expr, align = $align:expr, rtc = $rtc:expr) => {
            #[test]
            fn $name() {
                let (data, w, h) = pattern();
                let opts = G3Options {
                    k: $k,
                    end_of_line: $eol,
                    byte_align: $align,
                    rtc: $rtc,
                };
                let encoded = encode_g3(&data, w, h, &opts);
                let mut p = params($k, w, h);
                p.end_of_line = $eol;
                p.encoded_byte_align = $align;
                let decoded = decode_ccitt(&encoded, &p).unwrap();
                assert_eq!(decoded.image.data, data);
                assert_eq!(decoded.damaged_rows, 0);
                // Rows unknown: the row count must come out of the data.
                p.rows = 0;
                let decoded = decode_ccitt(&encoded, &p).unwrap();
                assert_eq!(decoded.image.height, h, "row count with Rows 0");
                assert_eq!(decoded.image.data, data);
            }
        };
    }

    g3_round_trip!(g3_1d_plain, k = 0, eol = false, align = false, rtc = false);
    g3_round_trip!(g3_1d_eol_rtc, k = 0, eol = true, align = false, rtc = true);
    g3_round_trip!(
        g3_1d_aligned_rows,
        k = 0,
        eol = false,
        align = true,
        rtc = true
    );
    g3_round_trip!(
        g3_1d_aligned_eol,
        k = 0,
        eol = true,
        align = true,
        rtc = true
    );
    g3_round_trip!(
        g3_2d_k4_eol_rtc,
        k = 4,
        eol = true,
        align = false,
        rtc = true
    );
    g3_round_trip!(
        g3_2d_k4_tags_only,
        k = 4,
        eol = false,
        align = false,
        rtc = false
    );
    g3_round_trip!(
        g3_2d_k4_aligned_eol,
        k = 4,
        eol = true,
        align = true,
        rtc = true
    );
    g3_round_trip!(
        g3_2d_k2_aligned_rows,
        k = 2,
        eol = false,
        align = true,
        rtc = false
    );

    #[test]
    fn g4_byte_aligned_rows_decode() {
        // EncodedByteAlign with K < 0: every row starts on a byte boundary.
        let (data, w, h) = pattern();
        let tables = tables();
        let stride = (w as usize).div_ceil(8);
        let mut bw = BitWriter::new();
        let mut reference = vec![w, w];
        let mut coding = Vec::new();
        for row in data.chunks_exact(stride) {
            bw.align();
            changing_elements(row, w, true, &mut coding);
            encode_2d_row(&mut bw, tables, &coding, &reference, w);
            std::mem::swap(&mut reference, &mut coding);
        }
        bw.align();
        bw.write(1, 12);
        bw.write(1, 12);
        let encoded = bw.finish();
        let mut p = params(-1, w, 0);
        p.encoded_byte_align = true;
        let decoded = decode_ccitt(&encoded, &p).unwrap();
        assert_eq!(decoded.image.height, h);
        assert_eq!(decoded.image.data, data);
    }

    #[test]
    fn end_of_block_false_stops_after_rows_without_eofb() {
        let (data, w, h) = pattern();
        let stride = (w as usize).div_ceil(8);
        let opts = G3Options {
            k: 0,
            end_of_line: false,
            byte_align: false,
            rtc: false,
        };
        let mut encoded = encode_g3(&data, w, h, &opts);
        encoded.extend_from_slice(&[0xA5; 6]);
        let mut p = params(0, w, h);
        p.end_of_block = false;
        let decoded = decode_ccitt(&encoded, &p).unwrap();
        assert_eq!(decoded.rows_decoded, h);
        assert_eq!(decoded.image.data[..stride * h as usize], data[..]);
    }

    #[test]
    fn damaged_row_stops_by_default_and_resyncs_with_tolerance() {
        let (data, w, h) = pattern();
        let stride = (w as usize).div_ceil(8);
        let opts = G3Options {
            k: 0,
            end_of_line: true,
            byte_align: false,
            rtc: true,
        };
        let mut encoded = encode_g3(&data, w, h, &opts);
        // Corrupt row 3: overwrite the bits after its EOL (the 4th) with a
        // white makeup code for 1792 pixels, which overflows the 40 columns.
        let mut eols = Vec::new();
        let r = BitReader::new(&encoded);
        for pos in 0..encoded.len() * 8 - 12 {
            let mut rr = r.clone();
            rr.skip(pos as u32);
            if rr.peek(12) == 1 {
                eols.push(pos);
            }
        }
        let start = eols[3] + 12;
        for (i, bit) in [0u8, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0].into_iter().enumerate() {
            let pos = start + i;
            let mask = 0x80 >> (pos % 8);
            if bit == 1 {
                encoded[pos / 8] |= mask;
            } else {
                encoded[pos / 8] &= !mask;
            }
        }

        let mut p = params(0, w, h);
        p.end_of_line = true;
        let decoded = decode_ccitt(&encoded, &p).unwrap();
        assert_eq!(decoded.damaged_rows, 1);
        assert_eq!(decoded.rows_decoded, 4, "stops after the damaged row");
        assert_eq!(&decoded.image.data[..stride * 3], &data[..stride * 3]);
        assert!(decoded.image.data[stride * 4..].iter().all(|&b| b == 0));

        p.damaged_rows_before_error = 1;
        let decoded = decode_ccitt(&encoded, &p).unwrap();
        assert_eq!(decoded.damaged_rows, 1);
        assert_eq!(decoded.rows_decoded, h);
        assert_eq!(
            &decoded.image.data[stride * 4..],
            &data[stride * 4..],
            "rows after resync"
        );
    }

    #[test]
    fn invalid_columns_is_an_error() {
        let p = CcittParams {
            columns: 0,
            ..CcittParams::default()
        };
        assert!(matches!(
            decode_ccitt(&[0xFF], &p),
            Err(CodecError::InvalidParams { .. })
        ));
    }

    #[test]
    fn garbage_yields_error_or_partial_never_panics() {
        for seed in 0..64u32 {
            let bytes: Vec<u8> = (0..40u32)
                .map(|i| (seed.wrapping_mul(2654435761).wrapping_add(i * 7919) >> 13) as u8)
                .collect();
            for k in [-1, 0, 2] {
                let p = CcittParams {
                    k,
                    columns: 37,
                    rows: 9,
                    ..CcittParams::default()
                };
                if let Ok(out) = decode_ccitt(&bytes, &p) {
                    assert_eq!(out.image.data.len(), 5 * 9);
                }
            }
        }
    }

    #[test]
    fn encoder_rejects_short_buffer() {
        assert!(matches!(
            encode_ccitt_g4(&[0; 3], 8, 4, true),
            Err(CodecError::InvalidParams { .. })
        ));
    }
}
