// Author: Jeff
// Date: 2026-09-19
// Description: mg-bookr — ebooks and audiobooks for the Geist suite
// Notes: store keeps the records, tools runs helper programs safely; scanning, reader data,
//        the vault export and the mpv audiobook player arrive slice by slice

pub mod listen;
pub mod meta;
pub mod mpv;
pub mod reader;
pub mod scan;
mod secure_db;
pub mod store;
pub mod terminal_text;
pub mod tools;
pub mod tui;
pub mod vault;
