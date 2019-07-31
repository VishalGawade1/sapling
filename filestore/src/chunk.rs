// Copyright (c) 2019-present, Facebook, Inc.
// All Rights Reserved.
//
// This software may be used and distributed according to the terms of the
// GNU General Public License version 2 or any later version.

use bytes::{Bytes, BytesMut};
use failure_ext::{Error, Result};
use futures::{try_ready, Async, Future, Poll, Stream};
use futures_ext::{BoxFuture, BoxStream, FutureExt, StreamExt};
use std::convert::TryFrom;
use std::fmt::{self, Debug};

use crate::errors::ErrorKind;

#[derive(Debug)]
#[must_use = "streams do nothing unless polled"]
struct ChunkStream<S> {
    stream: S,
    chunk_size: usize,
    buff: BytesMut,
}

impl<S> ChunkStream<S> {
    fn new(stream: S, chunk_size: usize) -> ChunkStream<S> {
        assert!(chunk_size > 0);

        ChunkStream {
            stream,
            chunk_size,
            buff: BytesMut::with_capacity(chunk_size),
        }
    }
}

impl<S> Stream for ChunkStream<S>
where
    S: Stream<Item = Bytes>,
{
    type Item = Bytes;
    type Error = S::Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        loop {
            if self.buff.len() >= self.chunk_size {
                // We've buffered more data than we need. Emit some.
                let chunk = self.buff.split_to(self.chunk_size).freeze();
                return Ok(Async::Ready(Some(chunk)));
            }

            // We need more data. Poll for some!

            if let Some(bytes) = try_ready!(self.stream.poll()) {
                // We got more data. Extend our buffer, then see if that is enough to return. Note
                // that extend_from slice implicitly extends our BytesMut.
                self.buff.extend_from_slice(&bytes);
                continue;
            }

            // No more data is coming. Return whatever we have left.

            let out = if self.buff.len() > 0 {
                // We did have some buffered data. Emit that.
                let chunk = std::mem::replace(&mut self.buff, BytesMut::new()).freeze();
                Async::Ready(Some(chunk))
            } else {
                // We have no more buffered data. We're done.
                Async::Ready(None)
            };

            return Ok(out);
        }
    }
}

pub enum Chunks {
    Inline(BoxFuture<Bytes, Error>),
    Chunked(u64, BoxStream<Bytes, Error>),
}

impl Debug for Chunks {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Chunks::Inline(_) => write!(f, "Chunks::Inline(..)"),
            Chunks::Chunked(size, _) => write!(f, "Chunks::Chunked({:?}, ...)", size),
        }
    }
}

/// Chunk a stream of incoming data for storage. We use the incoming size hint to decide whether
/// to chunk.
pub fn make_chunks<S>(data: S, expected_size: u64, chunk_size: u64) -> Chunks
where
    S: Stream<Item = Bytes, Error = Error> + Send + 'static,
{
    // NOTE: We stop reading if the stream we are provided exceeds the expected_size we were given.
    // While we do check later that the stream matches *exactly* the size we were given, doing this
    // check lets us bail early (and e.g. ensures that if we are told something is 1 byte but it
    // actually is 1TB, we don't try to buffer the whole 1TB).
    let mut observed_size: u64 = 0; // This moves into the closure below and serves as its state.
    let data = data.and_then(move |chunk| {
        // NOTE: unwrap() will fail if we have a Bytes whose length is too large to fit in a u64.
        // We presumably don't have such Bytes in memory!
        observed_size += u64::try_from(chunk.len()).unwrap();

        if observed_size > expected_size {
            return Err(ErrorKind::InvalidSize(expected_size, observed_size).into());
        }

        Ok(chunk)
    });

    if expected_size > chunk_size {
        let stream = ChunkStream::new(data, chunk_size as usize).boxify();
        Chunks::Chunked(expected_size, stream)
    } else {
        let buff = BytesMut::with_capacity(expected_size as usize);

        let fut = data
            .fold(buff, move |mut buff, chunk| -> Result<BytesMut> {
                // NOTE: extend_from_slice should never need to extend here (but it won't panic if
                // it has to), since we reserve the max capacity upfront and don't allow the stream
                // to grow beyond that.
                buff.extend_from_slice(&chunk);
                Ok(buff)
            })
            .map(|bytes_mut| bytes_mut.freeze())
            .boxify();

        Chunks::Inline(fut)
    }
}

#[cfg(test)]
mod test {
    use super::*;

    use futures::stream;
    use quickcheck::quickcheck;
    use tokio::runtime::Runtime;

    #[test]
    fn test_make_chunks_no_chunking() {
        let in_stream = stream::iter_ok::<_, Error>(vec![]);

        match make_chunks(in_stream, 10, 100) {
            Chunks::Inline(_) => {}
            c => panic!("Did not expect {:?}", c),
        };
    }

