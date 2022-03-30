#[allow(unused)]
#[allow(unused_imports)]

/// The encoding module converts the internal data structures to and from a lossless compact binary
/// data format.
///
/// This is modelled after the run-length encoding in Automerge and Yjs.

// Notes for next time I break compatibility:
// - Version in encode::write_local_version - skip second 0 if its ROOT.

mod varint;
mod encode_oplog;
mod decode_oplog;
#[cfg(test)]
mod fuzzer;

use std::error::Error;
use std::fmt::{Debug, Display, Formatter};
use std::marker::PhantomData;
use std::mem::{replace, size_of};
use rle::{HasLength, MergableSpan, SplitableSpan};
use crate::list::encoding::varint::*;
use num_enum::TryFromPrimitive;
pub use encode_oplog::{EncodeOptions, ENCODE_FULL, ENCODE_PATCH};
use crate::list::remote_ids::ConversionError;

#[cfg(feature = "serde")]
use serde_crate::Serialize;

const MAGIC_BYTES: [u8; 8] = *b"DMNDTYPS";

#[derive(Debug, Eq, PartialEq, Clone, Copy)]
#[non_exhaustive]
#[cfg_attr(feature = "serde", derive(Serialize), serde(crate="serde_crate"))]
pub enum ParseError {
    InvalidMagic,
    UnsupportedProtocolVersion,
    DocIdMismatch,
    BaseVersionUnknown,
    UnknownChunk,
    UnknownCompressionFormat(u32),
    LZ4DecompressionError, // I'd wrap it but lz4_flex errors don't implement any traits
    // LZ4DecompressionError(lz4_flex::block::DecompressError),
    CompressedDataMissing,
    InvalidChunkHeader,
    MissingChunk(u32),
    // UnexpectedChunk {
    //     // I could use Chunk here, but I'd rather not expose them publicly.
    //     // expected: Chunk,
    //     // actual: Chunk,
    //     expected: u32,
    //     actual: u32,
    // },
    InvalidLength,
    UnexpectedEOF,
    // TODO: Consider elidiing the details here to keep the wasm binary small.
    // InvalidUTF8(Utf8Error),
    InvalidUTF8,
    InvalidRemoteID(ConversionError),
    InvalidVarInt,
    InvalidContent,

    ChecksumFailed,

    /// This error is interesting. We're loading a chunk but missing some of the data. In the future
    /// I'd like to explicitly support this case, and allow the oplog to contain a somewhat- sparse
    /// set of data, and load more as needed.
    DataMissing,
}

impl Display for ParseError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "ParseError {:?}", self)
    }
}

impl Error for ParseError {}

const PROTOCOL_VERSION: usize = 0;

fn push_u32(into: &mut Vec<u8>, val: u32) {
    let mut buf = [0u8; 5];
    let pos = encode_u32(val, &mut buf);
    into.extend_from_slice(&buf[..pos]);
}

fn push_u64(into: &mut Vec<u8>, val: u64) {
    let mut buf = [0u8; 10];
    let pos = encode_u64(val, &mut buf);
    into.extend_from_slice(&buf[..pos]);
}

fn push_usize(into: &mut Vec<u8>, val: usize) {
    if size_of::<usize>() <= size_of::<u32>() {
        push_u32(into, val as u32);
    } else if size_of::<usize>() == size_of::<u64>() {
        push_u64(into, val as u64);
    } else {
        panic!("usize larger than u64 is not supported");
    }
}

fn push_str(into: &mut Vec<u8>, val: &str) {
    let bytes = val.as_bytes();
    push_usize(into, bytes.len());
    into.extend_from_slice(bytes);
}

fn push_u32_le(into: &mut Vec<u8>, val: u32) {
    // This is used for the checksum. Using LE because varint is LE.
    let bytes = val.to_le_bytes();
    into.extend_from_slice(&bytes);
}

fn checksum(data: &[u8]) -> u32 {
    // This is crc32c. Using the crc library because the resulting binary size is much smaller.
    // let checksum = crc32c::crc32c(&result);
    crc::Crc::<u32>::new(&crc::CRC_32_ISCSI).checksum(data)
}

// #[derive(Debug, PartialEq, Eq, Copy, Clone)]
#[derive(Debug, PartialEq, Eq, Copy, Clone, TryFromPrimitive)]
#[repr(u32)]
enum ChunkType {
    /// Packed bytes storing any data compressed in later parts of the file.
    CompressedFieldsLZ4 = 5,

    /// FileInfo contains optional UserData and AgentNames.
    FileInfo = 1,
    DocId = 2,
    AgentNames = 3,
    UserData = 4,

    /// The StartBranch chunk describes the state of the document before included patches have been
    /// applied.
    StartBranch = 10,
    Version = 12,
    /// StartBranch content is optional.
    Content = 13,
    ContentCompressed = 14, // Might make more sense to have a generic compression tag for chunks.

    Patches = 20,
    OpVersions = 21,
    OpTypeAndPosition = 22,
    OpParents = 23,

    PatchContent = 24,
    /// ContentKnown is a RLE expressing which ranges of patches have known content
    ContentIsKnown = 25,

    TransformedPositions = 27, // Currently unused

    Crc = 100,
}

#[derive(Debug, PartialEq, Eq, Copy, Clone, TryFromPrimitive)]
#[repr(u32)]
enum DataType {
    Bool = 1,
    VarUInt = 2,
    VarInt = 3,
    PlainText = 4,
}

#[derive(Debug, PartialEq, Eq, Copy, Clone, TryFromPrimitive)]
#[repr(u32)]
enum CompressionFormat {
    // Just for future proofing, ya know?
    LZ4 = 1,
}

fn push_chunk_header(into: &mut Vec<u8>, chunk_type: ChunkType, len: usize) {
    push_u32(into, chunk_type as u32);
    push_usize(into, len);
}

fn push_chunk(into: &mut Vec<u8>, chunk_type: ChunkType, data: &[u8]) {
    push_chunk_header(into, chunk_type, data.len());
    into.extend_from_slice(data);
}

#[derive(Clone)]
struct Merger<S: MergableSpan, F: FnMut(S, &mut Ctx), Ctx = ()> {
    last: Option<S>,
    f: F,
    _ctx: PhantomData<Ctx> // This is awful.
}

impl<S: MergableSpan, F: FnMut(S, &mut Ctx), Ctx> Merger<S, F, Ctx> {
    pub fn new(f: F) -> Self {
        Self { last: None, f, _ctx: PhantomData }
    }

    pub fn push2(&mut self, span: S, ctx: &mut Ctx) {
        if let Some(last) = self.last.as_mut() {
            if last.can_append(&span) {
                last.append(span);
            } else {
                let old = replace(last, span);
                (self.f)(old, ctx);
            }
        } else {
            self.last = Some(span);
        }
    }

    pub fn flush2(mut self, ctx: &mut Ctx) {
        if let Some(span) = self.last.take() {
            (self.f)(span, ctx);
        }
    }
}

// I hate this.
impl<S: MergableSpan, F: FnMut(S, &mut ())> Merger<S, F, ()> {
    pub fn push(&mut self, span: S) {
        self.push2(span, &mut ());
    }
    pub fn flush(self) {
        self.flush2(&mut ());
    }
}

impl<S: MergableSpan, F: FnMut(S, &mut Ctx), Ctx> Drop for Merger<S, F, Ctx> {
    fn drop(&mut self) {
        if self.last.is_some() && !std::thread::panicking() {
            panic!("Merger dropped with unprocessed data");
        }
    }
}
