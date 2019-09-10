use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::sync::Arc;

use crossbeam::atomic::AtomicCell;

use futures::future::Either;

use crate::bigwig::{BBIWriteOptions, BedEntry, BigBedWrite, ChromGroupRead, ChromGroupReadStreamingIterator, WriteGroupsError};
use crate::idmap::IdMap;
use crate::streaming_linereader::StreamingLineReader;
use crate::chromvalues::{ChromGroups, ChromValues};


pub fn get_chromgroupstreamingiterator<V: 'static, S: StreamingChromValues + std::marker::Send + 'static>(vals: V, options: BBIWriteOptions, chrom_map: HashMap<String, u32>)
    -> impl ChromGroupReadStreamingIterator
    where V : ChromGroups<BedEntry, ChromGroup<S>> + std::marker::Send {
    struct ChromGroupReadStreamingIteratorImpl<S: StreamingChromValues + std::marker::Send, C: ChromGroups<BedEntry, ChromGroup<S>> + std::marker::Send> {
        chrom_groups: C,
        last_chrom: Option<String>,
        chrom_ids: Option<IdMap<String>>,
        pool: futures::executor::ThreadPool,
        options: BBIWriteOptions,
        chrom_map: HashMap<String, u32>,
        _s: std::marker::PhantomData<S>,
    }

    impl<S: StreamingChromValues + std::marker::Send + 'static, C: ChromGroups<BedEntry, ChromGroup<S>> + std::marker::Send> ChromGroupReadStreamingIterator for ChromGroupReadStreamingIteratorImpl<S, C> {
    fn next(&mut self) -> Result<Option<Either<ChromGroupRead, (IdMap<String>)>>, WriteGroupsError> {
            match self.chrom_groups.next()? {
                Some((chrom, group)) => {
                    let chrom_ids = self.chrom_ids.as_mut().unwrap();
                    let last = self.last_chrom.replace(chrom.clone());
                    if let Some(c) = last {
                        // TODO: test this correctly fails
                        if c >= chrom {
                            return Err(WriteGroupsError::InvalidInput("Input bedGraph not sorted by chromosome. Sort with `sort -k1,1 -k2,2n`.".to_string()));
                        }
                    }
                    let length = match self.chrom_map.get(&chrom) {
                        Some(length) => *length,
                        None => return Err(WriteGroupsError::InvalidInput(format!("Input bedGraph contains chromosome that isn't in the input chrom sizes: {}", chrom))),
                    };
                    let chrom_id = chrom_ids.get_id(chrom.clone());
                    let group = BigBedWrite::begin_processing_chrom(chrom, chrom_id, length, group, self.pool.clone(), self.options.clone())?;
                    Ok(Some(Either::Left(group)))
                },
                None => {
                    match self.chrom_ids.take() {
                        Some(chrom_ids) => Ok(Some(Either::Right(chrom_ids))),
                        None => Ok(None),
                    }
                }
            }
        }
    }

    let group_iter = ChromGroupReadStreamingIteratorImpl {
        chrom_groups: vals,
        last_chrom: None,
        chrom_ids: Some(IdMap::new()),
        pool: futures::executor::ThreadPoolBuilder::new().pool_size(6).create().expect("Unable to create thread pool."),
        options: options.clone(),
        chrom_map: chrom_map,
        _s: std::marker::PhantomData,
    };
    group_iter
}

pub trait StreamingChromValues {
    fn next<'a>(&'a mut self) -> io::Result<Option<(&'a str, u32, u32, String)>>;
}

pub struct BedStream<B: BufRead> {
    bed: StreamingLineReader<B>
}

impl<B: BufRead> StreamingChromValues for BedStream<B> {
    fn next<'a>(&'a mut self) -> io::Result<Option<(&'a str, u32, u32, String)>> {
        let l = self.bed.read()?;
        let line = match l {
            Some(line) => line,
            None => return Ok(None),
        };
        let mut split = line.split_whitespace();
        let chrom = match split.next() {
            Some(chrom) => chrom,
            None => {
                return Ok(None);
            },
        };
        let start = split.next().expect("Missing start").parse::<u32>().unwrap();
        let end = split.next().expect("Missing end").parse::<u32>().unwrap();
        let rest_strings: Vec<&str> = split.collect();
        let rest = &rest_strings[..].join("\t");
        Ok(Some((chrom, start, end, rest.to_string())))
    }
}

