use std::convert::TryFrom;
use std::fs::{File, OpenOptions};
use std::marker::PhantomData;
use std::ops::{Deref, DerefMut};
use std::path::Path;
use std::slice;
use std::{cmp, fmt, io};

use crate::os::{advise, flush, lock, map_anon, map_file, protect, unlock, unmap};
use crate::{
    AdviseAccess, AdviseUsage, ConvertResult, Error, Flush, Input, Operation, Protect, Result,
    Size, Span, SpanMut,
};

mod private {
    pub trait FromPtr {
        unsafe fn from_ptr(ptr: *mut u8, len: usize) -> Self;
    }
}

use private::FromPtr;

/// Allocation of one or more read-only sequential pages.
///
/// # Examples
///
/// ```
/// # extern crate vmap;
/// use vmap::{Map, AdviseAccess, AdviseUsage};
/// use std::fs::OpenOptions;
/// use std::str::from_utf8;
///
/// # fn main() -> vmap::Result<()> {
/// let page = Map::with_options().offset(113).len(30).open("README.md")?;
/// page.advise(AdviseAccess::Sequential, AdviseUsage::WillNeed)?;
/// assert_eq!(Ok("fast and safe memory-mapped IO"), from_utf8(&page[..]));
/// assert_eq!(Ok("safe"), from_utf8(&page[9..13]));
/// # Ok(())
/// # }
/// ```
pub struct Map(MapMut);

impl Map {
    /// TODO
    pub fn with_options() -> Options<Self> {
        Options::new()
    }