    #[test]
    fn test_make_chunks_no_chunking_limit() {
        let in_stream = stream::iter_ok::<_, Error>(vec![]);

        match make_chunks(in_stream, 100, 100) {
            Chunks::Inline(_) => {}
            c => panic!("Did not expect {:?}", c),
        };
    }

    #[test]
    fn test_make_chunks_chunking() {
        let in_stream = stream::iter_ok::<_, Error>(vec![]);

        match make_chunks(in_stream, 1000, 100) {
            Chunks::Chunked(1000, _) => {}
            c => panic!("Did not expect {:?}", c),
        };
    }

    #[test]
    fn test_make_chunks_overflow_inline() {
        // Make chunks buffers if we expect content that is small enough to fit the chunk size.
        // However, if we get more content than that, we should stop.
        let mut rt = Runtime::new().unwrap();

        let chunks = vec![
            Bytes::from(vec![1; 5]),
            Bytes::from(vec![1; 5]),
            Bytes::from(vec![1; 5]),
            Bytes::from(vec![1; 5]),
        ];
        let in_stream = stream::iter_ok::<_, Error>(chunks);

        let fut = match make_chunks(in_stream, 10, 100) {
            c @ Chunks::Chunked(..) => panic!("Did not expect {:?}", c),
            Chunks::Inline(fut) => fut,
        };

        rt.block_on(fut)
            .expect_err("make_chunks should abort if the content does not end as advertised");
    }

    #[test]
    fn test_make_chunks_overflow_chunked() {
        // If we get more content than advertises, abort.
        let mut rt = Runtime::new().unwrap();

        let chunks = vec![
            Bytes::from(vec![1; 5]),
            Bytes::from(vec![1; 5]),
            Bytes::from(vec![1; 5]),
            Bytes::from(vec![1; 5]),
        ];
        let in_stream = stream::iter_ok::<_, Error>(chunks);

        let fut = match make_chunks(in_stream, 10, 1) {
            Chunks::Chunked(_, stream) => stream.collect(),
            c @ Chunks::Inline(..) => panic!("Did not expect {:?}", c),
        };

        rt.block_on(fut)
            .expect_err("make_chunks should abort if the content does not end as advertised");
    }

    #[test]
    fn test_bigger_incoming_chunks() {
        // Explicitly test that ChunkStream handles splitting chunks.
        let chunks = vec![vec![1; 10], vec![1; 10]];
        assert!(do_check_chunk_stream(chunks, 5))
    }

    #[test]
    fn test_smaller_incoming_chunks() {
        // Explicitly test that ChunkStream handles putting chunks together.
        let chunks = vec![vec![1; 10], vec![1; 10]];
        assert!(do_check_chunk_stream(chunks, 15))
    }

    fn do_check_chunk_stream(in_chunks: Vec<Vec<u8>>, size: usize) -> bool {
        let mut rt = Runtime::new().unwrap();

        let in_chunks: Vec<Bytes> = in_chunks.into_iter().map(Bytes::from).collect();
        let chunk_stream = ChunkStream::new(stream::iter_ok::<_, ()>(in_chunks.clone()), size);
        let out_chunks = rt.block_on(chunk_stream.collect()).unwrap();

        let expected_bytes = in_chunks.iter().fold(Bytes::new(), |mut bytes, chunk| {
            bytes.extend_from_slice(&chunk);
            bytes
        });

        let got_bytes = out_chunks.iter().fold(Bytes::new(), |mut bytes, chunk| {
            bytes.extend_from_slice(&chunk);
            bytes
        });

        // The contents should be the same
        if expected_bytes != got_bytes {
            return false;
        }

        // If there were no contents, then just return that.
        if expected_bytes.len() == 0 {
            return true;
        }

        // All chunks except for the last one must equal chunk size
        for chunk in out_chunks[0..out_chunks.len() - 1].iter() {
            if chunk.len() != size {
                return false;
            }
        }

        // The last chunk must smaller than the chunk size
        if out_chunks[out_chunks.len() - 1].len() > size {
            return false;
        }

        true
    }

    quickcheck! {
        fn check_chunk_stream(in_chunks: Vec<Vec<u8>>, size: usize) -> bool {
            let size = size + 1; // Don't allow 0 as the size.
            do_check_chunk_stream(in_chunks, size)
        }

        fn check_make_chunks_fut_joins(in_chunks: Vec<Vec<u8>>) -> bool {
            let mut rt = Runtime::new().unwrap();

            let in_chunks: Vec<Bytes> = in_chunks.into_iter().map(Bytes::from).collect();
            let in_stream = stream::iter_ok::<_, Error>(in_chunks.clone());

            let expected_bytes = in_chunks.iter().fold(Bytes::new(), |mut bytes, chunk| {
                bytes.extend_from_slice(&chunk);
                bytes
            });

            let len = expected_bytes.len() as u64;

            let fut = match make_chunks(in_stream, len, len) {
                Chunks::Inline(fut) => fut,
                c => panic!("Did not expect {:?}", c),
            };

            let out_bytes = rt.block_on(fut).unwrap();
            out_bytes == expected_bytes
        }
    }
}