pub struct BedIteratorStream<I: Iterator<Item=io::Result<(String, u32, u32, String)>>> {
    iter: I,
    curr: Option<(String, u32, u32, String)>,
}

impl<I: Iterator<Item=io::Result<(String, u32, u32, String)>>> StreamingChromValues for BedIteratorStream<I> {
    fn next<'a>(&'a mut self) -> io::Result<Option<(&'a str, u32, u32, String)>> {
        use std::ops::Deref;
        self.curr = match self.iter.next() {
            None => return Ok(None),
            Some(v) => Some(v?),
        };
        Ok(self.curr.as_ref().map(|v| (v.0.deref(), v.1, v.2, v.3.clone())))
    }
}

pub struct BedParser<S: StreamingChromValues>{
    state: Arc<AtomicCell<Option<BedParserState<S>>>>,
}

impl<S: StreamingChromValues> BedParser<S> {
    pub fn new(stream: S) -> BedParser<S> {
        let state = BedParserState {
            stream,
            curr_chrom: None,
            next_chrom: ChromOpt::None,
            curr_val: None,
            next_val: None,
        };
        BedParser {
            state: Arc::new(AtomicCell::new(Some(state))),
        }
    }
}

impl BedParser<BedStream<BufReader<File>>> {
    pub fn from_file(file: File) -> BedParser<BedStream<BufReader<File>>> {
        BedParser::new(BedStream { bed: StreamingLineReader::new(BufReader::new(file)) })
    }
}

impl<I: Iterator<Item=io::Result<(String, u32, u32, String)>>> BedParser<BedIteratorStream<I>> {
    pub fn from_iter(iter: I) -> BedParser<BedIteratorStream<I>> {
        BedParser::new(BedIteratorStream { iter, curr: None })
    }
}

#[derive(Debug)]
enum ChromOpt {
    None,
    Same,
    Diff(String),
}

#[derive(Debug)]
pub struct BedParserState<S: StreamingChromValues> {
    stream: S,
    curr_chrom: Option<String>,
    curr_val: Option<BedEntry>,
    next_chrom: ChromOpt,
    next_val: Option<BedEntry>,
}

impl<S: StreamingChromValues> BedParserState<S> {
    fn advance(&mut self) -> io::Result<()> {
        self.curr_val = self.next_val.take();
        match std::mem::replace(&mut self.next_chrom, ChromOpt::None) {
            ChromOpt::Diff(real_chrom) => {
                self.curr_chrom.replace(real_chrom);
            },
            ChromOpt::Same => {},
            ChromOpt::None => {
                self.curr_chrom = None;
            },
        }

        if let Some((chrom, start, end, rest)) = self.stream.next()? {
            self.next_val.replace(BedEntry { start, end, rest });
            if let Some(curr_chrom) = &self.curr_chrom {
                if curr_chrom != chrom {
                    self.next_chrom = ChromOpt::Diff(chrom.to_owned());
                } else {
                    self.next_chrom = ChromOpt::Same;
                }
            } else {
                self.next_chrom = ChromOpt::Diff(chrom.to_owned());
            }
        }
        if self.curr_val.is_none() && self.next_val.is_some() {
            self.advance()?;
        }
        Ok(())
    }
}

impl<S: StreamingChromValues> ChromGroups<BedEntry, ChromGroup<S>> for BedParser<S> {
    fn next(&mut self) -> io::Result<Option<(String, ChromGroup<S>)>> {
        let mut state = self.state.swap(None).expect("Invalid usage. This iterator does not buffer and all values should be exhausted for a chrom before next() is called.");
        if let ChromOpt::Same = state.next_chrom {
            panic!("Invalid usage. This iterator does not buffer and all values should be exhausted for a chrom before next() is called.");
        }
        state.advance()?;

        let next_chrom = state.curr_chrom.as_ref();
        let ret = match next_chrom {
            None => Ok(None),
            Some(chrom) => {
                let group = ChromGroup { state: self.state.clone(), curr_state: None };
                Ok(Some((chrom.to_owned(), group)))
            },
        };
        self.state.swap(Some(state));
        ret
    }
}

