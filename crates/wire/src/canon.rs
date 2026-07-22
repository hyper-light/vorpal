//! Canonical JSON (invariant I3).
//!
//! Any value whose **bytes are hashed or content-addressed** must serialize deterministically. The
//! rule types reused from `vorpal-config` contain `HashMap`s (`labels`, `metadata`, `constraints`,
//! `utils`, `transform`), which serialize in nondeterministic iteration order — so canonicalization
//! happens here, **recursively**, at the wire boundary: every object's keys are emitted in
//! byte-lexicographic order, with compact separators, regardless of how the `serde_json::Value` was
//! produced (and regardless of whether some crate in the build graph enabled serde_json's
//! `preserve_order` feature — this writer never relies on map iteration order being sorted).
//!
//! Number formatting is serde_json's (`itoa`/`ryu` shortest form), which is deterministic for any
//! given value. Strings are escaped by serde_json's writer, also deterministic.

use serde_json::Value;

/// Serialize a JSON value with all object keys sorted recursively (byte-lexicographic), compact.
pub fn to_canonical_json(value: &Value) -> Vec<u8> {
  let mut out = Vec::with_capacity(128);
  write_canonical(value, &mut out);
  out
}

/// Canonical bytes + their blake3 digest, computed **once over exactly these bytes** — callers ship
/// the bytes and the digest together so the receiver verifies what was actually sent, never a
/// re-serialization.
pub fn canonical_json_digest(value: &Value) -> (Vec<u8>, [u8; 32]) {
  let bytes = to_canonical_json(value);
  let digest = crate::hash::digest32(&bytes);
  (bytes, digest)
}

fn write_canonical(value: &Value, out: &mut Vec<u8>) {
  match value {
    Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {
      // Scalars have exactly one serde_json encoding; reuse it.
      serde_json::to_writer(&mut *out, value).expect("scalar JSON write to Vec cannot fail");
    }
    Value::Array(items) => {
      out.push(b'[');
      for (i, item) in items.iter().enumerate() {
        if i > 0 {
          out.push(b',');
        }
        write_canonical(item, out);
      }
      out.push(b']');
    }
    Value::Object(map) => {
      // Never trust the map's own order: collect and sort by key bytes.
      let mut keys: Vec<&String> = map.keys().collect();
      keys.sort_unstable();
      out.push(b'{');
      for (i, key) in keys.iter().enumerate() {
        if i > 0 {
          out.push(b',');
        }
        serde_json::to_writer(&mut *out, &Value::String((*key).clone()))
          .expect("string JSON write to Vec cannot fail");
        out.push(b':');
        write_canonical(&map[key.as_str()], out);
      }
      out.push(b'}');
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;

  #[test]
  fn keys_sort_recursively() {
    let v = json!({
      "zeta": {"b": 1, "a": [ {"y": 0, "x": 0} ]},
      "alpha": true,
    });
    let s = String::from_utf8(to_canonical_json(&v)).unwrap();
    assert_eq!(s, r#"{"alpha":true,"zeta":{"a":[{"x":0,"y":0}],"b":1}}"#);
  }

  #[test]
  fn canonical_bytes_are_input_order_independent() {
    // Two logically equal objects built in different insertion orders must canonicalize equally.
    let mut a = serde_json::Map::new();
    a.insert("k1".into(), json!(1));
    a.insert("k2".into(), json!({"n2": 2, "n1": 1}));
    let mut b = serde_json::Map::new();
    b.insert("k2".into(), json!({"n1": 1, "n2": 2}));
    b.insert("k1".into(), json!(1));
    let (ba, da) = canonical_json_digest(&Value::Object(a));
    let (bb, db) = canonical_json_digest(&Value::Object(b));
    assert_eq!(ba, bb);
    assert_eq!(da, db);
  }

  #[test]
  fn round_trips_through_serde_json() {
    let v = json!({"m": {"b": [1, 2.5, "x"], "a": null}, "s": "é\n\"q\""});
    let bytes = to_canonical_json(&v);
    let back: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(back, v);
  }
}
