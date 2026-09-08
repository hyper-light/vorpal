//! Peak memory the way the process itself can report it: the **private** high-water mark
//! (what the process owns and cannot hand back) beside the resident one (which also counts
//! file-backed pages the kernel may reclaim — mmapped artifacts, page cache). Indexing a tree
//! whose artifacts are mmapped makes resident overstate the footprint, sometimes by a lot;
//! tgrep measured a 2 GiB file at 1.99 GiB resident against 77.8 MiB private. Both are
//! reported, named, so a reader compares like with like.
//!
//! macOS: `proc_pid_rusage(RUSAGE_INFO_V4).ri_lifetime_max_phys_footprint` is exactly the
//! "peak memory footprint" `/usr/bin/time -l` prints; `ru_maxrss` (bytes on macOS) is its
//! "maximum resident set size". Linux: `VmHWM` from `/proc/self/status` is the resident peak;
//! there is no anonymous high-water mark, so the private peak is the maximum `RssAnon` seen
//! by [`sample_private`] (called at phase seams) — labelled `sampled`. Windows: `None`.

use std::sync::atomic::AtomicU64;

/// A process's memory high-water marks in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeakMemory {
  /// Private (anonymous) peak: `None` where the platform cannot say.
  pub private_peak: Option<u64>,
  /// Resident-set peak, file-backed pages included.
  pub rss_peak: Option<u64>,
  /// How `private_peak` was obtained: `exact`, `sampled`, or `unavailable`.
  pub method: &'static str,
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
static PRIVATE_HWM: AtomicU64 = AtomicU64::new(0);

/// Fold the current private (anonymous) resident size into the sampled high-water mark.
/// Cheap; call at phase boundaries. A no-op where the platform reports the peak itself.
pub fn sample_private() {
  #[cfg(target_os = "linux")]
  {
    if let Some(anon) = linux::status_field("RssAnon:") {
      PRIVATE_HWM.fetch_max(anon, std::sync::atomic::Ordering::Relaxed);
    }
  }
}

/// The process's peak memory so far.
pub fn peak_memory() -> PeakMemory {
  #[cfg(target_os = "macos")]
  {
    return macos::peak();
  }
  #[cfg(target_os = "linux")]
  {
    return linux::peak();
  }
  #[allow(unreachable_code)]
  PeakMemory {
    private_peak: None,
    rss_peak: None,
    method: "unavailable",
  }
}

/// `4.8 GB`, `612 MB`, `3.1 KB` — decimal units, one decimal above a gigabyte.
pub fn format_bytes(bytes: u64) -> String {
  const GB: f64 = 1e9;
  const MB: f64 = 1e6;
  const KB: f64 = 1e3;
  let b = bytes as f64;
  if b >= GB {
    format!("{:.1} GB", b / GB)
  } else if b >= MB {
    format!("{:.0} MB", b / MB)
  } else if b >= KB {
    format!("{:.0} KB", b / KB)
  } else {
    format!("{bytes} B")
  }
}

/// One line for a report: `peak memory: 4.8 GB private, 6.1 GB resident`.
pub fn describe(peak: &PeakMemory) -> String {
  match (peak.private_peak, peak.rss_peak) {
    (Some(private), Some(rss)) => format!(
      "peak memory: {} private{}, {} resident",
      format_bytes(private),
      if peak.method == "sampled" { " (sampled)" } else { "" },
      format_bytes(rss)
    ),
    (None, Some(rss)) => format!("peak memory: {} resident (private peak unavailable on this platform)", format_bytes(rss)),
    (Some(private), None) => format!("peak memory: {} private", format_bytes(private)),
    (None, None) => "peak memory: unavailable on this platform".to_string(),
  }
}

#[cfg(target_os = "macos")]
mod macos {
  use super::PeakMemory;

  pub fn peak() -> PeakMemory {
    let rss_peak = unsafe {
      let mut usage: libc::rusage = std::mem::zeroed();
      if libc::getrusage(libc::RUSAGE_SELF, &mut usage) == 0 {
        Some(usage.ru_maxrss as u64) // bytes on macOS
      } else {
        None
      }
    };
    let private_peak = unsafe {
      let mut info: libc::rusage_info_v4 = std::mem::zeroed();
      let rc = libc::proc_pid_rusage(
        libc::getpid(),
        libc::RUSAGE_INFO_V4,
        (&mut info as *mut libc::rusage_info_v4).cast::<libc::rusage_info_t>(),
      );
      if rc == 0 {
        Some(info.ri_lifetime_max_phys_footprint)
      } else {
        None
      }
    };
    PeakMemory {
      private_peak,
      rss_peak,
      method: if private_peak.is_some() { "exact" } else { "unavailable" },
    }
  }
}

#[cfg(target_os = "linux")]
mod linux {
  use super::{PRIVATE_HWM, PeakMemory};
  use std::sync::atomic::Ordering;

  /// A `/proc/self/status` field in kB, as bytes.
  pub fn status_field(name: &str) -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|l| l.starts_with(name))?;
    let kb: u64 = line[name.len()..].trim().trim_end_matches("kB").trim().parse().ok()?;
    Some(kb * 1024)
  }

  pub fn peak() -> PeakMemory {
    super::sample_private();
    let sampled = PRIVATE_HWM.load(Ordering::Relaxed);
    PeakMemory {
      private_peak: (sampled > 0).then_some(sampled),
      rss_peak: status_field("VmHWM:"),
      method: if sampled > 0 { "sampled" } else { "unavailable" },
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn reports_a_positive_resident_peak_that_grows_when_memory_is_touched() {
    let before = peak_memory();
    #[cfg(unix)]
    assert!(before.rss_peak.is_some_and(|b| b > 0));
    let mut hog: Vec<u8> = vec![0; 64 << 20];
    for (i, byte) in hog.iter_mut().enumerate().step_by(4096) {
      *byte = (i & 0xff) as u8;
    }
    let after = peak_memory();
    #[cfg(unix)]
    assert!(after.rss_peak.unwrap() >= before.rss_peak.unwrap() + (32 << 20), "{before:?} → {after:?}");
    std::hint::black_box(&hog);
    assert_eq!(format_bytes(4_800_000_000), "4.8 GB");
    assert_eq!(format_bytes(612_000_000), "612 MB");
    assert!(describe(&after).starts_with("peak memory: "));
  }
}
