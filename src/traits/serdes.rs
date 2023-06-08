use crate::utils::*;
use anyhow::Result;
use std::{
    io::{Read, Seek, Write},
    mem::MaybeUninit,
    ops::{Deref, DerefMut},
    path::Path,
    ptr::addr_of_mut,
};

/// Possible backends of a [`MemCase`]. The `None` variant is used when the data structure is
/// created in memory; the `Memory` variant is used when the data structure is deserialized
/// from a file loaded into allocated memory; the `Mmap` variant is used when
/// the data structure is deserialized from a memory-mapped file.
pub enum Backend {
    /// No backend. The data structure is a standard Rust data structure.
    None,
    /// The backend is an allocated memory region.
    Memory(Vec<u64>),
    /// The backend is a memory-mapped file.
    Mmap(mmap_rs::Mmap),
}
/// A wrapper keeping together a data structure and the memory
/// it was deserialized from. It is specifically designed for
/// the case of memory-mapped files, where the mapping must
/// be kept alive for the whole lifetime of the data structure.
/// It can also be used with data structures deserialized from
/// memory, although in that case it is not strictly necessary
/// (cloning each field would work); nonetheless, reading a
/// single block of memory with [`Read::read_exact`] can be
/// very fast, and using [`load`] is a way to ensure that
/// no cloning is performed.
///
/// [`MemCase`] implements [`Deref`] and [`DerefMut`] to the
/// wrapped type, so it can be used almost transparently. However,
/// if you need to use a memory-mapped structure as a field in
/// a struct and you want to avoid `dyn`, you will have
/// to use [`MemCase`] as the type of the field.
/// [`MemCase`] implements [`From`] for the
/// wrapped type, using the no-op [None](`Backend::None`) variant
/// of [`Backend`], so a data structure can be [encased](encase)
/// almost transparently.
pub struct MemCase<S>(S, Backend);

unsafe impl<S: Send> Send for MemCase<S> {}
unsafe impl<S: Sync> Sync for MemCase<S> {}

impl<S> AsRef<S> for MemCase<S> {
    fn as_ref(&self) -> &S {
        &self.0
    }
}

impl<S> AsMut<S> for MemCase<S> {
    fn as_mut(&mut self) -> &mut S {
        &mut self.0
    }
}