pub struct ChromGroup<S: StreamingChromValues> {
    state: Arc<AtomicCell<Option<BedParserState<S>>>>,
    curr_state: Option<BedParserState<S>>,
}

impl<S: StreamingChromValues> ChromValues<BedEntry> for ChromGroup<S> {
    fn next(&mut self) -> io::Result<Option<BedEntry>> {
        if self.curr_state.is_none() {
            let opt_state = self.state.swap(None);
            if opt_state.is_none() {
                panic!("Invalid usage. This iterator does not buffer and all values should be exhausted for a chrom before next() is called.");
            }
            self.curr_state = opt_state;
        }
        let state = self.curr_state.as_mut().unwrap();
        if let Some(val) = state.curr_val.take() {
            return Ok(Some(val));
        }
        if let ChromOpt::Diff(_) = state.next_chrom {
            return Ok(None);
        }
        state.advance()?;
        Ok(state.curr_val.take())
    }

    fn peek(&mut self) -> Option<&BedEntry> {
        if self.curr_state.is_none() {
            let opt_state = self.state.swap(None);
            if opt_state.is_none() {
                panic!("Invalid usage. This iterator does not buffer and all values should be exhausted for a chrom before next() is called.");
            }
            self.curr_state = opt_state;
        }
        let state = self.curr_state.as_ref().unwrap();
        if let ChromOpt::Diff(_) = state.next_chrom {
            return None;
        }
        return state.next_val.as_ref();
    }
}

impl<S: StreamingChromValues> Drop for ChromGroup<S> {
    fn drop(&mut self) {
        if let Some(state) = self.curr_state.take() {
            self.state.swap(Some(state));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io;
    use std::path::PathBuf;
    extern crate test;

    #[test]
    fn test_works() -> io::Result<()> {
        let mut dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        dir.push("resources/test");
        dir.push("small.bed");
        let f = File::open(dir)?;
        let mut bgp = BedParser::from_file(f);
        {
            let (chrom, mut group) = bgp.next()?.unwrap();
            assert_eq!(chrom, "chr17");
            assert_eq!(BedEntry { start: 1, end: 100, rest: "test1\t0".to_string() }, group.next()?.unwrap());
            assert_eq!(&BedEntry { start: 101, end: 200, rest: "test2\t0".to_string() }, group.peek().unwrap());
            assert_eq!(&BedEntry { start: 101, end: 200, rest: "test2\t0".to_string() }, group.peek().unwrap());

            assert_eq!(BedEntry { start: 101, end: 200, rest: "test2\t0".to_string() }, group.next()?.unwrap());
            assert_eq!(&BedEntry { start: 201, end: 300, rest: "test3\t0".to_string() }, group.peek().unwrap());

            assert_eq!(BedEntry { start: 201, end: 300, rest: "test3\t0".to_string() }, group.next()?.unwrap());
            assert_eq!(None, group.peek());

            assert_eq!(None, group.next()?);
            assert_eq!(None, group.peek());
        }
        {
            let (chrom, mut group) = bgp.next()?.unwrap();
            assert_eq!(chrom, "chr18");
            assert_eq!(BedEntry { start: 1, end: 100, rest: "test4\t0".to_string() }, group.next()?.unwrap());
            assert_eq!(&BedEntry { start: 101, end: 200, rest: "test5\t0".to_string() }, group.peek().unwrap());
            assert_eq!(&BedEntry { start: 101, end: 200, rest: "test5\t0".to_string() }, group.peek().unwrap());

            assert_eq!(BedEntry { start: 101, end: 200, rest: "test5\t0".to_string() }, group.next()?.unwrap());
            assert_eq!(None, group.peek());

            assert_eq!(None, group.next()?);
            assert_eq!(None, group.peek());
        }
        {
            let (chrom, mut group) = bgp.next()?.unwrap();
            assert_eq!(chrom, "chr19");
            assert_eq!(BedEntry { start: 1, end: 100, rest: "test6\t0".to_string() }, group.next()?.unwrap());
            assert_eq!(None, group.peek());

            assert_eq!(None, group.next()?);
            assert_eq!(None, group.peek());
        }
        assert!(bgp.next()?.is_none());
        Ok(())
    }

}