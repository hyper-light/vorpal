//! Disk spill for the reference stream between commit and resolution.
//!
//! References are written once (append-only, already rebased) while the commit runs, and
//! read exactly once — sequentially, chunk by chunk — by resolution, then discarded. Holding
//! them in RAM buys nothing but peak footprint: at kernel scale the buffered vector was
//! ~220 MB sitting under every later phase's live set. Spilling turns that into a ~200 MB
//! temp file written behind a `BufWriter` and a streaming read whose in-flight memory is a
//! few chunks.
//!
//! Records are fixed-width 34-byte little-endian, portable by construction, but the file is
//! **process-private**: it stores interned `NameId` bits, which are meaningless outside the
//! process that wrote them (and are never stable across runs). Create, read, delete — never
//! persist.

use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use vorpal_kg::NodeId;

use crate::intern::{Interner, NameId};
use crate::reference::{RefForm, RefKind, Reference};

/// Bytes per spilled reference record.
const RECORD: usize = 34;

/// References per read chunk (~1 MB in flight per chunk).
pub const SPILL_CHUNK: usize = 32_768;

fn kind_tag(kind: RefKind) -> u8 {
  match kind {
    RefKind::Call => 0,
    RefKind::Type => 1,
    RefKind::Import => 2,
    RefKind::Implements => 3,
    RefKind::Use => 4,
  }
}

fn kind_of(tag: u8) -> RefKind {
  match tag {
    0 => RefKind::Call,
    1 => RefKind::Type,
    2 => RefKind::Import,
    3 => RefKind::Implements,
    _ => RefKind::Use,
  }
}

fn form_tag(form: RefForm) -> u8 {
  match form {
    RefForm::Bare => 0,
    RefForm::Static => 1,
    RefForm::Method => 2,
    RefForm::MethodHinted => 3,
  }
}

fn form_of(tag: u8) -> RefForm {
  match tag {
    1 => RefForm::Static,
    2 => RefForm::Method,
    3 => RefForm::MethodHinted,
    _ => RefForm::Bare,
  }
}

fn encode(reference: &Reference, buf: &mut [u8; RECORD]) {
  buf[0..8].copy_from_slice(&reference.from.raw().to_le_bytes());
  buf[8..12].copy_from_slice(&reference.name.to_bits().to_le_bytes());
  buf[12..16].copy_from_slice(&reference.from_path.to_bits().to_le_bytes());
  // `NameId` bits are never 0 (biased `NonZeroU32`), so 0 is a safe "no qualifier" sentinel.
  let qualifier = reference.qualifier.map(NameId::to_bits).unwrap_or(0);
  buf[16..20].copy_from_slice(&qualifier.to_le_bytes());
  buf[20..24].copy_from_slice(&reference.evidence.0.to_le_bytes());
  buf[24..28].copy_from_slice(&reference.evidence.1.to_le_bytes());
  buf[28] = kind_tag(reference.kind);
  buf[29] = form_tag(reference.form);
  let alias = reference.alias.map(NameId::to_bits).unwrap_or(0);
  buf[30..34].copy_from_slice(&alias.to_le_bytes());
}

fn decode<'i>(interner: &'i Interner, buf: &[u8; RECORD]) -> Reference<'i> {
  let u64_at = |i: usize| u64::from_le_bytes(buf[i..i + 8].try_into().unwrap());
  let u32_at = |i: usize| u32::from_le_bytes(buf[i..i + 4].try_into().unwrap());
  Reference {
    from: NodeId::new(u64_at(0)),
    name: interner
      .id_from_bits(u32_at(8))
      .expect("spill wrote a valid name id"),
    from_path: interner
      .id_from_bits(u32_at(12))
      .expect("spill wrote a valid path id"),
    qualifier: interner.id_from_bits(u32_at(16)),
    evidence: (u32_at(20), u32_at(24)),
    kind: kind_of(buf[28]),
    form: form_of(buf[29]),
    alias: interner.id_from_bits(u32_at(30)),
  }
}

/// Append-only spill writer, buffered. `finish` flushes and hands back the read handle.
pub struct RefSpillWriter<'i> {
  interner: &'i Interner,
  out: BufWriter<File>,
  path: PathBuf,
  count: u64,
  qualified_imports: Vec<Reference<'i>>,
}

impl<'i> RefSpillWriter<'i> {
  pub fn create(interner: &'i Interner, path: &Path) -> io::Result<Self> {
    Ok(Self {
      interner,
      out: BufWriter::new(File::create(path)?),
      path: path.to_path_buf(),
      count: 0,
      qualified_imports: Vec::new(),
    })
  }

