//! Typed columns over owned vectors or mapped file sections (§9.1's zero-copy read form).
//!
//! A sealed on-disk column is just its elements' little-endian bytes; on little-endian
//! targets a [`PodColumn`] over a [`MappedStore`] section is a pointer cast — no read, no
//! allocation, no per-element work — and the pages fault in lazily as queries touch them.
//! Big-endian targets take the owned, per-element-decoded fallback (house style: the same
//! split `read_le_slice`-style loaders use). Writers build the identical column as an owned
//! vector, so build and load code paths share every accessor.

use std::ops::Deref;
use std::sync::Arc;

use crate::MappedStore;

/// A `[T]` column backed by either an owned vector (build form) or a mapped section of a
/// sealed file (read form).
pub struct PodColumn<T: bytemuck::Pod> {
  inner: Inner<T>,
}

enum Inner<T> {
  Owned(Vec<T>),
  Mapped {
    /// Keeps the mapping alive for as long as the column exists.
    _store: Arc<MappedStore>,
    ptr: *const T,
    len: usize,
  },
}

// SAFETY: the mapped variant points into an immutable, read-only mapping owned (via Arc) by
// the column itself; `T: Pod` carries no interior mutability or drop glue.
unsafe impl<T: bytemuck::Pod + Send> Send for PodColumn<T> {}
unsafe impl<T: bytemuck::Pod + Sync> Sync for PodColumn<T> {}

impl<T: bytemuck::Pod> PodColumn<T> {
  pub fn from_vec(values: Vec<T>) -> Self {
    Self {
      inner: Inner::Owned(values),
    }
  }

  /// View `bytes[range]` of `store` as a `[T]` column.
  ///
  /// Zero-copy on little-endian targets (validated for bounds, element-size divisibility,
  /// and alignment). On big-endian targets, decodes an owned copy with `decode` so the
  /// numeric values are identical everywhere.
  pub fn from_mapped_le<const W: usize>(
    store: &Arc<MappedStore>,
    offset: usize,
    len_bytes: usize,
    decode: fn([u8; W]) -> T,
  ) -> std::io::Result<Self> {
    debug_assert_eq!(W, std::mem::size_of::<T>());
    let bytes = store.as_bytes();
    let end = offset
      .checked_add(len_bytes)
      .filter(|&end| end <= bytes.len())
      .ok_or_else(|| std::io::Error::other("column section out of bounds"))?;
    if len_bytes % std::mem::size_of::<T>() != 0 {
      return Err(std::io::Error::other("column section not element-sized"));
    }
    let section = &bytes[offset..end];
    if cfg!(target_endian = "little") {
      let ptr = section.as_ptr();
      if ptr.align_offset(std::mem::align_of::<T>()) != 0 {
        return Err(std::io::Error::other("column section misaligned"));
      }
      Ok(Self {
        inner: Inner::Mapped {
          _store: store.clone(),
          ptr: ptr.cast(),
          len: len_bytes / std::mem::size_of::<T>(),
        },
      })
    } else {
      Ok(Self::from_vec(
        section
          .chunks_exact(W)
          .map(|chunk| decode(chunk.try_into().expect("exact chunk")))
          .collect(),
      ))
    }
  }
}

impl<T: bytemuck::Pod> Deref for PodColumn<T> {
  type Target = [T];

  fn deref(&self) -> &[T] {
    match &self.inner {
      Inner::Owned(values) => values,
      // SAFETY: constructed from a validated, aligned, in-bounds section of a mapping that
      // `_store` keeps alive; the file is sealed (never truncated or mutated while mapped).
      Inner::Mapped { ptr, len, .. } => unsafe { std::slice::from_raw_parts(*ptr, *len) },
    }
  }
}

impl<T: bytemuck::Pod + std::fmt::Debug> std::fmt::Debug for PodColumn<T> {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_list().entries(self.iter()).finish()
  }
}

impl<T: bytemuck::Pod> Default for PodColumn<T> {
  fn default() -> Self {
    Self::from_vec(Vec::new())
  }
}

impl<T: bytemuck::Pod + PartialEq> PartialEq for PodColumn<T> {
  fn eq(&self, other: &Self) -> bool {
    self[..] == other[..]
  }
}

impl<T: bytemuck::Pod> Clone for PodColumn<T> {
  fn clone(&self) -> Self {
    // Cloning materializes: clones are for tests and small structures, not the bulk path.
    Self::from_vec(self.to_vec())
  }
}
