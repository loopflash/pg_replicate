pub mod cdc_event;
pub mod table_row;

#[derive(Debug)]
pub enum Cell {
    Null,
    Bool(bool),
    String(String),
    I16(i16),
    I32(i32),
    I64(i64),
    TimeStamp(String),
    TimeStampTz(String),
    Bytes(Vec<u8>),
}