  pub fn push(&mut self, reference: &Reference<'i>) -> io::Result<()> {
    // Qualifier-carrying imports are additionally retained in RAM: the link phase resolves
    // them *first* to seed per-file import bindings (§3.3 scope step), and re-decoding the
    // whole spill for a pre-pass would cost a second full stream. They are rare — ~0.1% of
    // references at kernel scale — so the retained vector is a few hundred KB, not the
    // ~220 MB the spill exists to avoid.
    if reference.kind == RefKind::Import && reference.form == RefForm::Static {
      self.qualified_imports.push(*reference);
    }
    let mut buf = [0u8; RECORD];
    encode(reference, &mut buf);
    self.out.write_all(&buf)?;
    self.count += 1;
    Ok(())
  }

  pub fn finish(mut self) -> io::Result<RefSpill<'i>> {
    self.out.flush()?;
    Ok(RefSpill {
      interner: self.interner,
      path: self.path,
      count: self.count,
      qualified_imports: self.qualified_imports,
    })
  }
}

/// A finished spill: read it once with [`RefSpill::chunks`], then [`RefSpill::remove`] it.
pub struct RefSpill<'i> {
  interner: &'i Interner,
  path: PathBuf,
  count: u64,
  qualified_imports: Vec<Reference<'i>>,
}

impl<'i> RefSpill<'i> {
  pub fn count(&self) -> u64 {
    self.count
  }

  /// The qualifier-carrying import references, in write order — the input of the link
  /// phase's import-binding pre-pass. Also present in the spilled stream itself (this is a
  /// retained copy, not a diversion), so chunked resolution still sees every reference.
  pub fn qualified_imports(&self) -> &[Reference<'i>] {
    &self.qualified_imports
  }

  /// Sequential chunk reader over the spilled records, in write order.
  pub fn chunks(&self) -> io::Result<RefSpillChunks<'i>> {
    Ok(RefSpillChunks {
      interner: self.interner,
      reader: BufReader::with_capacity(1 << 20, File::open(&self.path)?),
      remaining: self.count,
    })
  }

  /// Delete the spill file. Errors are the caller's to ignore where cleanup is best-effort.
  pub fn remove(self) -> io::Result<()> {
    std::fs::remove_file(&self.path)
  }
}

/// Iterator of `Vec<Reference>` chunks (each `SPILL_CHUNK` long except the last).
pub struct RefSpillChunks<'i> {
  interner: &'i Interner,
  reader: BufReader<File>,
  remaining: u64,
}

impl<'i> Iterator for RefSpillChunks<'i> {
  type Item = io::Result<Vec<Reference<'i>>>;

  fn next(&mut self) -> Option<Self::Item> {
    if self.remaining == 0 {
      return None;
    }
    let take = (self.remaining as usize).min(SPILL_CHUNK);
    let mut chunk = Vec::with_capacity(take);
    let mut buf = [0u8; RECORD];
    for _ in 0..take {
      if let Err(err) = self.reader.read_exact(&mut buf) {
        return Some(Err(err));
      }
      chunk.push(decode(self.interner, &buf));
    }
    self.remaining -= take as u64;
    Some(Ok(chunk))
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// One shared session for the whole test binary: tests only ever intern a bounded
  /// vocabulary, and `'static` ids keep the assertions free of lifetime plumbing.
  fn itn() -> &'static Interner {
    static INTERNER: std::sync::OnceLock<Interner> = std::sync::OnceLock::new();
    INTERNER.get_or_init(Interner::new)
  }

  #[test]
  fn roundtrips_references_exactly() {
    let refs: Vec<Reference<'static>> = (0..200_000u64)
      .map(|i| {
        let mut r = Reference::new(
          itn(),
          NodeId::new(i),
          &format!("src/file_{}.rs", i % 97),
          &format!("name_{}", i % 1013),
          match i % 5 {
            0 => RefKind::Call,
            1 => RefKind::Type,
            2 => RefKind::Import,
            3 => RefKind::Implements,
            _ => RefKind::Use,
          },
        )
        .with_evidence(i as u32, i as u32 + 7);
        if i % 3 == 0 {
          r = r.with_qualifier(itn(), Some(format!("Owner{}", i % 11)));
        }
        if i % 4 == 1 {
          r = r.with_form(RefForm::Static);
        } else if i % 4 == 2 {
          r = r.with_form(RefForm::Method);
        }
        r
      })
      .collect();

    let dir = std::env::temp_dir().join(format!("vorpal-spill-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("refs.spill");
    let mut writer = RefSpillWriter::create(itn(), &path).unwrap();
    for r in &refs {
      writer.push(r).unwrap();
    }
    let spill = writer.finish().unwrap();
    assert_eq!(spill.count(), refs.len() as u64);

    let mut back = Vec::new();
    for chunk in spill.chunks().unwrap() {
      back.extend(chunk.unwrap());
    }
    assert_eq!(back, refs);
    spill.remove().unwrap();
    let _ = std::fs::remove_dir(&dir);
  }
}
