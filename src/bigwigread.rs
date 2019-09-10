use std::io::{self, Read, Seek, SeekFrom};
use std::io::{BufReader};
use std::fs::File;
use std::vec::Vec;

use byteordered::{ByteOrdered, Endianness};
use flate2::read::ZlibDecoder;

use crate::seekableread::{Reopen, SeekableRead, ReopenableFile};
use crate::bbiread::{BBIRead, BBIFileReadInfoError, BBIFileInfo, Block, ChromAndSize, read_info};
use crate::bigwig::{BBIFile, Value};


fn get_vals<R: Reopen<S>, S: SeekableRead + 'static>(bigwig: &mut BigWigRead<R, S>, current_block: Block, known_offset: &mut u64, start: u32, end: u32) -> io::Result<Box<dyn Iterator<Item=Value> + Send>> {
    let endianness = bigwig.info.header.endianness;
    let uncompress_buf_size: usize = bigwig.info.header.uncompress_buf_size as usize;
    let file = bigwig.reader.as_mut().unwrap();

    // TODO: Could minimize this by chunking block reads
    if *known_offset != current_block.offset {
        match file.seek(SeekFrom::Start(current_block.offset)) {
            Ok(_) => {},
            Err(e) => return Err(e),
        }
    }
    let block_vals = match get_block_values(file, &current_block, endianness, uncompress_buf_size, start, end) {
        Ok(vals) => vals,
        Err(e) => return Err(e),
    };
    *known_offset = current_block.offset + current_block.size;
    Ok(Box::new(block_vals))
}

struct IntervalIter<'a, I, R, S> where I: Iterator<Item=Block> + Send, R: Reopen<S>, S: SeekableRead {
    bigwig: &'a mut BigWigRead<R, S>,
    known_offset: u64,
    blocks: I,
    vals: Option<Box<dyn Iterator<Item=Value> + Send + 'a>>,
    start: u32,
    end: u32,
}

impl<'a, I, R, S: 'static> Iterator for IntervalIter<'a, I, R, S> where I: Iterator<Item=Block> + Send, R: Reopen<S>, S: SeekableRead {
    type Item = io::Result<Value>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            match &mut self.vals {
                Some(vals) => {
                    match vals.next() {
                        Some(v) => { return Some(Ok(v)); }
                        None => { self.vals = None; }
                    }
                },
                None => {
                    // TODO: Could minimize this by chunking block reads
                    let current_block = self.blocks.next()?;
                    match get_vals(self.bigwig, current_block, &mut self.known_offset, self.start, self.end) {
                        Ok(vals) => { self.vals = Some(vals); }
                        Err(e) => { return Some(Err(e)); }
                    }
                },
            }
        }
    }
}

/// Same as IntervalIter but owned
struct OwnedIntervalIter<I, R, S> where I: Iterator<Item=Block> + Send, R: Reopen<S>, S: SeekableRead {
    bigwig: BigWigRead<R, S>,
    known_offset: u64,
    blocks: I,
    vals: Option<Box<dyn Iterator<Item=Value> + Send>>,
    start: u32,
    end: u32,
}

impl<I, R, S: 'static> Iterator for OwnedIntervalIter<I, R, S> where I: Iterator<Item=Block> + Send, R: Reopen<S>, S: SeekableRead {
    type Item = io::Result<Value>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            match &mut self.vals {
                Some(vals) => {
                    match vals.next() {
                        Some(v) => { return Some(Ok(v)); }
                        None => { self.vals = None; }
                    }
                },
                None => {
                    // TODO: Could minimize this by chunking block reads
                    let current_block = self.blocks.next()?;
                    match get_vals(&mut self.bigwig, current_block, &mut self.known_offset, self.start, self.end) {
                        Ok(vals) => { self.vals = Some(vals); }
                        Err(e) => { return Some(Err(e)); }
                    }
                },
            }
        }
    }
}

#[derive(Debug)]
pub enum BigWigReadAttachError {
    NotABigWig,
    InvalidChroms,
    IoError(io::Error),
}

impl From<io::Error> for BigWigReadAttachError {
    fn from(error: io::Error) -> Self {
        BigWigReadAttachError::IoError(error)
    }
}

impl From<BBIFileReadInfoError> for BigWigReadAttachError {
    fn from(error: BBIFileReadInfoError) -> Self {
        return match error {
            BBIFileReadInfoError::UnknownMagic => BigWigReadAttachError::NotABigWig,
            BBIFileReadInfoError::InvalidChroms => BigWigReadAttachError::InvalidChroms,
            BBIFileReadInfoError::IoError(e) => BigWigReadAttachError::IoError(e),
        }
    }
}

pub struct BigWigRead<R, S> where R:Reopen<S>, S: SeekableRead {
    pub info: BBIFileInfo,
    reopen: R,
    reader: Option<ByteOrdered<BufReader<S>, Endianness>>,
}


impl<R, S> Clone for BigWigRead<R, S> where R: Reopen<S>, S: SeekableRead {
    fn clone(&self) -> Self {
        BigWigRead {
            info: self.info.clone(),
            reopen: self.reopen.clone(),
            reader: None,
        }
    }
}


impl<R: Reopen<S>, S: SeekableRead> BBIRead<S> for BigWigRead<R, S> {
    fn get_info(&self) -> &BBIFileInfo {
        &self.info
    }

