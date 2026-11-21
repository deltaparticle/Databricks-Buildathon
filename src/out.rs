//! Parquet output: one uncompressed row group per file.

use std::fs::File;
use std::io::BufWriter;
use std::path::Path;
use std::sync::Arc;

use bytes::Bytes;
use parquet::basic::Compression;
use parquet::data_type::{ByteArray, ByteArrayType, Int32Type, Int64Type};
use parquet::file::properties::{EnabledStatistics, WriterProperties};
use parquet::file::writer::SerializedFileWriter;
use parquet::schema::parser::parse_message_type;

pub enum Col {
    /// Required strings: concatenated bytes and end offsets.
    Str(Vec<u8>, Vec<u32>),
    /// Optional strings: bytes and ends of present values, a def level per row.
    OptStr(Vec<u8>, Vec<u32>, Vec<i16>),
    I32(Vec<i32>),
    I64(Vec<i64>),
}

fn byte_arrays(buf: Vec<u8>, ends: &[u32]) -> Vec<ByteArray> {
    let all = Bytes::from(buf);
    let mut out = Vec::with_capacity(ends.len());
    let mut start = 0usize;
    for &e in ends {
        out.push(ByteArray::from(all.slice(start..e as usize)));
        start = e as usize;
    }
    out
}

pub fn write(path: &Path, schema: &str, cols: Vec<Col>) -> parquet::errors::Result<()> {
    let schema = Arc::new(parse_message_type(schema)?);
    let props = Arc::new(
        WriterProperties::builder()
            .set_compression(Compression::UNCOMPRESSED)
            .set_dictionary_enabled(false)
            .set_statistics_enabled(EnabledStatistics::None)
            .set_max_row_group_size(usize::MAX)
            .set_data_page_row_count_limit(1 << 20)
            .set_write_batch_size(1 << 16)
            .build(),
    );
    let file = BufWriter::with_capacity(1 << 22, File::create(path)?);
    let mut writer = SerializedFileWriter::new(file, schema, props)?;
    let mut rg = writer.next_row_group()?;
    let mut cols = cols.into_iter();
    while let Some(mut cw) = rg.next_column()? {
        match cols.next().expect("column count") {
            Col::Str(buf, ends) => {
                let v = byte_arrays(buf, &ends);
                cw.typed::<ByteArrayType>().write_batch(&v, None, None)?;
            }
            Col::OptStr(buf, ends, defs) => {
                let v = byte_arrays(buf, &ends);
                cw.typed::<ByteArrayType>().write_batch(&v, Some(&defs), None)?;
            }
            Col::I32(v) => {
                cw.typed::<Int32Type>().write_batch(&v, None, None)?;
            }
            Col::I64(v) => {
                cw.typed::<Int64Type>().write_batch(&v, None, None)?;
            }
        }
        cw.close()?;
    }
    rg.close()?;
    writer.close()?;
    Ok(())
}

pub const USER_SCHEMA: &str = "message user_daily {
  required binary user_id (STRING);
  required int32 day (DATE);
  required int64 sessions;
  required int64 events;
  required int64 revenue_cents;
  optional binary top_category (STRING);
}";

pub const CAMPAIGN_SCHEMA: &str = "message campaign_daily {
  required binary campaign_id (STRING);
  required binary channel (STRING);
  required int32 day (DATE);
  required int64 sessions;
  required int64 purchasing_sessions;
  required int64 revenue_cents;
  required int64 assisted_sessions;
}";
