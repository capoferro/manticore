// Copyright lowRISC contributors.
// Licensed under the Apache License, Version 2.0, see LICENSE for details.
// SPDX-License-Identifier: Apache-2.0

//! Provides the [`Read`] trait, analogous to [`std::io::Read`].

#![allow(unsafe_code)]

use core::alloc::Layout;
use core::mem;

use zerocopy::AsBytes;
use zerocopy::FromBytes;
use zerocopy::LayoutVerified;

use crate::io;
use crate::io::endian::LeInt;
use crate::mem::misalign_of;
use crate::mem::Arena;

#[cfg(doc)]
use crate::mem::ArenaExt;

/// Represents a place that bytes can be read from, such as a `&[u8]`.
///
/// # Relation with [`std::io::Read`]
/// [`std::io::Read`] is distinct from `Read`, since `Read` must know,
/// a-priori, the total length of the underlying buffer.
///
/// The recommended way to use a [`std::io::Read`] with a `manticore` API is to
/// use `read_to_end(&mut buf)` and to then pass `&mut buf[..]` into
/// `manticore`. We hope to remove this restriction in the future.
pub trait Read {
    /// Reads exactly `n` bytes from `self`.
    fn read_bytes(&mut self, out: &mut [u8]) -> Result<(), io::Error>;

    /// Returns the number of bytes still available to read.
    fn remaining_data(&self) -> usize;
}
impl dyn Read {} // Ensure object-safety.

/// Convenience functions for reading integers from a [`Read`].
#[extend::ext(name = ReadInt)]
pub impl<R: Read + ?Sized> R {
    /// Reads a little-endian integer.
    #[inline]
    fn read_le<I: LeInt>(&mut self) -> Result<I, io::Error> {
        I::read_from(self)
    }
}

/// A [`Read`] that may, as an optimization, zero-copy read data for the
/// lifetime `'a`.
///
/// Most implementations can get away with `impl ReadZero<'_> for MyReader {}`.
/// This will make it fall back on a copying operation.
///
/// # Safety
///
/// Buffers returned by `read_direct()` must have the size and alignment
/// constraints specified by the `layout` argument. The default implementation
/// does this automatically.
pub unsafe trait ReadZero<'a>: Read + 'a {
    /// Performs a zero-copy-optimizable read, falling back to copying onto
    /// `arena` if necessary.
    ///
    /// This function provides an optimization opportunity to implementations.
    /// Some implementations, such as `&[u8]`, already hold all of their
    /// contents in memory and, thus, can return a reference into themselves.
    #[inline]
    fn read_direct(
        &mut self,
        arena: &'a dyn Arena,
        layout: Layout,
    ) -> Result<&'a [u8], io::Error> {
        let out = arena
            .alloc_raw(layout)
            .map_err(|_| io::Error::BufferExhausted)?;
        self.read_bytes(out)?;
        Ok(out)
    }
}

/// Convenience functions for direct reads, exposed as a trait.
#[extend::ext(name = ReadZeroExt)]
pub impl<'a, R: ReadZero<'a> + ?Sized> R {
    /// Reads a value of type `T`.
    ///
    /// See [`ArenaExt::alloc()`].
    fn read_object<T: AsBytes + FromBytes + Copy>(
        &mut self,
        arena: &'a dyn Arena,
    ) -> Result<&'a T, io::Error> {
        let bytes = self.read_direct(arena, Layout::new::<T>())?;
        let lv = LayoutVerified::<_, T>::new(bytes)
            .expect("read_direct() implemented incorrectly");
        Ok(lv.into_ref())
    }

    /// Reads a slice of type `[T]`.
    ///
    /// See [`ArenaExt::alloc_slice()`].
    fn read_slice<T: AsBytes + FromBytes + Copy>(
        &mut self,
        n: usize,
        arena: &'a dyn Arena,
    ) -> Result<&'a [T], io::Error> {
        let layout = Layout::array::<T>(n).map_err(|_| io::Error::Internal)?;
        let bytes = self.read_direct(arena, layout)?;
        let lv = LayoutVerified::<_, [T]>::new_slice(bytes)
            .expect("read_direct() implemented incorrectly");
        Ok(lv.into_slice())
    }
}

impl<R: Read + ?Sized> Read for &mut R {
    #[inline]
    fn read_bytes(&mut self, out: &mut [u8]) -> Result<(), io::Error> {
        R::read_bytes(*self, out)
    }

    #[inline]
    fn remaining_data(&self) -> usize {
        R::remaining_data(*self)
    }
}

impl Read for &[u8] {
    fn read_bytes(&mut self, out: &mut [u8]) -> Result<(), io::Error> {
        let n = out.len();
        if self.len() < n {
            return Err(io::Error::BufferExhausted);
        }

        out.copy_from_slice(&self[..n]);
        *self = &self[n..];
        Ok(())
    }

    fn remaining_data(&self) -> usize {
        self.len()
    }
}

unsafe impl<'a, 'b: 'a> ReadZero<'a> for &'b [u8] {
    fn read_direct(
        &mut self,
        arena: &'a dyn Arena,
        layout: Layout,
    ) -> Result<&'a [u8], io::Error> {
        if self.len() < layout.size() {
            return Err(io::Error::BufferExhausted);
        }

        if misalign_of(self.as_ptr() as usize, layout.align()) == 0 {
            let (out, rest) = self.split_at(layout.size());
            *self = rest;
            return Ok(out);
        }

        let out = arena
            .alloc_raw(layout)
            .map_err(|_| io::Error::BufferExhausted)?;
        self.read_bytes(out)?;
        Ok(out)
    }
}

impl Read for &mut [u8] {
    fn read_bytes(&mut self, out: &mut [u8]) -> Result<(), io::Error> {
        let n = out.len();
        if self.len() < n {
            return Err(io::Error::BufferExhausted);
        }

        out.copy_from_slice(&self[..n]);
        let buf = mem::replace(self, &mut []);
        *self = &mut buf[n..];
        Ok(())
    }

    fn remaining_data(&self) -> usize {
        self.len()
    }
}

unsafe impl<'a, 'b: 'a> ReadZero<'a> for &'b mut [u8] {
    fn read_direct(
        &mut self,
        arena: &'a dyn Arena,
        layout: Layout,
    ) -> Result<&'a [u8], io::Error> {
        if self.len() < layout.size() {
            return Err(io::Error::BufferExhausted);
        }

        if misalign_of(self.as_ptr() as usize, layout.align()) == 0 {
            let buf = mem::replace(self, &mut []);
            let (out, rest) = buf.split_at_mut(layout.size());
            *self = rest;
            return Ok(out);
        }

        let out = arena
            .alloc_raw(layout)
            .map_err(|_| io::Error::BufferExhausted)?;
        self.read_bytes(out)?;
        Ok(out)
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn read_bytes() {
        let mut bytes: &[u8] = b"Hello!";
        let mut three_bytes = [0; 3];
        bytes.read_bytes(&mut three_bytes).unwrap();
        assert_eq!(&three_bytes[..], b"Hel");
        assert_eq!(bytes.len(), 3);
        assert_eq!(bytes.read_le::<u16>().unwrap(), 0x6f6c);
        assert_eq!(bytes.len(), 1);
        assert!(bytes.read_le::<u32>().is_err());
    }
}