    /// Creates a new read-only map object using the full range of a file.
    ///
    /// The underlying file handle is open as read-only. If there is a need to
    /// convert the `Map` into a `MapMut`, use `Map::file` with a file handle
    /// open for writing. If not done, the convertion to `MapMut` will fail.
    ///
    /// # Examples
    /// ```
    /// # extern crate vmap;
    /// use vmap::Map;
    /// use std::fs::OpenOptions;
    /// use std::str::from_utf8;
    ///
    /// # fn main() -> vmap::Result<()> {
    /// let map = Map::open("README.md")?;
    /// assert_eq!(map.is_empty(), false);
    /// assert_eq!(Ok("fast and safe memory-mapped IO"), from_utf8(&map[113..143]));
    ///
    /// // The file handle is read-only.
    /// assert!(map.into_map_mut().is_err());
    /// # Ok(())
    /// # }
    /// ```
    #[deprecated(since = "0.4.0", note = "use Map::with_options().open(path) instead")]
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        Self::with_options().open(path)
    }

    /// Create a new map object from a range of a file.
    ///
    /// # Examples
    ///
    /// ```
    /// # extern crate vmap;
    /// use vmap::Map;
    /// use std::fs::OpenOptions;
    /// use std::str::from_utf8;
    ///
    /// # fn main() -> vmap::Result<()> {
    /// let file = OpenOptions::new().read(true).open("README.md")?;
    /// let map = Map::file(&file, 0, 143)?;
    /// assert_eq!(map.is_empty(), false);
    /// assert_eq!(Ok("fast and safe memory-mapped IO"), from_utf8(&map[113..143]));
    ///
    /// let map = Map::file(&file, 0, file.metadata()?.len() as usize + 1);
    /// assert!(map.is_err());
    /// # Ok(())
    /// # }
    /// ```
    #[deprecated(
        since = "0.4.0",
        note = "use Map::with_options().offset(off).len(len).map(f) instead"
    )]
    pub fn file(f: &File, offset: usize, length: usize) -> Result<Self> {
        Self::with_options().offset(offset).len(length).map(f)
    }

    /// Create a new map object from a maximum range of a file. Unlike `file`,
    /// the length is only a maximum size to map. If the length of the file
    /// is less than the requested range, the returned mapping will be
    /// shortened to match the file.
    ///
    /// # Examples
    ///
    /// ```
    /// # extern crate vmap;
    /// use vmap::Map;
    /// use std::fs::OpenOptions;
    /// use std::str::from_utf8;
    ///
    /// # fn main() -> vmap::Result<()> {
    /// let file = OpenOptions::new().read(true).open("README.md")?;
    /// let map = Map::file_max(&file, 0, 5000)?.expect("should be valid range");
    /// assert_eq!(map.is_empty(), false);
    /// assert_eq!(Ok("fast and safe memory-mapped IO"), from_utf8(&map[113..143]));
    ///
    /// let map = Map::file_max(&file, 0, file.metadata()?.len() as usize + 1);
    /// assert!(!map.is_err());
    ///
    /// let map = Map::file_max(&file, 5000, 100)?;
    /// assert!(map.is_none());
    /// # Ok(())
    /// # }
    /// ```
    #[deprecated(
        since = "0.4.0",
        note = "use Map::with_options().offset(off).len_max(len).map_if(f) instead"
    )]
    pub fn file_max(f: &File, offset: usize, max_length: usize) -> Result<Option<Self>> {
        Self::with_options()
            .offset(offset)
            .len_max(max_length)
            .map_if(f)
    }

    /// Transfer ownership of the map into a mutable map.
    ///
    /// This will change the protection of the mapping. If the original file
    /// was not opened with write permissions, this will error.
    ///
    /// # Examples
    ///
    /// ```
    /// # extern crate vmap;
    /// # extern crate tempdir;
    /// use vmap::Map;
    /// use std::io::Write;
    /// use std::fs::OpenOptions;
    /// use std::path::PathBuf;
    /// use std::str::from_utf8;
    /// # use std::fs;
    ///
    /// # fn main() -> vmap::Result<()> {
    /// # let tmp = tempdir::TempDir::new("vmap")?;
    /// let path: PathBuf = /* path to file */
    /// # tmp.path().join("into_map_mut");
    /// # fs::write(&path, b"this is a test")?;
    ///
    /// // Map the beginning of the file
    /// let map = Map::with_options().write().len(14).open(path)?;
    /// assert_eq!(Ok("this is a test"), from_utf8(&map[..]));
    ///
    /// let mut map = map.into_map_mut()?;
    /// {
    ///     let mut data = &mut map[..];
    ///     data.write_all(b"that")?;
    /// }
    /// assert_eq!(Ok("that is a test"), from_utf8(&map[..]));
    /// # Ok(())
    /// # }
    /// ```
    pub fn into_map_mut(self) -> ConvertResult<MapMut, Self> {
        unsafe {
            let (ptr, len) = Size::page().bounds(self.0.ptr, self.0.len);
            match protect(ptr, len, Protect::ReadWrite) {
                Ok(()) => Ok(self.0),
                Err(err) => Err((err, self)),
            }
        }
    }

    /// Transfer ownership of the map into a mutable map.
    ///
    /// This will change the protection of the mapping. If the original file
    /// was not opened with write permissions, this will error.
    ///
    /// This will cause the original map to be dropped if the protection change
    /// fails. Using `into_map_mut` allows the original map to be retained in the
    /// case of a failure.
    #[deprecated(since = "0.4.0", note = "use try_into or into_map_mut instead")]
    pub fn make_mut(self) -> Result<MapMut> {
        Ok(self.into_map_mut()?)
    }

    /// Updates the advise for the entire mapped region..
    pub fn advise(&self, access: AdviseAccess, usage: AdviseUsage) -> Result<()> {
        self.0.advise(access, usage)
    }

    /// Updates the advise for a specific range of the mapped region.
    pub fn advise_range(
        &self,
        off: usize,
        len: usize,
        access: AdviseAccess,
        usage: AdviseUsage,
    ) -> Result<()> {
        self.0.advise_range(off, len, access, usage)
    }

    /// Lock all mapped physical pages into memory.
    pub fn lock(&self) -> Result<()> {
        self.0.lock()
    }

    /// Lock a range of physical pages into memory.
    pub fn lock_range(&self, off: usize, len: usize) -> Result<()> {
        self.0.lock_range(off, len)
    }

    /// Unlock all mapped physical pages into memory.
    pub fn unlock(&self) -> Result<()> {
        self.0.unlock()
    }

    /// Unlock a range of physical pages into memory.
    pub fn unlock_range(&self, off: usize, len: usize) -> Result<()> {
        self.0.unlock_range(off, len)
    }
}

impl FromPtr for Map {
    unsafe fn from_ptr(ptr: *mut u8, len: usize) -> Self {
        Self(MapMut::from_ptr(ptr, len))
    }
}

impl Span for Map {
    #[inline]
    fn len(&self) -> usize {
        self.0.len()
    }

