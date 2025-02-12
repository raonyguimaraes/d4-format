use crate::mode::{AccessMode, CanRead, CanWrite, ReadOnly, ReadWrite};
use std::fs::File;
use std::io::{Read, Result, Seek, SeekFrom, Write};
use std::marker::PhantomData;
use std::ops::Deref;
use std::sync::{Arc, Mutex};

/// The file object that supports random access. Since in D4 file,
/// we actually use a random access file mode, which means all the read
/// and write should provide the address. And this is the object that provides
/// the low level random access interface.
///
/// At the same time, this RandFile object is synchronized, which means we guarantee
/// the thread safety that each block of data is written to file correctly.
///
/// The type parameter `Mode` is served as a type marker that identify the ability of this
/// file.
///
/// The rand file provides a offset-based file access API and data can be read and write from the
/// specified address in blocks. But rand file itself doesn't tracking the block size and it's the
/// upper layer's responsibility to determine the correct block beginning.
pub struct RandFile<'a, Mode: AccessMode, T: 'a> {
    inner: Arc<Mutex<IoWrapper<'a, T>>>,
    token: u32,
    _phantom: PhantomData<Mode>,
}

impl<M: AccessMode, T> Drop for RandFile<'_, M, T> {
    fn drop(&mut self) {
        let mut inner = self.inner.lock().unwrap();
        if inner.token_stack[self.token as usize].0 > 0 {
            inner.token_stack[self.token as usize].0 -= 1;
        }
        let mut update_callbacks = vec![];
        while inner.current_token > 0 && inner.token_stack[inner.current_token as usize].0 == 0 {
            inner.current_token -= 1;
            if let Some((_, update)) = inner.token_stack.pop() {
                update_callbacks.push(update);
            }
        }
        drop(inner);
        update_callbacks.into_iter().for_each(|f| f());
    }
}

struct IoWrapper<'a, T: 'a> {
    inner: T,
    current_token: u32,
    token_stack: Vec<(u32, Box<dyn FnOnce() + Send + 'a>)>,
}

impl<T> IoWrapper<'_, T> {
    fn try_borrow_mut(&mut self, token: u32) -> Result<&mut T> {
        if token == self.current_token {
            Ok(&mut self.inner)
        } else {
            Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "Rand file locked",
            ))
        }
    }
}

impl<T> Deref for IoWrapper<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.inner
    }
}

impl<M: AccessMode, T> Clone for RandFile<'_, M, T> {
    fn clone(&self) -> Self {
        self.inner.lock().unwrap().token_stack[self.token as usize].0 += 1;
        Self {
            inner: self.inner.clone(),
            token: self.token,
            _phantom: PhantomData,
        }
    }
}

impl<'a, M: AccessMode, T: 'a> RandFile<'a, M, T> {
    /// Create a new random access file wrapper
    ///
    /// - `inner`: The underlying implementation for the backend
    /// - `returns`: The newly created random file object
    fn new(inner: T) -> Self {
        RandFile {
            inner: Arc::new(Mutex::new(IoWrapper {
                current_token: 0,
                token_stack: vec![(1, Box::new(|| ()))],
                inner,
            })),
            token: 0,
            _phantom: PhantomData,
        }
    }

    pub fn lock(&mut self, update_fn: Box<dyn FnOnce() + Send + 'a>) -> Result<Self> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::Other, "Lock Error"))?;
        inner.current_token += 1;
        inner.token_stack.push((1, update_fn));
        let token = inner.current_token;
        drop(inner);
        Ok(RandFile {
            inner: self.inner.clone(),
            token,
            _phantom: PhantomData,
        })
    }
}

impl<T: Read + Seek> RandFile<'_, ReadOnly, T> {
    /// The convenient helper function to create a read-only random file
    ///
    /// - `inner`: The underlying implementation for this backend
    pub fn for_read_only(inner: T) -> Self {
        Self::new(inner)
    }
}

impl<T: Read + Write + Seek> RandFile<'_, ReadWrite, T> {
    /// The convenient helper function to create a read-write random file
    ///
    /// - `inner`: The underlying implementation for this backend
    pub fn for_read_write(inner: T) -> Self {
        Self::new(inner)
    }
}

impl<T: CanRead<File>> RandFile<'_, T, File> {
    pub fn mmap(&self, offset: u64, size: usize) -> Result<mapping::MappingHandle> {
        mapping::MappingHandle::new(self, offset, size)
    }
}

impl<T: CanRead<File> + CanWrite<File>> RandFile<'_, T, File> {
    pub fn mmap_mut(&mut self, offset: u64, size: usize) -> Result<mapping::MappingHandleMut> {
        mapping::MappingHandleMut::new(self, offset, size)
    }
}

