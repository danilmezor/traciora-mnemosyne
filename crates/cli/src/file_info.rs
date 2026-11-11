//! The file-info TSV contract. Filesystem samples are current snapshots;
//! retained adapter-stat lifecycle and watcher timing remain separate work.
use crate::search::Params;
use mnemosyne_core::{basename, extension, strip_extension, vault_search::ecmascript_number};
use mnemosyne_engine::{Engine, FileTarget};
use std::{
    fs::Metadata,
    time::{SystemTime, UNIX_EPOCH},
};

/// Rounded desktop file-stat fields, independently usable by a retained cache.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FileStat {
    pub size: u64,
    pub created_ms: f64,
    pub modified_ms: f64,
}

// Math.round chooses the integer toward positive infinity at exact halves.
// Separating the fractional part also avoids rounding x + 0.5 prematurely.
fn rounded_milliseconds(value: f64) -> f64 {
    if value == 0.0 || !value.is_finite() {
        return value;
    }
    let floor = value.floor();
    let rounded = if value - floor < 0.5 {
        floor
    } else {
        floor + 1.0
    };
    if rounded == 0.0 && value < 0.0 {
        -0.0
    } else {
        rounded
    }
}

fn milliseconds(time: SystemTime) -> f64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => {
            duration.as_secs() as f64 * 1000.0 + duration.subsec_nanos() as f64 / 1_000_000.0
        }
        Err(error) => {
            let duration = error.duration();
            -(duration.as_secs() as f64 * 1000.0 + duration.subsec_nanos() as f64 / 1_000_000.0)
        }
    }
}

impl FileStat {
    /// Convert the raw millisecond fields supplied by the desktop fs adapter.
    pub fn from_raw_milliseconds(size: u64, birthtime_ms: f64, mtime_ms: f64) -> Self {
        Self {
            size,
            created_ms: rounded_milliseconds(birthtime_ms),
            modified_ms: rounded_milliseconds(mtime_ms),
        }
    }
    pub fn from_metadata(metadata: &Metadata) -> std::io::Result<Self> {
        Ok(Self::from_raw_milliseconds(
            metadata.len(),
            milliseconds(metadata.created()?),
            milliseconds(metadata.modified()?),
        ))
    }
}

/// Native handler body; generic command dispatch owns the final pipe newline.
pub fn format(path: &str, stat: FileStat) -> String {
    format!(
        "path\t{path}\nname\t{}\nextension\t{}\nsize\t{}\ncreated\t{}\nmodified\t{}",
        strip_extension(basename(path)),
        extension(basename(path)),
        ecmascript_number(stat.size as f64),
        ecmascript_number(stat.created_ms),
        ecmascript_number(stat.modified_ms)
    )
}

pub fn execute(engine: &Engine, params: &Params) -> anyhow::Result<String> {
    let value = |key| {
        params
            .get(key)
            .map(|value| value.as_deref().unwrap_or("true"))
    };
    let path = engine.resolve_target(
        FileTarget {
            file: value("file"),
            path: value("path"),
        },
        true,
    )?;
    let stat = FileStat::from_metadata(&std::fs::metadata(engine.vault.root.join(&path))?)?;
    Ok(format(&path, stat))
}