    #[inline]
    fn as_ptr(&self) -> *const u8 {
        self.0.as_ptr()
    }
}

impl Deref for Map {
    type Target = [u8];

    #[inline]
    fn deref(&self) -> &[u8] {
        unsafe { slice::from_raw_parts(self.0.ptr, self.0.len) }
    }
}

impl AsRef<[u8]> for Map {
    #[inline]
    fn as_ref(&self) -> &[u8] {
        self.deref()
    }
}

impl TryFrom<MapMut> for Map {
    type Error = (Error, MapMut);

    fn try_from(map: MapMut) -> ConvertResult<Self, MapMut> {
        map.into_map()
    }
}

impl fmt::Debug for Map {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.debug_struct("Map")
            .field("ptr", &self.0.ptr)
            .field("len", &self.0.len)
            .finish()
    }
}

/// Allocation of one or more read-write sequential pages.
#[derive(Debug)]
pub struct MapMut {
    ptr: *mut u8,
    len: usize,
}

impl MapMut {
    /// TODO
    pub fn with_options() -> Options<Self> {
        let mut opts = Options::new();
        opts.write();
        opts
    }

    /// Create a new anonymous mapping at least as large as the hint.
    ///
    /// # Examples
    ///
    /// ```
    /// # extern crate vmap;
    /// use vmap::{MapMut, Protect};
    /// use std::io::Write;
    /// use std::str::from_utf8;
    ///
    /// # fn main() -> vmap::Result<()> {
    /// let mut map = MapMut::new(200, Protect::ReadWrite)?;
    /// {
    ///     let mut data = &mut map[..];
    ///     assert!(data.len() >= 200);
    ///     data.write_all(b"test")?;
    /// }
    /// assert_eq!(Ok("test"), from_utf8(&map[..4]));
    /// # Ok(())
    /// # }
    /// ```
    #[deprecated(
        since = "0.4.0",
        note = "use MapMut::with_options().len_min(hint).protect(prot).alloc() instead"
    )]
    pub fn new(hint: usize, prot: Protect) -> Result<Self> {
        Self::with_options().len_min(hint).protect(prot).alloc()
    }

    /// Creates a new read/write map object using the full range of a file.
    ///
    /// # Examples
    /// ```
    /// # extern crate vmap;
    /// use vmap::MapMut;
    /// use std::fs::OpenOptions;
    /// use std::str::from_utf8;
    ///
    /// # fn main() -> vmap::Result<()> {
    /// let map = MapMut::open("README.md")?;
    /// assert_eq!(map.is_empty(), false);
    /// assert_eq!(Ok("fast and safe memory-mapped IO"), from_utf8(&map[113..143]));
    /// # Ok(())
    /// # }
    /// ```
    #[deprecated(
        since = "0.4.0",
        note = "use MapMut::with_options().open(path) instead"
    )]
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        Self::with_options().open(path)
    }

    /// Create a new mutable map object from a range of a file.
    #[deprecated(
        since = "0.4.0",
        note = "use MapMut::with_options().offset(off).len(len).map(f) instead"
    )]
    pub fn file(f: &File, offset: usize, length: usize) -> Result<Self> {
        Self::with_options().offset(offset).len(length).map(f)
    }

    /// Create a new mutable map object from a maximum range of a file. Unlike
    /// `file`, the length is only a maximum size to map. If the length of the
    /// file is less than the requested range, the returned mapping will be
    /// shortened to match the file.
    #[deprecated(
        since = "0.4.0",
        note = "use MapMut::with_options().offset(off).len_max(len).map_if(f) instead"
    )]
    pub fn file_max(f: &File, offset: usize, max_length: usize) -> Result<Option<Self>> {
        Self::with_options()
            .offset(offset)
            .len_max(max_length)
            .map_if(f)
    }

    /// Create a new private map object from a range of a file.
    ///
    /// Initially, the mapping will be shared with other processes, but writes
    /// will be kept private.
    ///
    /// # Examples
    ///
    /// ```
    /// # extern crate vmap;
    /// use vmap::MapMut;
    /// use std::io::Write;
    /// use std::fs::OpenOptions;
    /// use std::str::from_utf8;
    ///
    /// # fn main() -> vmap::Result<()> {
    /// let file = OpenOptions::new().read(true).open("README.md")?;
    /// let mut map = MapMut::copy(&file, 113, 30)?;
    /// assert_eq!(map.is_empty(), false);
    /// assert_eq!(Ok("fast and safe memory-mapped IO"), from_utf8(&map[..]));
    /// {
    ///     let mut data = &mut map[..];
    ///     data.write_all(b"nice")?;
    /// }
    /// assert_eq!(Ok("nice and safe memory-mapped IO"), from_utf8(&map[..]));
    /// # Ok(())
    /// # }
    /// ```
    #[deprecated(
        since = "0.4.0",
        note = "use MapMut::with_options().copy().offset(off).len(len).map(f) instead"
    )]
    pub fn copy(f: &File, offset: usize, length: usize) -> Result<Self> {
        Self::with_options()
            .copy()
            .offset(offset)
            .len(length)
            .map(f)
    }

    /// Create a new private map object from a range of a file.  Unlike
    /// `copy`, the length is only a maximum size to map. If the length of the
    /// file is less than the requested range, the returned mapping will be
    /// shortened to match the file.
    ///
    /// Initially, the mapping will be shared with other processes, but writes
    /// will be kept private.
    #[deprecated(
        since = "0.4.0",
        note = "use MapMut::with_options().copy().offset(off).len_max(len).map_if(f) instead"
    )]
    pub fn copy_max(f: &File, offset: usize, max_length: usize) -> Result<Option<Self>> {
        Self::with_options()
            .copy()
            .offset(offset)
            .len_max(max_length)
            .map_if(f)
    }

    /// Transfer ownership of the map into a mutable map.
    ///
    /// This will change the protection of the mapping. If the original file
    /// was not opened with write permissions, this will error.
    ///
    /// # Examples
    ///
    /// ```
    /// # extern crate vmap;
    /// # extern crate tempdir;
    /// use vmap::MapMut;
    /// use std::io::Write;
    /// use std::fs::OpenOptions;
    /// use std::path::PathBuf;
    /// use std::str::from_utf8;
    /// # use std::fs;
    ///
    /// # fn main() -> vmap::Result<()> {
    /// # let tmp = tempdir::TempDir::new("vmap")?;
    /// let path: PathBuf = /* path to file */
    /// # tmp.path().join("into_map_mut");
    /// # fs::write(&path, b"this is a test")?;
    /// let mut map = MapMut::with_options().len(14).open(&path)?;
    /// assert_eq!(Ok("this is a test"), from_utf8(&map[..]));
    /// {
    ///     let mut data = &mut map[..];
    ///     data.write_all(b"that")?;
    /// }
    ///
    /// let map = map.into_map()?;
    /// assert_eq!(Ok("that is a test"), from_utf8(&map[..]));
    /// # Ok(())
    /// # }
    /// ```
    pub fn into_map(self) -> ConvertResult<Map, Self> {
        unsafe {
            let (ptr, len) = Size::page().bounds(self.ptr, self.len);
            match protect(ptr, len, Protect::ReadWrite) {
                Ok(()) => Ok(Map(self)),
                Err(err) => Err((err, self)),
            }
        }
    }

    /// Transfer ownership of the map into a mutable map.
    ///
    /// This will change the protection of the mapping. If the original file
    /// was not opened with write permissions, this will error.
    ///
    /// This will cause the original map to be dropped if the protection change
    /// fails. Using `into_map` allows the original map to be retained in the
    /// case of a failure.
    #[deprecated(since = "0.4.0", note = "use try_into or into_map instead")]
    pub fn make_read_only(self) -> Result<Map> {
        Ok(self.into_map()?)
    }

    /// Writes modifications back to the filesystem.
    ///
    /// Flushes will happen automatically, but this will invoke a flush and
    /// return any errors with doing so.
    pub fn flush(&self, file: &File, mode: Flush) -> Result<()> {
        unsafe {
            let (ptr, len) = Size::page().bounds(self.ptr, self.len);
            flush(ptr, file, len, mode)
        }
    }

    /// Updates the advise for the entire mapped region..
    pub fn advise(&self, access: AdviseAccess, usage: AdviseUsage) -> Result<()> {
        unsafe {
            let (ptr, len) = Size::page().bounds(self.ptr, self.len);
            advise(ptr, len, access, usage)
        }
    }

    /// Updates the advise for a specific range of the mapped region.
    pub fn advise_range(
        &self,
        off: usize,
        len: usize,
        access: AdviseAccess,
        usage: AdviseUsage,
    ) -> Result<()> {
        if off + len > self.len {
            Err(Error::input(Operation::Advise, Input::InvalidRange))
        } else {
            unsafe {
                let (ptr, len) = Size::page().bounds(self.ptr.add(off), len);
                advise(ptr, len, access, usage)
            }
        }
    }

    /// Lock all mapped physical pages into memory.
    pub fn lock(&self) -> Result<()> {
        unsafe {
            let (ptr, len) = Size::page().bounds(self.ptr, self.len);
            lock(ptr, len)
        }
    }

    /// Lock a range of physical pages into memory.
    pub fn lock_range(&self, off: usize, len: usize) -> Result<()> {
        if off + len > self.len {
            Err(Error::input(Operation::Lock, Input::InvalidRange))
        } else {
            unsafe {
                let (ptr, len) = Size::page().bounds(self.ptr.add(off), len);
                lock(ptr, len)
            }
        }
    }

    /// Unlock all mapped physical pages into memory.
    pub fn unlock(&self) -> Result<()> {
        unsafe {
            let (ptr, len) = Size::page().bounds(self.ptr, self.len);
            unlock(ptr, len)
        }
    }

    /// Unlock a range of physical pages into memory.
    pub fn unlock_range(&self, off: usize, len: usize) -> Result<()> {
        if off + len > self.len {
            Err(Error::input(Operation::Unlock, Input::InvalidRange))
        } else {
            unsafe {
                let (ptr, len) = Size::page().bounds(self.ptr.add(off), len);
                unlock(ptr, len)
            }
        }
    }
}

