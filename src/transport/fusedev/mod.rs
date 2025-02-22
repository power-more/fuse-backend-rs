// Copyright (C) 2020 Alibaba Cloud. All rights reserved.
//
// SPDX-License-Identifier: Apache-2.0

//! Traits and Structs to implement the /dev/fuse Fuse transport layer.

use std::collections::VecDeque;
use std::fmt;
use std::io::{self, IoSlice, Write};
use std::marker::PhantomData;
use std::mem::ManuallyDrop;
use std::os::unix::io::RawFd;

use nix::sys::uio::{writev, IoVec};
use nix::unistd::write;
use vm_memory::{ByteValued, VolatileMemory, VolatileMemoryError, VolatileSlice};

use super::{FileReadWriteVolatile, FileVolatileSlice, IoBuffers, Reader};
use crate::BitmapSlice;

#[cfg(target_os = "linux")]
mod linux_session;
#[cfg(target_os = "linux")]
pub use linux_session::*;

#[cfg(target_os = "macos")]
mod macos_session;
#[cfg(target_os = "macos")]
pub use macos_session::*;

/// Error codes for Virtio queue related operations.
#[derive(Debug)]
pub enum Error {
    /// Virtio queue descriptor chain overflows.
    DescriptorChainOverflow,
    /// Failed to find memory region for guest physical address.
    FindMemoryRegion,
    /// Invalid virtio queue descriptor chain.
    InvalidChain,
    /// Generic IO error.
    IoError(io::Error),
    /// Out of bounds when splitting VolatileSplice.
    SplitOutOfBounds(usize),
    /// Failed to access volatile memory.
    VolatileMemoryError(VolatileMemoryError),
    /// Session errors
    SessionFailure(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use self::Error::*;

        match self {
            DescriptorChainOverflow => write!(
                f,
                "the combined length of all the buffers in a `DescriptorChain` would overflow"
            ),
            FindMemoryRegion => write!(f, "no memory region for this address range"),
            InvalidChain => write!(f, "invalid descriptor chain"),
            IoError(e) => write!(f, "descriptor I/O error: {}", e),
            SplitOutOfBounds(off) => write!(f, "`DescriptorChain` split is out of bounds: {}", off),
            VolatileMemoryError(e) => write!(f, "volatile memory error: {}", e),
            SessionFailure(e) => write!(f, "fuse session failure: {}", e),
        }
    }
}

impl std::error::Error for Error {}

impl From<Error> for std::io::Error {
    fn from(e: Error) -> Self {
        std::io::Error::new(std::io::ErrorKind::Other, e)
    }
}

/// Result for fusedev transport driver related operations.
pub type Result<T> = std::result::Result<T, Error>;

/// Fake trait to simplify implementation when vhost-user-fs is not used.
pub trait FsCacheReqHandler {}

/// A buffer reference wrapper for fuse requests.
#[derive(Debug)]
pub struct FuseBuf<'a> {
    mem: &'a mut [u8],
}

impl<'a> FuseBuf<'a> {
    /// Construct a new fuse request buffer wrapper.
    pub fn new(mem: &'a mut [u8]) -> FuseBuf<'a> {
        FuseBuf { mem }
    }
}

impl<'a, S: BitmapSlice + Default> Reader<'a, S> {
    /// Construct a new Reader wrapper over `desc_chain`.
    ///
    /// 'request`: Fuse request from clients read from /dev/fuse
    pub fn new(buf: FuseBuf<'a>) -> Result<Reader<'a, S>> {
        let mut buffers: VecDeque<VolatileSlice<'a, S>> = VecDeque::new();
        // Safe because Reader has the same lifetime with buf.
        buffers.push_back(unsafe {
            VolatileSlice::with_bitmap(buf.mem.as_mut_ptr(), buf.mem.len(), S::default())
        });

        Ok(Reader {
            buffers: IoBuffers {
                buffers,
                bytes_consumed: 0,
            },
        })
    }
}

/// A writer for fuse request. There are a few special properties to follow:
/// 1. A fuse device request MUST be written to the fuse device in one shot.
/// 2. If the writer is split, a final commit() MUST be called to issue the
///    device write operation.
/// 3. Concurrency, caller should not write to the writer concurrently.
#[derive(Debug, PartialEq, Eq)]
pub struct Writer<'a, S: BitmapSlice = ()> {
    fd: RawFd,
    buffered: bool,
    buf: ManuallyDrop<Vec<u8>>,
    bitmapslice: S,
    phantom: PhantomData<&'a mut [S]>,
}

impl<'a, S: BitmapSlice + Default> Writer<'a, S> {
    /// Construct a new Writer
    pub fn new(fd: RawFd, data_buf: &'a mut [u8]) -> Result<Writer<'a, S>> {
        let buf = unsafe { Vec::from_raw_parts(data_buf.as_mut_ptr(), 0, data_buf.len()) };
        Ok(Writer {
            fd,
            buffered: false,
            buf: ManuallyDrop::new(buf),
            bitmapslice: S::default(),
            phantom: PhantomData,
        })
    }
}

impl<'a, S: BitmapSlice> Writer<'a, S> {
    /// Splits this `Writer` into two at the given offset in the buffer.
    /// After the split, `self` will be able to write up to `offset` bytes while the returned
    /// `Writer` can write up to `available_bytes() - offset` bytes.  Returns an error if
    /// `offset > self.available_bytes()`.
    pub fn split_at(&mut self, offset: usize) -> Result<Writer<'a, S>> {
        if self.buf.capacity() < offset {
            return Err(Error::SplitOutOfBounds(offset));
        }

