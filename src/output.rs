//! A minimal Parquet writer for the two tables: one row group, one PLAIN data
//! page (v1) per column, no compression, no dictionary, no statistics. The
//! footer is written in Thrift's compact protocol by hand.

use std::fs::File;
use std::io::Write;
use std::path::Path;

/// Parquet physical types.
const INT32: i32 = 1;
const INT64: i32 = 2;
const BYTE_ARRAY: i32 = 6;
/// Encodings.
const PLAIN: i32 = 0;
const RLE: i32 = 3;

pub enum Logical {
    None,
    String,
    Date,
}

/// One column, already PLAIN-encoded.
pub struct Column {
    pub name: &'static str,
    physical: i32,
    logical: Logical,
    optional: bool,
    rows: usize,
    /// Definition levels (one per row) for an optional column.
    present: Vec<bool>,
    data: Vec<u8>,
}

impl Column {
    pub fn strings(name: &'static str, rows: usize) -> Self {
        Self::new(name, BYTE_ARRAY, Logical::String, false, rows)
    }
    pub fn optional_strings(name: &'static str, rows: usize) -> Self {
        Self::new(name, BYTE_ARRAY, Logical::String, true, rows)
    }
    pub fn dates(name: &'static str, rows: usize) -> Self {
        Self::new(name, INT32, Logical::Date, false, rows)
    }
    pub fn int64s(name: &'static str, rows: usize) -> Self {
        Self::new(name, INT64, Logical::None, false, rows)
    }
    pub fn int32s(name: &'static str, rows: usize) -> Self {
        Self::new(name, INT32, Logical::None, false, rows)
    }

    /// An integer column of `values`: INT32 when every value fits (smaller files;
    /// any integer width is accepted), else INT64.
    pub fn integers(name: &'static str, values: &[i64]) -> Self {
        let narrow = values.iter().all(|&v| v == v as i32 as i64);
        let mut c = if narrow { Self::int32s(name, values.len()) } else { Self::int64s(name, values.len()) };
        for &v in values {
            if narrow {
                c.push_i32(v as i32);
            } else {
                c.push_i64(v);
            }
        }
        c
    }

    fn new(name: &'static str, physical: i32, logical: Logical, optional: bool, rows: usize) -> Self {
        let width = match physical {
            INT32 => 4,
            INT64 => 8,
            _ => 12,
        };
        Self {
            name,
            physical,
            logical,
            optional,
            rows: 0,
            present: Vec::with_capacity(if optional { rows } else { 0 }),
            data: Vec::with_capacity(rows * width),
        }
    }

    #[inline]
    pub fn push_str(&mut self, value: &[u8]) {
        self.data.extend_from_slice(&(value.len() as u32).to_le_bytes());
        self.data.extend_from_slice(value);
        self.rows += 1;
        if self.optional {
            self.present.push(true);
        }
    }

    /// Appends a string written by `f` into the buffer (no intermediate allocation).
    #[inline]
    pub fn push_str_with(&mut self, f: impl FnOnce(&mut Vec<u8>)) {
        let at = self.data.len();
        self.data.extend_from_slice(&[0; 4]);
        f(&mut self.data);
        let len = (self.data.len() - at - 4) as u32;
        self.data[at..at + 4].copy_from_slice(&len.to_le_bytes());
        self.rows += 1;
        if self.optional {
            self.present.push(true);
        }
    }

    #[inline]
    pub fn push_null(&mut self) {
        debug_assert!(self.optional);
        self.rows += 1;
        self.present.push(false);
    }

    #[inline]
    pub fn push_i32(&mut self, value: i32) {
        self.data.extend_from_slice(&value.to_le_bytes());
        self.rows += 1;
    }

    #[inline]
    pub fn push_i64(&mut self, value: i64) {
        self.data.extend_from_slice(&value.to_le_bytes());
        self.rows += 1;
    }

    /// The definition levels of an optional column, with their 4-byte length
    /// (data page v1); empty for a required column. The values follow them.
    fn levels(&self) -> Vec<u8> {
        let mut out = Vec::new();
        if self.optional {
            // RLE/bit-packed hybrid, bit width 1, as one bit-packed run, after a
            // 4-byte length (data page v1).
            let groups = self.rows.div_ceil(8);
            let mut levels = Vec::with_capacity(groups + 10);
            varint(&mut levels, ((groups as u64) << 1) | 1);
            for chunk in self.present.chunks(8) {
                let mut byte = 0u8;
                for (i, &p) in chunk.iter().enumerate() {
                    byte |= (p as u8) << i;
                }
                levels.push(byte);
            }
            if groups == 0 {
                levels.clear();
            }
            out.extend_from_slice(&(levels.len() as u32).to_le_bytes());
            out.extend_from_slice(&levels);
        }
        out
    }
}

fn varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

/// Thrift compact protocol, just what the footer and page headers need.
struct Thrift {
    out: Vec<u8>,
    last: Vec<i16>,
}

const T_I32: u8 = 5;
const T_I64: u8 = 6;
const T_BINARY: u8 = 8;
const T_LIST: u8 = 9;
const T_STRUCT: u8 = 12;