impl FromPtr for MapMut {
    unsafe fn from_ptr(ptr: *mut u8, len: usize) -> Self {
        Self { ptr, len }
    }
}

impl Span for MapMut {
    #[inline]
    fn len(&self) -> usize {
        self.len
    }

    #[inline]
    fn as_ptr(&self) -> *const u8 {
        self.ptr
    }
}

impl SpanMut for MapMut {
    #[inline]
    fn as_mut_ptr(&mut self) -> *mut u8 {
        self.ptr
    }
}

impl Drop for MapMut {
    fn drop(&mut self) {
        unsafe {
            if self.len > 0 {
                let (ptr, len) = Size::allocation().bounds(self.ptr, self.len);
                unmap(ptr, len).unwrap_or_default();
            }
        }
    }
}

impl Deref for MapMut {
    type Target = [u8];

    #[inline]
    fn deref(&self) -> &[u8] {
        unsafe { slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl DerefMut for MapMut {
    #[inline]
    fn deref_mut(&mut self) -> &mut [u8] {
        unsafe { slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}

impl AsRef<[u8]> for MapMut {
    #[inline]
    fn as_ref(&self) -> &[u8] {
        self.deref()
    }
}

impl AsMut<[u8]> for MapMut {
    #[inline]
    fn as_mut(&mut self) -> &mut [u8] {
        self.deref_mut()
    }
}

impl TryFrom<Map> for MapMut {
    type Error = (Error, Map);

    fn try_from(map: Map) -> ConvertResult<Self, Map> {
        map.into_map_mut()
    }
}

enum Len {
    End,
    Exact(usize),
    Min(usize),
    Max(usize),
}

enum Resize {
    None,
    Exact(usize),
    AtLeast(usize),
}

/// TODO
pub struct Options<T> {
    open_options: OpenOptions,
    resize: Resize,
    len: Len,
    offset: usize,
    protect: Protect,
    truncate: bool,
    phantom: PhantomData<T>,
}

impl<T> Options<T>
where
    T: FromPtr,
{
    /// TODO
    pub fn new() -> Self {
        let mut open_options = OpenOptions::new();
        open_options.read(true);
        Self {
            open_options,
            resize: Resize::None,
            len: Len::End,
            offset: 0,
            protect: Protect::ReadOnly,
            truncate: false,
            phantom: PhantomData,
        }
    }

    /// TODO
    pub fn new_from(options: &OpenOptions) -> Self {
        let mut open_options = OpenOptions::new();
        open_options.clone_from(options);
        open_options.read(true);
        Self {
            open_options,
            resize: Resize::None,
            len: Len::End,
            offset: 0,
            protect: Protect::ReadOnly,
            truncate: false,
            phantom: PhantomData,
        }
    }

    /// TODO
    pub fn write(&mut self) -> &mut Self {
        self.protect(Protect::ReadWrite)
    }

    /// TODO
    pub fn copy(&mut self) -> &mut Self {
        self.protect(Protect::ReadCopy)
    }

    /// TODO
    pub fn protect(&mut self, protect: Protect) -> &mut Self {
        self.open_options.write(protect == Protect::ReadWrite);
        self.protect = protect;
        self
    }

    /// TODO
    pub fn create(&mut self, create: bool) -> &mut Self {
        self.open_options.create(create);
        self
    }

    /// TODO
    pub fn create_new(&mut self, create_new: bool) -> &mut Self {
        self.open_options.create_new(create_new);
        self
    }

    /// TODO
    pub fn truncate(&mut self, truncate: bool) -> &mut Self {
        self.open_options.truncate(truncate);
        self.truncate = truncate;
        self
    }

    /// TODO
    pub fn offset(&mut self, offset: usize) -> &mut Self {
        self.offset = offset;
        self
    }

    /// TODO
    pub fn len(&mut self, len: usize) -> &mut Self {
        self.len = Len::Exact(len);
        self
    }

    /// TODO
    pub fn len_min(&mut self, len_min: usize) -> &mut Self {
        self.len = Len::Min(len_min);
        self
    }

    /// TODO
    pub fn len_max(&mut self, len_max: usize) -> &mut Self {
        self.len = Len::Max(len_max);
        self
    }

    /// TODO
    pub fn resize(&mut self, resize: usize) -> &mut Self {
        self.resize = Resize::Exact(resize);
        self
    }

    /// TODO
    pub fn resize_min(&mut self, resize_min: usize) -> &mut Self {
        self.resize = Resize::AtLeast(resize_min);
        self
    }

    /// TODO
    pub fn open<P: AsRef<Path>>(&self, path: P) -> Result<T> {
        self.map(&self.open_options.open(path).map_err(map_file_err)?)
    }

    /// TODO
    pub fn open_if<P: AsRef<Path>>(&self, path: P) -> Result<Option<T>> {
        self.map_if(&self.open_options.open(path).map_err(map_file_err)?)
    }

    /// TODO
    pub fn map(&self, f: &File) -> Result<T> {
        self.map_if(f)?
            .ok_or_else(|| Error::input(Operation::MapFile, Input::InvalidRange))
    }

    /// TODO
    pub fn map_if(&self, f: &File) -> Result<Option<T>> {
        let off = self.offset;
        let md = f.metadata().map_err(map_file_err)?;

        let resize = |sz: usize| f.set_len(sz as u64).map(|_| sz).map_err(map_file_err);

        if self.truncate && md.len() > 0 {
            resize(0)?;
        }

        let flen = match self.resize {
            Resize::Exact(sz) => resize(sz)?,
            Resize::AtLeast(sz) if sz as u64 > md.len() => resize(sz)?,
            _ => md.len() as usize,
        };

        if flen < off {
            return Ok(None);
        }

        let max = flen - off;
        let len = match self.len {
            Len::Min(l) | Len::Exact(l) if l > max => return Ok(None),
            Len::Min(_) | Len::End => max,
            Len::Max(l) => cmp::min(l, max),
            Len::Exact(l) => l,
        };

        let mapoff = Size::allocation().truncate(off);
        let maplen = len + (off - mapoff);
        let ptr = map_file(f, mapoff, maplen, self.protect)?;
        unsafe { Ok(Some(T::from_ptr(ptr.add(off - mapoff), len))) }
    }

    /// TODO
    pub fn alloc(&self) -> Result<T> {
        let off = Size::page().offset(self.offset);
        let len = match self.len {
            Len::End => Size::allocation().round(off) - off,
            Len::Min(l) => Size::allocation().round(off + l) - off,
            Len::Max(l) | Len::Exact(l) => l,
        };

        let ptr = map_anon(off + len, self.protect)?;
        unsafe { Ok(T::from_ptr(ptr.add(off), len)) }
    }
}

fn map_file_err(e: io::Error) -> Error {
    Error::io(Operation::MapFile, e)
}