impl<S> Deref for MemCase<S> {
    type Target = S;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<S> DerefMut for MemCase<S> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<S: Send + Sync> From<S> for MemCase<S> {
    fn from(s: S) -> Self {
        encase(s)
    }
}

/// Encases a data structure in a [`MemCase`] with no backend.
pub fn encase<S>(s: S) -> MemCase<S> {
    MemCase(s, Backend::None)
}

/// Mamory map a file and deserialize a data structure from it,
/// returning a [`MemCase`] containing the data structure and the
/// memory mapping.
#[allow(clippy::uninit_vec)]
pub fn map<'a, P: AsRef<Path>, S: Deserialize<'a>>(path: P) -> Result<MemCase<S>> {
    let file_len = path.as_ref().metadata()?.len();
    let file = std::fs::File::open(path)?;

    Ok({
        let mut uninit: MaybeUninit<MemCase<S>> = MaybeUninit::uninit();
        let ptr = uninit.as_mut_ptr();

        let mmap = unsafe {
            mmap_rs::MmapOptions::new(file_len as _)?
                .with_file(file, 0)
                .map()?
        };

        unsafe {
            addr_of_mut!((*ptr).1).write(Backend::Mmap(mmap));
        }

        if let Backend::Mmap(mmap) = unsafe { &(*ptr).1 } {
            let (s, _) = S::deserialize(mmap)?;
            unsafe {
                addr_of_mut!((*ptr).0).write(s);
            }

            unsafe { uninit.assume_init() }
        } else {
            unreachable!()
        }
    })
}

/// Load a file into memory and deserialize a data structure from it,
/// returning a [`MemCase`] containing the data structure and the
/// memory.
#[allow(clippy::uninit_vec)]
pub fn load<'a, P: AsRef<Path>, S: Deserialize<'a>>(path: P) -> Result<MemCase<S>> {
    let file_len = path.as_ref().metadata()?.len() as usize;
    let mut file = std::fs::File::open(path)?;
    let capacity = (file_len + 7) / 8;
    let mut mem = Vec::<u64>::with_capacity(capacity);
    unsafe {
        // This is safe because we are filling the vector
        // reading from a file.
        mem.set_len(capacity);
    }
    Ok({
        let mut uninit: MaybeUninit<MemCase<S>> = MaybeUninit::uninit();
        let ptr = uninit.as_mut_ptr();

        unsafe {
            addr_of_mut!((*ptr).1).write(Backend::Memory(mem));
        }

        if let Backend::Memory(mem) = unsafe { &mut (*ptr).1 } {
            let bytes: &mut [u8] = bytemuck::cast_slice_mut::<u64, u8>(mem);
            file.read_exact(&mut bytes[..file_len])?;
            // Fixes the last few bytes to guarantee zero-extension semantics
            // for bit vectors.
            bytes[file_len..].fill(0);

            let (s, _) = S::deserialize(bytes)?;

            unsafe {
                addr_of_mut!((*ptr).0).write(s);
            }

            unsafe { uninit.assume_init() }
        } else {
            unreachable!()
        }
    })
}

/// Mamory map a file and deserialize a data structure from it,
/// returning a [`MemCase`] containing the data structure and the
/// memory mapping.
pub fn map_slice<'a, P: AsRef<Path>, T: bytemuck::Pod>(path: P) -> Result<MemCase<&'a [T]>> {
    let file_len = path.as_ref().metadata()?.len();
    let file = std::fs::File::open(path)?;

    Ok({
        let mut uninit: MaybeUninit<MemCase<&'a [T]>> = MaybeUninit::uninit();
        let ptr = uninit.as_mut_ptr();

        let mmap = unsafe {
            mmap_rs::MmapOptions::new(file_len as _)?
                .with_file(file, 0)
                .map()?
        };

        unsafe {
            addr_of_mut!((*ptr).1).write(Backend::Mmap(mmap));
        }

        if let Backend::Mmap(mmap) = unsafe { &(*ptr).1 } {
            let s = bytemuck::cast_slice::<u8, T>(mmap);
            unsafe {
                addr_of_mut!((*ptr).0).write(s);
            }

            unsafe { uninit.assume_init() }
        } else {
            unreachable!()
        }
    })
}

/// Load a file into memory and deserialize a data structure from it,
/// returning a [`MemCase`] containing the data structure and the
/// memory.
#[allow(clippy::uninit_vec)]
pub fn load_slice<'a, P: AsRef<Path>, T: bytemuck::Pod>(path: P) -> Result<MemCase<&'a [T]>> {
    let file_len = path.as_ref().metadata()?.len() as usize;
    let mut file = std::fs::File::open(path)?;
    let capacity = (file_len + 7) / 8;
    let mut mem = Vec::<u64>::with_capacity(capacity);
    unsafe {
        // This is safe because we are filling the vector
        // reading from a file.
        mem.set_len(capacity);
    }
    Ok({
        let mut uninit: MaybeUninit<MemCase<&'a [T]>> = MaybeUninit::uninit();
        let ptr = uninit.as_mut_ptr();

        unsafe {
            addr_of_mut!((*ptr).1).write(Backend::Memory(mem));
        }

        if let Backend::Memory(mem) = unsafe { &mut (*ptr).1 } {
            let bytes: &mut [u8] = bytemuck::cast_slice_mut::<u64, u8>(mem);
            file.read_exact(&mut bytes[..file_len])?;
            // Fixes the last few bytes to guarantee zero-extension semantics
            // for bit vectors.
            bytes[file_len..].fill(0);
            let s: &mut [T] = bytemuck::cast_slice_mut::<u8, T>(bytes);

            unsafe {
                addr_of_mut!((*ptr).0).write(s);
            }

            unsafe { uninit.assume_init() }
        } else {
            unreachable!()
        }
    })
}

pub trait Serialize {
    fn serialize<F: Write + Seek>(&self, backend: &mut F) -> Result<usize>;
}

pub trait Deserialize<'a>: Sized {
    /// a function that return a deserialzied values that might contains
    /// references to the backend
    fn deserialize(backend: &'a [u8]) -> Result<(Self, &'a [u8])>;
}

macro_rules! impl_stuff{
    ($($ty:ty),*) => {$(

impl Serialize for $ty {
    #[inline(always)]
    fn serialize<F: Write>(&self, backend: &mut F) -> Result<usize> {
        Ok(backend.write(&self.to_ne_bytes())?)
    }
}

impl<'a> Deserialize<'a> for $ty {
    #[inline(always)]
    fn deserialize(backend: &'a [u8]) -> Result<(Self, &'a [u8])> {
        Ok((
            <$ty>::from_ne_bytes(backend[..core::mem::size_of::<$ty>()].try_into().unwrap()),
            &backend[core::mem::size_of::<$ty>()..],
        ))
    }
}
        impl<'a> Deserialize<'a> for &'a [$ty] {
            fn deserialize(backend: &'a [u8]) -> Result<(Self, &'a [u8])> {
                let (len, backend) = usize::deserialize(backend)?;
                let bytes = len * core::mem::size_of::<$ty>();
                let (_pre, data, after) = unsafe { backend[..bytes].align_to() };
                // TODO make error / we added padding so it's ok
                assert!(after.is_empty());
                Ok((data, &backend[bytes..]))
            }
        }
    )*};
}

impl_stuff!(usize, u8, u16, u32, u64);

impl<T: Serialize> Serialize for Vec<T> {
    fn serialize<F: Write + Seek>(&self, backend: &mut F) -> Result<usize> {
        let len = self.len();
        let mut bytes = 0;
        bytes += backend.write(&len.to_ne_bytes())?;
        // ensure alignement
        let file_pos = backend.stream_position()? as usize;
        for _ in 0..pad_align_to(file_pos, core::mem::size_of::<T>()) {
            bytes += backend.write(&[0])?;
        }
        // write the values
        for item in self {
            bytes += item.serialize(backend)?;
        }
        Ok(bytes)
    }
}