        let (len1, len2) = if self.buf.len() > offset {
            (offset, self.buf.len() - offset)
        } else {
            (self.buf.len(), 0)
        };
        let cap2 = self.buf.capacity() - offset;
        let ptr = self.buf.as_mut_ptr();

        // Safe because both buffers refer to different parts of the same underlying `data_buf`.
        self.buf = unsafe { ManuallyDrop::new(Vec::from_raw_parts(ptr, len1, offset)) };
        self.buffered = true;
        let buf = unsafe { ManuallyDrop::new(Vec::from_raw_parts(ptr.add(offset), len2, cap2)) };

        Ok(Writer {
            fd: self.fd,
            buffered: true,
            buf,
            bitmapslice: self.bitmapslice.clone(),
            phantom: PhantomData,
        })
    }

    /// Commit all internal buffers of self and others
    /// We need this because the lifetime of others is usually shorter than self.
    pub fn commit(&mut self, other: Option<&Writer<'a, S>>) -> io::Result<usize> {
        if !self.buffered {
            return Ok(0);
        }

        let o = other.map(|v| v.buf.as_slice()).unwrap_or(&[]);
        let res = match (self.buf.len(), o.len()) {
            (0, 0) => Ok(0),
            (0, _) => write(self.fd, o),
            (_, 0) => write(self.fd, self.buf.as_slice()),
            (_, _) => {
                let bufs = [IoVec::from_slice(self.buf.as_slice()), IoVec::from_slice(o)];
                writev(self.fd, &bufs)
            }
        };

        res.map_err(|e| {
            error! {"fail to write to fuse device on commit: {}", e};
            io::Error::from_raw_os_error(e as i32)
        })
    }

    /// Returns number of bytes already written to the internal buffer.
    pub fn bytes_written(&self) -> usize {
        self.buf.len()
    }

    /// Returns number of bytes available for writing.
    pub fn available_bytes(&self) -> usize {
        self.buf.capacity() - self.buf.len()
    }

    fn account_written(&mut self, count: usize) {
        let new_len = self.buf.len() + count;
        // Safe because check_avail_space() ensures that `count` is valid.
        unsafe { self.buf.set_len(new_len) };
    }

    /// Writes an object to the writer.
    pub fn write_obj<T: ByteValued>(&mut self, val: T) -> io::Result<()> {
        self.write_all(val.as_slice())
    }

    /// Writes data to the writer from a file descriptor.
    /// Returns the number of bytes written to the writer.
    pub fn write_from<F: FileReadWriteVolatile>(
        &mut self,
        mut src: F,
        count: usize,
    ) -> io::Result<usize> {
        self.check_available_space(count)?;

        let cnt = src.read_vectored_volatile(
            // Safe because we have made sure buf has at least count capacity above
            unsafe {
                &[FileVolatileSlice::new(
                    self.buf.as_mut_ptr().add(self.buf.len()),
                    count,
                )]
            },
        )?;
        self.account_written(cnt);

        if self.buffered {
            Ok(cnt)
        } else {
            Self::do_write(self.fd, &self.buf[..cnt])
        }
    }

    /// Writes data to the writer from a File at offset `off`.
    /// Returns the number of bytes written to the writer.
    pub fn write_from_at<F: FileReadWriteVolatile>(
        &mut self,
        mut src: F,
        count: usize,
        off: u64,
    ) -> io::Result<usize> {
        self.check_available_space(count)?;

        let cnt = src.read_vectored_at_volatile(
            // Safe because we have made sure buf has at least count capacity above
            unsafe {
                &[FileVolatileSlice::new(
                    self.buf.as_mut_ptr().add(self.buf.len()),
                    count,
                )]
            },
            off,
        )?;
        self.account_written(cnt);

        if self.buffered {
            Ok(cnt)
        } else {
            Self::do_write(self.fd, &self.buf[..cnt])
        }
    }

