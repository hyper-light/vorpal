use std::fs;

pub fn first(path: &str) -> std::io::Result<u64> {
    Ok(fs::metadata(path)?.len())
}

mod inner {
    pub fn nested(path: &str) -> bool {
        std::fs::metadata(path).is_ok()
    }
}

impl Holder {
    pub fn size(&self) -> u64 {
        fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0)
    }
}

pub struct Holder {
    pub path: String,
}

macro_rules! probe {
    ($p:expr) => {
        fs::metadata($p)
    };
}

pub fn last(path: &str) -> bool {
    probe!(path).is_ok() && fs::metadata(path).is_ok()
}