impl<Mode: CanWrite<T>, T: Write + Seek> RandFile<'_, Mode, T> {
    /// Append a block to the random accessing file
    /// the return value is the relative address compare to the last
    /// accessed block.
    ///
    /// - `buf`: The data buffer that needs to be write
    /// - `returns`: The absolute address of the block that has been written to the file.
    pub fn append_block(&mut self, buf: &[u8]) -> Result<u64> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::Other, "LockError"))?;
        let ret = inner.try_borrow_mut(self.token)?.seek(SeekFrom::End(0))?;
        inner.try_borrow_mut(self.token)?.write_all(buf)?;
        Ok(ret)
    }

    /// Update a data block with the given data buffer.
    ///
    /// - `offset`: The offset of the data block
    /// - `buf`: The data buffer to write
    pub fn update_block(&mut self, offset: u64, buf: &[u8]) -> Result<()> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::Other, "LockError"))?;
        inner
            .try_borrow_mut(self.token)?
            .seek(SeekFrom::Start(offset))?;
        inner.try_borrow_mut(self.token)?.write_all(buf)?;
        Ok(())
    }

    /// Reserve some space in the rand file. This is useful when we want to reserve a data block
    /// for future use. This is very useful for some volatile data (for example the directory block), etc.
    /// And later, we are able to use `update_block` function to keep the reserved block up-to-dated
    pub fn reserve_block(&mut self, size: usize) -> Result<u64> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::Other, "LockError"))?;
        let ret = inner.try_borrow_mut(self.token)?.seek(SeekFrom::End(0))?;
        inner
            .try_borrow_mut(self.token)?
            .seek(SeekFrom::Current(size as i64 - 1))?;
        inner.try_borrow_mut(self.token)?.write_all(b"\0")?;
        Ok(ret)
    }
}
impl<Mode: CanRead<T>, T: Read + Seek> RandFile<'_, Mode, T> {
    pub fn size(&mut self) -> Result<u64> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::Other, "LockError"))?;
        inner.try_borrow_mut(self.token)?.seek(SeekFrom::End(0))
    }
    /// Read a block from the random accessing file
    /// the size of the buffer slice is equal to the number of bytes that is requesting
    /// But there might not be enough bytes available for read, thus we always return
    /// the actual number of bytes is loaded
    pub fn read_block(&mut self, addr: u64, buf: &mut [u8]) -> Result<usize> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::Other, "LockError"))?;
        inner
            .try_borrow_mut(self.token)?
            .seek(SeekFrom::Start(addr))?;
        let mut ret = 0;
        loop {
            let bytes_read = inner.try_borrow_mut(self.token)?.read(&mut buf[ret..])?;
            if bytes_read == 0 {
                break Ok(ret);
            }
            ret += bytes_read;
        }
    }
}

pub mod mapping {
    use super::*;

    use memmap::{Mmap, MmapMut, MmapOptions};
    use std::fs::File;
    use std::io::{Error, ErrorKind};
    use std::sync::Arc;

    struct SyncGuard(MmapMut);

    impl Drop for SyncGuard {
        fn drop(&mut self) {
            self.0.flush().expect("Sync Error");
        }
    }

    #[derive(Clone)]
    pub struct MappingHandle(Arc<Mmap>);

    impl AsRef<[u8]> for MappingHandle {
        fn as_ref(&self) -> &[u8] {
            self.0.as_ref()
        }
    }

    impl MappingHandle {
        pub(super) fn new<M: CanRead<File>>(
            file: &RandFile<M, File>,
            offset: u64,
            size: usize,
        ) -> Result<Self> {
            let inner = file
                .inner
                .lock()
                .map_err(|_| Error::new(ErrorKind::Other, "Lock Error"))?;
            let mapped = unsafe { MmapOptions::new().offset(offset).len(size).map(&*inner)? };
            drop(inner);
            Ok(MappingHandle(Arc::new(mapped)))
        }
    }

    #[derive(Clone)]
    pub struct MappingHandleMut(Arc<SyncGuard>, usize, usize);

    impl AsRef<[u8]> for MappingHandleMut {
        fn as_ref(&self) -> &[u8] {
            unsafe { std::slice::from_raw_parts(std::mem::transmute(self.1), self.2) }
        }
    }

    impl AsMut<[u8]> for MappingHandleMut {
        fn as_mut(&mut self) -> &mut [u8] {
            unsafe { std::slice::from_raw_parts_mut(std::mem::transmute(self.1), self.2) }
        }
    }

    impl MappingHandleMut {
        pub(super) fn new<M: CanRead<File> + CanWrite<File>>(
            file: &RandFile<M, File>,
            offset: u64,
            size: usize,
        ) -> Result<Self> {
            let inner = file
                .inner
                .lock()
                .map_err(|_| Error::new(ErrorKind::Other, "Lock Error"))?;
            let mut mapped = unsafe {
                MmapOptions::new()
                    .offset(offset)
                    .len(size)
                    .map_mut(&*inner)?
            };
            drop(inner);
            let addr = mapped.as_mut().as_mut_ptr();
            Ok(MappingHandleMut(
                Arc::new(SyncGuard(mapped)),
                addr as usize,
                size,
            ))
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::io::Cursor;
    #[test]
    fn test_from_inner() {
        let backend = Cursor::new(vec![0; 1024]);
        let _rand_file = RandFile::for_read_only(backend);

        let backend = Cursor::new(vec![0; 1024]);
        let _rand_file = RandFile::for_read_write(backend);
    }

    #[test]
    fn test_read_write_blocks() {
        let backend = Cursor::new(vec![0; 0]);
        let mut rand_file = RandFile::for_read_write(backend);
        assert_eq!(0, rand_file.append_block(b"This is a test block").unwrap());
        assert_eq!(20, rand_file.append_block(b"This is a test block").unwrap());

        let mut buf = [0u8; 20];
        assert_eq!(20, rand_file.read_block(0, &mut buf).unwrap());
        assert_eq!(b"This is a test block", &buf);
    }

    #[test]
    fn test_lock() {
        let backend = Cursor::new(vec![0; 0]);
        let mut rand_file = RandFile::for_read_write(backend);
        let flag = Arc::new(std::sync::Mutex::new(false));
        {
            let flag = flag.clone();
            let mut locked = rand_file
                .lock(Box::new(move || {
                    *flag.lock().unwrap() = true;
                }))
                .unwrap();
            let mut locked_clone = locked.clone();

            locked.append_block(b"a").unwrap();
            locked_clone.append_block(b"a").unwrap();

            rand_file.append_block(b"c").expect_err("Should be error!");
        }
        rand_file.append_block(b"c").unwrap();
        assert!(*flag.lock().unwrap());
    }
}