    /// Writes all data to the writer from a file descriptor.
    pub fn write_all_from<F: FileReadWriteVolatile>(
        &mut self,
        mut src: F,
        mut count: usize,
    ) -> io::Result<()> {
        self.check_available_space(count)?;

        while count > 0 {
            match self.write_from(&mut src, count) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "failed to write whole buffer",
                    ))
                }
                Ok(n) => count -= n,
                Err(ref e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }

        Ok(())
    }

    fn check_available_space(&self, sz: usize) -> io::Result<()> {
        assert!(self.buffered || self.buf.len() == 0);
        if sz > self.available_bytes() {
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "data out of range, available {} requested {}",
                    self.available_bytes(),
                    sz
                ),
            ))
        } else {
            Ok(())
        }
    }

    fn do_write(fd: RawFd, data: &[u8]) -> io::Result<usize> {
        let res = write(fd, data);

        res.map_err(|e| {
            error! {"fail to write to fuse device fd {}: {}, {:?}", fd, e, data};
            io::Error::new(io::ErrorKind::Other, format!("{}", e))
        })
    }
}

impl<'a, S: BitmapSlice> io::Write for Writer<'a, S> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.check_available_space(data.len())?;

        if self.buffered {
            self.buf.extend_from_slice(data);
            Ok(data.len())
        } else {
            Self::do_write(self.fd, data).map(|x| {
                self.account_written(x);
                x
            })
        }
    }

    // default write_vectored only writes the first non-empty IoSlice. Override it.
    fn write_vectored(&mut self, bufs: &[IoSlice<'_>]) -> io::Result<usize> {
        self.check_available_space(bufs.iter().fold(0, |acc, x| acc + x.len()))?;

        if self.buffered {
            let count = bufs.iter().filter(|b| !b.is_empty()).fold(0, |acc, b| {
                self.buf.extend_from_slice(b);
                acc + b.len()
            });
            Ok(count)
        } else {
            let buf: Vec<IoVec<&[u8]>> = bufs
                .iter()
                .filter(|b| !b.is_empty())
                .map(|b| IoVec::from_slice(b))
                .collect();

            if buf.is_empty() {
                return Ok(0);
            }
            writev(self.fd, buf.as_slice())
                .map(|x| {
                    self.account_written(x);
                    x
                })
                .map_err(|e| {
                    error! {"fail to write to fuse device on commit: {}", e};
                    io::Error::new(io::ErrorKind::Other, format!("{}", e))
                })
        }
    }

    /// As this writer can associate multiple writers by splitting, `flush()` can't
    /// flush them all. Disable it!
    fn flush(&mut self) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Other,
            "Writer does not support flush buffer.",
        ))
    }
}

#[cfg(feature = "async-io")]
mod async_io {
    use super::*;
    use crate::async_util::{AsyncDrive, AsyncUtil};

    impl<'a, S: BitmapSlice> Reader<'a, S> {
        /// Reads data from the data buffer into a File at offset `off` in asynchronous mode.
        ///
        /// Returns the number of bytes read from the descriptor chain buffer. The number of bytes
        /// read can be less than `count` if there isn't enough data in the descriptor chain buffer.
        pub async fn async_read_to_at<D: AsyncDrive>(
            &mut self,
            drive: D,
            dst: RawFd,
            count: usize,
            off: u64,
        ) -> io::Result<usize> {
            let bufs = self.buffers.allocate_io_slice(count);
            if bufs.is_empty() {
                Ok(0)
            } else {
                let result = if bufs.len() == 1 {
                    AsyncUtil::write(drive, dst, bufs[0].as_ref(), off).await?
                } else {
                    panic!("fusedev: only one data buffer is supported");
                };
                self.buffers.mark_used(result)?;
                Ok(result)
            }
        }
    }

    impl<'a, S: BitmapSlice> Writer<'a, S> {
        /// Write data from a buffer into this writer in asynchronous mode.
        ///
        /// Returns the number of bytes written to the writer.
        pub async fn async_write<D: AsyncDrive>(
            &mut self,
            drive: D,
            data: &[u8],
        ) -> io::Result<usize> {
            self.check_available_space(data.len())?;

            if self.buffered {
                // write to internal buf
                self.buf.extend_from_slice(data);
                Ok(data.len())
            } else {
                // write to fd, can only happen once per instance
                AsyncUtil::write(drive, self.fd, data, 0)
                    .await
                    .map(|x| {
                        self.account_written(x);
                        x
                    })
                    .map_err(|e| {
                        error! {"fail to write to fuse device fd {}: {}, {:?}", self.fd, e, data};
                        io::Error::new(io::ErrorKind::Other, format!("{}", e))
                    })
            }
        }