    fn ensure_reader(&mut self) -> io::Result<&mut ByteOrdered<BufReader<S>, Endianness>> {
        if self.reader.is_none() {
            let endianness = self.info.header.endianness;
            let fp = self.reopen.reopen()?;
            let file = ByteOrdered::runtime(BufReader::new(fp), endianness);
            self.reader.replace(file);
        }
        Ok(self.reader.as_mut().unwrap())
    }

    fn close(&mut self) {
        self.reader.take();
    }

    fn get_chroms(&self) -> Vec<ChromAndSize> {
        self.info.chrom_info.iter().map(|c| ChromAndSize { name: c.name.clone(), length: c.length }).collect::<Vec<_>>()
    }
}

impl BigWigRead<ReopenableFile, File> {
    pub fn from_file_and_attach(path: String) -> Result<Self, BigWigReadAttachError> {
        let reopen = ReopenableFile { path: path.clone() };
        let b = BigWigRead::from(reopen);
        if b.is_err() {
            eprintln!("Error when opening: {}", path);
        }
        b
    }
}

impl<R,S> BigWigRead<R,S> where R: Reopen<S>, S: SeekableRead {
    pub fn from(reopen: R) -> Result<Self, BigWigReadAttachError> {
        let fp = reopen.reopen()?;
        let file = BufReader::new(fp);
        let info = match read_info(file) {
            Err(e) => {
                return Err(e.into());
            }
            Ok(info) => info,
        };
        match info.filetype {
            BBIFile::BigWig => {},
            _ => return Err(BigWigReadAttachError::NotABigWig),
        }

        Ok(BigWigRead {
            info,
            reopen,
            reader: None,
        })
    }
}

impl<R: Reopen<S> + 'static, S: SeekableRead + 'static> BigWigRead<R, S> {
    pub fn get_interval_move(mut self, chrom_name: &str, start: u32, end: u32) -> io::Result<impl Iterator<Item=io::Result<Value>> + Send> {
        let blocks = self.get_overlapping_blocks(chrom_name, start, end)?;
        Ok(OwnedIntervalIter {
            bigwig: self,
            known_offset: 0,
            blocks: blocks.into_iter(),
            vals: None,
            start,
            end,
        })
    }

    pub fn get_interval<'a>(&'a mut self, chrom_name: &str, start: u32, end: u32) -> io::Result<impl Iterator<Item=io::Result<Value>> + Send + 'a> {
        let blocks = self.get_overlapping_blocks(chrom_name, start, end)?;
        Ok(IntervalIter {
            bigwig: self,
            known_offset: 0,
            blocks: blocks.into_iter(),
            vals: None,
            start,
            end,
        })
    }
}

/// This assumes that the file is currently at the block's start
fn get_block_values<S: SeekableRead>(file: &mut ByteOrdered<BufReader<S>, Endianness>, block: &Block, endianness: Endianness, uncompress_buf_size: usize, start: u32, end: u32) -> io::Result<impl Iterator<Item=Value>> {
    let mut values: Vec<Value> = Vec::new();

    let mut raw_data = vec![0u8; block.size as usize];
    file.read_exact(&mut raw_data)?;
    let block_data: Vec<u8> = if uncompress_buf_size > 0 {
        let mut uncompressed_block_data = vec![0u8; uncompress_buf_size];
        let mut d = ZlibDecoder::new(&raw_data[..]);
        let _ = d.read(&mut uncompressed_block_data)?;
        uncompressed_block_data
    } else {
        raw_data
    };

    let mut block_data_mut = ByteOrdered::runtime(&block_data[..], endianness);
    let _chrom_id = block_data_mut.read_u32()?;
    let chrom_start = block_data_mut.read_u32()?;
    let _chrom_end = block_data_mut.read_u32()?;
    let item_step = block_data_mut.read_u32()?;
    let item_span = block_data_mut.read_u32()?;
    let section_type = block_data_mut.read_u8()?;
    let _reserved = block_data_mut.read_u8()?;
    let item_count = block_data_mut.read_u16()?;

    let mut curr_start = chrom_start;
    for _ in 0..item_count {
        let mut value = match section_type {
            1 => {
                // bedgraph
                let chrom_start = block_data_mut.read_u32()?;
                let chrom_end = block_data_mut.read_u32()?;
                let value = block_data_mut.read_f32()?;
                Value {
                    start: chrom_start,
                    end: chrom_end,
                    value,
                }
            },
            2 => {
                // variable step
                let chrom_start = block_data_mut.read_u32()?;
                let chrom_end = chrom_start + item_span;
                let value = block_data_mut.read_f32()?;
                Value {
                    start: chrom_start,
                    end: chrom_end,
                    value,
                }
            },
            3 => {
                // fixed step
                let chrom_start = curr_start;
                curr_start += item_step;
                let chrom_end = chrom_start + item_span;
                let value = block_data_mut.read_f32()?;
                Value {
                    start: chrom_start,
                    end: chrom_end,
                    value,
                }
            },
            _ => return Err(std::io::Error::new(std::io::ErrorKind::Other, format!("Unknown bigwig section type: {}", section_type)))
        };
        if value.end >= start && value.start <= end {
            value.start = value.start.max(start);
            value.end = value.end.min(end);
            values.push(value)
        }
    }

    Ok(values.into_iter())
}