impl Thrift {
    fn new() -> Self {
        Self { out: Vec::new(), last: vec![0] }
    }
    fn field(&mut self, id: i16, kind: u8) {
        let last = self.last.last_mut().unwrap();
        let delta = id - *last;
        if (1..=15).contains(&delta) {
            self.out.push(((delta as u8) << 4) | kind);
        } else {
            self.out.push(kind);
            varint(&mut self.out, zigzag(id as i64));
        }
        *last = id;
    }
    fn i32(&mut self, id: i16, v: i32) {
        self.field(id, T_I32);
        varint(&mut self.out, zigzag(v as i64));
    }
    fn i64(&mut self, id: i16, v: i64) {
        self.field(id, T_I64);
        varint(&mut self.out, zigzag(v));
    }
    fn string(&mut self, id: i16, v: &str) {
        self.field(id, T_BINARY);
        varint(&mut self.out, v.len() as u64);
        self.out.extend_from_slice(v.as_bytes());
    }
    fn begin_struct(&mut self, id: i16) {
        self.field(id, T_STRUCT);
        self.last.push(0);
    }
    /// A struct as a list element (no field header).
    fn begin_element(&mut self) {
        self.last.push(0);
    }
    fn end_struct(&mut self) {
        self.out.push(0);
        self.last.pop();
    }
    fn list(&mut self, id: i16, kind: u8, len: usize) {
        self.field(id, T_LIST);
        if len < 15 {
            self.out.push(((len as u8) << 4) | kind);
        } else {
            self.out.push(0xF0 | kind);
            varint(&mut self.out, len as u64);
        }
    }
    fn i32_element(&mut self, v: i32) {
        varint(&mut self.out, zigzag(v as i64));
    }
    fn string_element(&mut self, v: &str) {
        varint(&mut self.out, v.len() as u64);
        self.out.extend_from_slice(v.as_bytes());
    }
}

fn page_header(rows: usize, size: usize) -> Vec<u8> {
    let mut t = Thrift::new();
    t.i32(1, 0); // type: DATA_PAGE
    t.i32(2, size as i32); // uncompressed_page_size
    t.i32(3, size as i32); // compressed_page_size
    t.begin_struct(5); // data_page_header
    t.i32(1, rows as i32); // num_values
    t.i32(2, PLAIN); // encoding
    t.i32(3, RLE); // definition_level_encoding
    t.i32(4, RLE); // repetition_level_encoding
    t.end_struct();
    t.out.push(0);
    t.out
}

/// Writes `columns` (all with the same row count) as one Parquet file.
pub fn write(path: &Path, columns: &[Column]) -> std::io::Result<()> {
    let rows = columns.first().map_or(0, |c| c.rows);
    assert!(columns.iter().all(|c| c.rows == rows), "columns differ in length");
    // Written piece by piece straight from the column buffers (no copy of the data).
    let mut file = std::io::BufWriter::with_capacity(1 << 16, File::create(path)?);
    let mut at = 0usize;
    fn put(file: &mut std::io::BufWriter<File>, at: &mut usize, bytes: &[u8]) -> std::io::Result<()> {
        *at += bytes.len();
        if bytes.len() >= 1 << 16 {
            file.flush()?;
            file.get_mut().write_all(bytes)
        } else {
            file.write_all(bytes)
        }
    }
    put(&mut file, &mut at, b"PAR1")?;
    let mut chunks = Vec::new(); // (offset, total size)
    for c in columns {
        let levels = c.levels();
        let page_len = levels.len() + c.data.len();
        assert!(page_len < i32::MAX as usize, "page too large");
        let header = page_header(rows, page_len);
        let offset = at;
        put(&mut file, &mut at, &header)?;
        put(&mut file, &mut at, &levels)?;
        put(&mut file, &mut at, &c.data)?;
        chunks.push((offset, header.len() + page_len));
    }

    let mut t = Thrift::new();
    t.i32(1, 1); // version
    t.list(2, T_STRUCT, columns.len() + 1); // schema
    t.begin_element();
    t.string(4, "schema");
    t.i32(5, columns.len() as i32); // num_children
    t.end_struct();
    for c in columns {
        t.begin_element();
        t.i32(1, c.physical);
        t.i32(3, if c.optional { 1 } else { 0 }); // repetition_type
        t.string(4, c.name);
        match c.logical {
            Logical::None => {}
            Logical::String => {
                t.i32(6, 0); // converted_type UTF8
                t.begin_struct(10); // logicalType
                t.begin_struct(1); // STRING
                t.end_struct();
                t.end_struct();
            }
            Logical::Date => {
                t.i32(6, 6); // converted_type DATE
                t.begin_struct(10);
                t.begin_struct(6); // DATE
                t.end_struct();
                t.end_struct();
            }
        }
        t.end_struct();
    }
    t.i64(3, rows as i64); // num_rows
    t.list(4, T_STRUCT, 1); // row_groups
    t.begin_element();
    t.list(1, T_STRUCT, columns.len()); // columns
    let mut total = 0i64;
    for (c, &(offset, size)) in columns.iter().zip(&chunks) {
        t.begin_element();
        t.i64(2, offset as i64); // file_offset
        t.begin_struct(3); // meta_data
        t.i32(1, c.physical);
        t.list(2, T_I32, 2); // encodings
        t.i32_element(PLAIN);
        t.i32_element(RLE);
        t.list(3, T_BINARY, 1); // path_in_schema
        t.string_element(c.name);
        t.i32(4, 0); // codec UNCOMPRESSED
        t.i64(5, rows as i64); // num_values
        t.i64(6, size as i64); // total_uncompressed_size
        t.i64(7, size as i64); // total_compressed_size
        t.i64(9, offset as i64); // data_page_offset
        t.end_struct();
        t.end_struct();
        total += size as i64;
    }
    t.i64(2, total); // total_byte_size
    t.i64(3, rows as i64); // num_rows
    t.end_struct();
    t.out.push(0); // end of FileMetaData

    put(&mut file, &mut at, &t.out)?;
    put(&mut file, &mut at, &(t.out.len() as u32).to_le_bytes())?;
    put(&mut file, &mut at, b"PAR1")?;
    file.flush()
}