        /// Write data from two buffers into this writer in asynchronous mode.
        ///
        /// Returns the number of bytes written to the writer.
        pub async fn async_write2<D: AsyncDrive>(
            &mut self,
            drive: D,
            data: &[u8],
            data2: &[u8],
        ) -> io::Result<usize> {
            let len = data.len() + data2.len();
            self.check_available_space(len)?;

            if self.buffered {
                // write to internal buf
                self.buf.extend_from_slice(data);
                self.buf.extend_from_slice(data2);
                Ok(len)
            } else {
                // write to fd, can only happen once per instance
                AsyncUtil::write2(drive, self.fd, data, data2, 0)
                    .await
                    .map(|x| {
                        self.account_written(x);
                        x
                    })
                    .map_err(|e| {
                        error! {"fail to write to fuse device fd {}: {}, {:?}", self.fd, e, data};
                        io::Error::new(io::ErrorKind::Other, format!("{}", e))
                    })
            }
        }

        /// Write data from two buffers into this writer in asynchronous mode.
        ///
        /// Returns the number of bytes written to the writer.
        pub async fn async_write3<D: AsyncDrive>(
            &mut self,
            drive: D,
            data: &[u8],
            data2: &[u8],
            data3: &[u8],
        ) -> io::Result<usize> {
            let len = data.len() + data2.len() + data3.len();
            self.check_available_space(len)?;

            if self.buffered {
                // write to internal buf
                self.buf.extend_from_slice(data);
                self.buf.extend_from_slice(data2);
                self.buf.extend_from_slice(data3);
                Ok(len)
            } else {
                // write to fd, can only happen once per instance
                AsyncUtil::write3(drive, self.fd, data, data2, data3, 0)
                    .await
                    .map(|x| {
                        self.account_written(x);
                        x
                    })
                    .map_err(|e| {
                        error! {"fail to write to fuse device fd {}: {}, {:?}", self.fd, e, data};
                        io::Error::new(io::ErrorKind::Other, format!("{}", e))
                    })
            }
        }

        /// Attempts to write an entire buffer into this writer in asynchronous mode.
        pub async fn async_write_all<D: AsyncDrive>(
            &mut self,
            drive: D,
            mut buf: &[u8],
        ) -> io::Result<()> {
            while !buf.is_empty() {
                match self.async_write(drive.clone(), buf).await {
                    Ok(0) => {
                        return Err(io::Error::new(
                            io::ErrorKind::WriteZero,
                            "failed to write whole buffer",
                        ));
                    }
                    Ok(n) => buf = &buf[n..],
                    Err(ref e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(e),
                }
            }

            Ok(())
        }

        /// Writes data from a File at offset `off` to the writer in asynchronous mode.
        ///
        /// Returns the number of bytes written to the writer.
        pub async fn async_write_from_at<D: AsyncDrive>(
            &mut self,
            drive: D,
            src: RawFd,
            count: usize,
            off: u64,
        ) -> io::Result<usize> {
            self.check_available_space(count)?;

            let drive2 = drive.clone();
            let buf = unsafe {
                std::slice::from_raw_parts_mut(self.buf.as_mut_ptr().add(self.buf.len()), count)
            };
            let cnt = AsyncUtil::read(drive2, src, buf, off).await?;
            self.account_written(cnt);

            if self.buffered {
                Ok(cnt)
            } else {
                // write to fd
                AsyncUtil::write(drive, self.fd, &self.buf[..cnt], 0).await
            }
        }

        /// Commit all internal buffers of the writer and others.
        ///
        /// We need this because the lifetime of others is usually shorter than self.
        pub async fn async_commit<D: AsyncDrive>(
            &mut self,
            drive: D,
            other: Option<&Writer<'a, S>>,
        ) -> io::Result<usize> {
            let o = other.map(|v| v.buf.as_slice()).unwrap_or(&[]);

            let res = match (self.buf.len(), o.len()) {
                (0, 0) => Ok(0),
                (0, _) => AsyncUtil::write(drive, self.fd, o, 0).await,
                (_, 0) => AsyncUtil::write(drive, self.fd, self.buf.as_slice(), 0).await,
                (_, _) => AsyncUtil::write2(drive, self.fd, self.buf.as_slice(), o, 0).await,
            };

            res.map_err(|e| {
                error! {"fail to write to fuse device on commit: {}", e};
                e
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::os::unix::io::AsRawFd;
    use vmm_sys_util::tempfile::TempFile;

    #[test]
    fn reader_test_simple_chain() {
        let mut buf = [0u8; 106];
        let mut reader = Reader::<()>::new(FuseBuf::new(&mut buf)).unwrap();

        assert_eq!(reader.available_bytes(), 106);
        assert_eq!(reader.bytes_read(), 0);

        let mut buffer = [0 as u8; 64];
        if let Err(_) = reader.read_exact(&mut buffer) {
            panic!("read_exact should not fail here");
        }

        assert_eq!(reader.available_bytes(), 42);
        assert_eq!(reader.bytes_read(), 64);

        match reader.read(&mut buffer) {
            Err(_) => panic!("read should not fail here"),
            Ok(length) => assert_eq!(length, 42),
        }

        assert_eq!(reader.available_bytes(), 0);
        assert_eq!(reader.bytes_read(), 106);
    }

    #[test]
    fn writer_test_simple_chain() {
        let file = TempFile::new().unwrap().into_file();
        let mut buf = vec![0x0u8; 106];
        let mut writer = Writer::<()>::new(file.as_raw_fd(), &mut buf).unwrap();

        writer.buffered = true;
        assert_eq!(writer.available_bytes(), 106);
        assert_eq!(writer.bytes_written(), 0);

        let mut buffer = [0 as u8; 64];
        if let Err(_) = writer.write_all(&mut buffer) {
            panic!("write_all should not fail here");
        }

        assert_eq!(writer.available_bytes(), 42);
        assert_eq!(writer.bytes_written(), 64);

        let mut buffer = [0 as u8; 42];
        match writer.write(&mut buffer) {
            Err(_) => panic!("write should not fail here"),
            Ok(length) => assert_eq!(length, 42),
        }

        assert_eq!(writer.available_bytes(), 0);
        assert_eq!(writer.bytes_written(), 106);
    }

    #[test]
    fn writer_test_split_chain() {
        let file = TempFile::new().unwrap().into_file();
        let mut buf = vec![0x0u8; 108];
        let mut writer = Writer::<()>::new(file.as_raw_fd(), &mut buf).unwrap();
        let writer2 = writer.split_at(106).unwrap();

        assert_eq!(writer.available_bytes(), 106);
        assert_eq!(writer.bytes_written(), 0);
        assert_eq!(writer2.available_bytes(), 2);
        assert_eq!(writer2.bytes_written(), 0);

        let mut buffer = [0 as u8; 64];
        if let Err(_) = writer.write_all(&mut buffer) {
            panic!("write_all should not fail here");
        }

        assert_eq!(writer.available_bytes(), 42);
        assert_eq!(writer.bytes_written(), 64);

        let mut buffer = [0 as u8; 42];
        match writer.write(&mut buffer) {
            Err(_) => panic!("write should not fail here"),
            Ok(length) => assert_eq!(length, 42),
        }

        assert_eq!(writer.available_bytes(), 0);
        assert_eq!(writer.bytes_written(), 106);
    }

    #[test]
    fn reader_unexpected_eof() {
        let mut buf = [0u8; 106];
        let mut reader = Reader::<()>::new(FuseBuf::new(&mut buf)).unwrap();

        let mut buf2 = Vec::with_capacity(1024);
        buf2.resize(1024, 0);

        assert_eq!(
            reader
                .read_exact(&mut buf2[..])
                .expect_err("read more bytes than available")
                .kind(),
            io::ErrorKind::UnexpectedEof
        );
    }

    #[test]
    fn reader_split_border() {
        let mut buf = [0u8; 128];
        let mut reader = Reader::<()>::new(FuseBuf::new(&mut buf)).unwrap();
        let other = reader.split_at(32).expect("failed to split Reader");

        assert_eq!(reader.available_bytes(), 32);
        assert_eq!(other.available_bytes(), 96);
    }

    #[test]
    fn reader_split_outofbounds() {
        let mut buf = [0u8; 128];
        let mut reader = Reader::<()>::new(FuseBuf::new(&mut buf)).unwrap();

        if let Ok(_) = reader.split_at(256) {
            panic!("successfully split Reader with out of bounds offset");
        }
    }

    #[test]
    fn writer_simple_commit_header() {
        let file = TempFile::new().unwrap().into_file();
        let mut buf = vec![0x0u8; 106];
        let mut writer = Writer::<()>::new(file.as_raw_fd(), &mut buf).unwrap();

        writer.buffered = true;
        assert_eq!(writer.available_bytes(), 106);

        writer.write(&[0x1u8; 4]).unwrap();
        assert_eq!(writer.available_bytes(), 102);
        assert_eq!(writer.bytes_written(), 4);

        let buf = vec![0xdeu8; 64];
        let slices = [
            IoSlice::new(&buf[..32]),
            IoSlice::new(&buf[32..48]),
            IoSlice::new(&buf[48..]),
        ];
        assert_eq!(
            writer
                .write_vectored(&slices)
                .expect("failed to write from buffer"),
            64
        );
        assert!(writer.flush().is_err());

        writer.commit(None).unwrap();
    }

    #[test]
    fn writer_split_commit_header() {
        let file = TempFile::new().unwrap().into_file();
        let mut buf = vec![0x0u8; 106];
        let mut writer = Writer::<()>::new(file.as_raw_fd(), &mut buf).unwrap();
        let mut other = writer.split_at(4).expect("failed to split Writer");

        assert_eq!(writer.available_bytes(), 4);
        assert_eq!(other.available_bytes(), 102);

        writer.write(&[0x1u8; 4]).unwrap();
        assert_eq!(writer.available_bytes(), 0);
        assert_eq!(writer.bytes_written(), 4);

        let buf = vec![0xdeu8; 64];
        let slices = [
            IoSlice::new(&buf[..32]),
            IoSlice::new(&buf[32..48]),
            IoSlice::new(&buf[48..]),
        ];
        assert_eq!(
            other
                .write_vectored(&slices)
                .expect("failed to write from buffer"),
            64
        );
        assert!(writer.flush().is_err());

        writer.commit(None).unwrap();
    }

    #[test]
    fn writer_split_commit_all() {
        let file = TempFile::new().unwrap().into_file();
        let mut buf = vec![0x0u8; 106];
        let mut writer = Writer::<()>::new(file.as_raw_fd(), &mut buf).unwrap();
        let mut other = writer.split_at(4).expect("failed to split Writer");

        assert_eq!(writer.available_bytes(), 4);
        assert_eq!(other.available_bytes(), 102);

        writer.write(&[0x1u8; 4]).unwrap();
        assert_eq!(writer.available_bytes(), 0);
        assert_eq!(writer.bytes_written(), 4);

        let buf = vec![0xdeu8; 64];
        let slices = [
            IoSlice::new(&buf[..32]),
            IoSlice::new(&buf[32..48]),
            IoSlice::new(&buf[48..]),
        ];
        assert_eq!(
            other
                .write_vectored(&slices)
                .expect("failed to write from buffer"),
            64
        );

        writer.commit(Some(&other)).unwrap();
    }

    #[test]
    fn read_full() {
        let mut buf2 = [0u8; 48];
        let mut reader = Reader::<()>::new(FuseBuf::new(&mut buf2)).unwrap();
        let mut buf = vec![0u8; 64];

        assert_eq!(
            reader.read(&mut buf[..]).expect("failed to read to buffer"),
            48
        );
    }

    #[test]
    fn write_full() {
        let file = TempFile::new().unwrap().into_file();
        let mut buf = vec![0x0u8; 48];
        let mut writer = Writer::<()>::new(file.as_raw_fd(), &mut buf).unwrap();

        let buf = vec![0xdeu8; 64];
        writer.write(&buf[..]).unwrap_err();

        let buf = vec![0xdeu8; 48];
        assert_eq!(
            writer.write(&buf[..]).expect("failed to write from buffer"),
            48
        );
    }

    #[test]
    fn write_vectored() {
        let file = TempFile::new().unwrap().into_file();
        let mut buf = vec![0x0u8; 48];
        let mut writer = Writer::<()>::new(file.as_raw_fd(), &mut buf).unwrap();

        let buf = vec![0xdeu8; 48];
        let slices = [
            IoSlice::new(&buf[..32]),
            IoSlice::new(&buf[32..40]),
            IoSlice::new(&buf[40..]),
        ];
        assert_eq!(
            writer
                .write_vectored(&slices)
                .expect("failed to write from buffer"),
            48
        );
    }

    #[test]
    fn read_obj() {
        let mut buf2 = [0u8; 9];
        let mut reader = Reader::<()>::new(FuseBuf::new(&mut buf2)).unwrap();

        let _val: u64 = reader.read_obj().expect("failed to read to file");

        assert_eq!(reader.available_bytes(), 1);
        assert_eq!(reader.bytes_read(), 8);
        assert!(reader.read_obj::<u64>().is_err());
    }

    #[test]
    fn read_exact_to() {
        let mut buf2 = [0u8; 48];
        let mut reader = Reader::<()>::new(FuseBuf::new(&mut buf2)).unwrap();
        let mut file = TempFile::new().unwrap().into_file();

        reader
            .read_exact_to(&mut file, 47)
            .expect("failed to read to file");

        assert_eq!(reader.available_bytes(), 1);
        assert_eq!(reader.bytes_read(), 47);
    }

    #[test]
    fn read_to_at() {
        let mut buf2 = [0u8; 48];
        let mut reader = Reader::<()>::new(FuseBuf::new(&mut buf2)).unwrap();
        let mut file = TempFile::new().unwrap().into_file();

        assert_eq!(
            reader
                .read_to_at(&mut file, 48, 16)
                .expect("failed to read to file"),
            48
        );
        assert_eq!(reader.available_bytes(), 0);
        assert_eq!(reader.bytes_read(), 48);
    }

    #[test]
    fn write_obj() {
        let file1 = TempFile::new().unwrap().into_file();
        let mut buf = vec![0x0u8; 48];
        let mut writer = Writer::<()>::new(file1.as_raw_fd(), &mut buf).unwrap();
        let val = 0x1u64;

        writer.write_obj(val).expect("failed to write from buffer");
        assert_eq!(writer.available_bytes(), 40);
    }

    #[test]
    fn write_all_from() {
        let file1 = TempFile::new().unwrap().into_file();
        let mut buf = vec![0x0u8; 48];
        let mut writer = Writer::<()>::new(file1.as_raw_fd(), &mut buf).unwrap();
        let mut file = TempFile::new().unwrap().into_file();
        let buf = vec![0xdeu8; 64];

        writer.buffered = true;

        file.write_all(&buf).unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        writer
            .write_all_from(&mut file, 47)
            .expect("failed to write from buffer");
        assert_eq!(writer.available_bytes(), 1);
        assert_eq!(writer.bytes_written(), 47);

        // Write more data than capacity
        writer.write_all_from(&mut file, 2).unwrap_err();
        assert_eq!(writer.available_bytes(), 1);
        assert_eq!(writer.bytes_written(), 47);
    }

    #[test]
    fn write_all_from_split() {
        let file1 = TempFile::new().unwrap().into_file();
        let mut buf = vec![0x0u8; 58];
        let mut writer = Writer::<()>::new(file1.as_raw_fd(), &mut buf).unwrap();
        let _other = writer.split_at(48).unwrap();
        let mut file = TempFile::new().unwrap().into_file();
        let buf = vec![0xdeu8; 64];

        file.write_all(&buf).unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        writer
            .write_all_from(&mut file, 47)
            .expect("failed to write from buffer");
        assert_eq!(writer.available_bytes(), 1);
        assert_eq!(writer.bytes_written(), 47);

        // Write more data than capacity
        writer.write_all_from(&mut file, 2).unwrap_err();
        assert_eq!(writer.available_bytes(), 1);
        assert_eq!(writer.bytes_written(), 47);
    }

    #[test]
    fn write_from_at() {
        let file1 = TempFile::new().unwrap().into_file();
        let mut buf = vec![0x0u8; 48];
        let mut writer = Writer::<()>::new(file1.as_raw_fd(), &mut buf).unwrap();
        let mut file = TempFile::new().unwrap().into_file();
        let buf = vec![0xdeu8; 64];

        writer.buffered = true;

        file.write_all(&buf).unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        assert_eq!(
            writer
                .write_from_at(&mut file, 40, 16)
                .expect("failed to write from buffer"),
            40
        );
        assert_eq!(writer.available_bytes(), 8);
        assert_eq!(writer.bytes_written(), 40);

        // Write more data than capacity
        writer.write_from_at(&mut file, 40, 16).unwrap_err();
        assert_eq!(writer.available_bytes(), 8);
        assert_eq!(writer.bytes_written(), 40);
    }

    #[test]
    fn write_from_at_split() {
        let file1 = TempFile::new().unwrap().into_file();
        let mut buf = vec![0x0u8; 58];
        let mut writer = Writer::<()>::new(file1.as_raw_fd(), &mut buf).unwrap();
        let _other = writer.split_at(48).unwrap();
        let mut file = TempFile::new().unwrap().into_file();
        let buf = vec![0xdeu8; 64];

        file.write_all(&buf).unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        assert_eq!(
            writer
                .write_from_at(&mut file, 40, 16)
                .expect("failed to write from buffer"),
            40
        );
        assert_eq!(writer.available_bytes(), 8);
        assert_eq!(writer.bytes_written(), 40);

        // Write more data than capacity
        writer.write_from_at(&mut file, 40, 16).unwrap_err();
        assert_eq!(writer.available_bytes(), 8);
        assert_eq!(writer.bytes_written(), 40);
    }

    #[cfg(feature = "async-io")]
    mod async_io {
        use futures::executor::{block_on, ThreadPool};
        use futures::task::SpawnExt;
        use ringbahn::drive::demo::DemoDriver;

        use super::*;

        #[test]
        fn async_read_to_at() {
            let file = TempFile::new().unwrap().into_file();
            let fd = file.as_raw_fd();

            let executor = ThreadPool::new().unwrap();
            let handle = executor
                .spawn_with_handle(async move {
                    let mut buf2 = [0u8; 48];
                    let mut reader = Reader::<()>::new(FuseBuf::new(&mut buf2)).unwrap();
                    let drive = DemoDriver::default();

                    reader.async_read_to_at(drive, fd, 48, 16).await
                })
                .unwrap();

            assert_eq!(block_on(handle).unwrap(), 48);
        }

        #[test]
        fn async_write() {
            let file = TempFile::new().unwrap().into_file();
            let fd = file.as_raw_fd();

            let executor = ThreadPool::new().unwrap();
            let handle = executor
                .spawn_with_handle(async move {
                    let drive = DemoDriver::default();
                    let mut buf = vec![0x0u8; 48];
                    let mut writer = Writer::<()>::new(fd, &mut buf).unwrap();

                    let buf = vec![0xdeu8; 64];
                    writer.async_write(drive, &buf[..]).await
                })
                .unwrap();

            // expect errors
            block_on(handle).unwrap_err();

            let fd = file.as_raw_fd();
            let handle = executor
                .spawn_with_handle(async move {
                    let drive = DemoDriver::default();
                    let mut buf = vec![0x0u8; 48];
                    let mut writer2 = Writer::<()>::new(fd, &mut buf).unwrap();

                    let buf = vec![0xdeu8; 48];
                    writer2.async_write(drive, &buf[..]).await
                })
                .unwrap();

            assert_eq!(block_on(handle).unwrap(), 48);
        }

        #[test]
        fn async_write2() {
            let file = TempFile::new().unwrap().into_file();
            let fd = file.as_raw_fd();

            let executor = ThreadPool::new().unwrap();
            let handle = executor
                .spawn_with_handle(async move {
                    let drive = DemoDriver::default();
                    let mut buf = vec![0x0u8; 48];
                    let mut writer = Writer::<()>::new(fd, &mut buf).unwrap();
                    let buf = vec![0xdeu8; 48];

                    writer.async_write2(drive, &buf[..32], &buf[32..]).await
                })
                .unwrap();

            assert_eq!(block_on(handle).unwrap(), 48);
        }

        #[test]
        fn async_write3() {
            let file = TempFile::new().unwrap().into_file();
            let fd = file.as_raw_fd();

            let executor = ThreadPool::new().unwrap();
            let handle = executor
                .spawn_with_handle(async move {
                    let drive = DemoDriver::default();
                    let mut buf = vec![0x0u8; 48];
                    let mut writer = Writer::<()>::new(fd, &mut buf).unwrap();
                    let buf = vec![0xdeu8; 48];

                    writer
                        .async_write3(drive, &buf[..32], &buf[32..40], &buf[40..])
                        .await
                })
                .unwrap();

            assert_eq!(block_on(handle).unwrap(), 48);
        }

        #[test]
        fn async_write_from_at() {
            let file1 = TempFile::new().unwrap().into_file();
            let fd1 = file1.as_raw_fd();
            let mut file = TempFile::new().unwrap().into_file();
            let fd = file.as_raw_fd();
            let buf = vec![0xdeu8; 64];

            file.write_all(&buf).unwrap();
            file.seek(SeekFrom::Start(0)).unwrap();

            let executor = ThreadPool::new().unwrap();
            let handle = executor
                .spawn_with_handle(async move {
                    let drive = DemoDriver::default();
                    let mut buf = vec![0x0u8; 48];
                    let mut writer = Writer::<()>::new(fd1, &mut buf).unwrap();

                    writer.async_write_from_at(drive, fd, 40, 16).await
                })
                .unwrap();

            assert_eq!(block_on(handle).unwrap(), 40);
        }

        #[test]
        fn async_writer_split_commit_all() {
            let file = TempFile::new().unwrap().into_file();
            let fd = file.as_raw_fd();
            let mut buf = vec![0x0u8; 106];
            let buf = unsafe { std::mem::transmute::<&mut [u8], &'static mut [u8]>(&mut buf) };
            let mut writer = Writer::<()>::new(fd, buf).unwrap();
            let mut other = writer.split_at(4).expect("failed to split Writer");

            assert_eq!(writer.available_bytes(), 4);
            assert_eq!(other.available_bytes(), 102);

            writer.write(&[0x1u8; 4]).unwrap();
            assert_eq!(writer.available_bytes(), 0);
            assert_eq!(writer.bytes_written(), 4);

            let buf = vec![0xdeu8; 64];
            let slices = [
                IoSlice::new(&buf[..32]),
                IoSlice::new(&buf[32..48]),
                IoSlice::new(&buf[48..]),
            ];
            assert_eq!(
                other
                    .write_vectored(&slices)
                    .expect("failed to write from buffer"),
                64
            );

            let executor = ThreadPool::new().unwrap();
            let handle = executor
                .spawn_with_handle(async move {
                    let drive = DemoDriver::default();

                    writer.async_commit(drive, Some(&other)).await
                })
                .unwrap();

            let _result = block_on(handle).unwrap();
        }
    }
}
