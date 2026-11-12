//! Shrink PDF files.
//!
//! Two kinds of work are done:
//!
//! * **Lossless** (always): unreachable objects are dropped, identical streams are
//!   merged, Flate streams are re-deflated at the best level, one-bit images are
//!   stored as CCITT Group 4, and the file is written with object streams and a
//!   cross-reference stream.
//! * **Lossy** (when [`Options::images`] is set): images are downsampled to a
//!   maximum resolution, measured against the size they are actually drawn at, and
//!   grey, RGB and CMYK images are re-encoded as JPEG.
//!
//! ```no_run
//! use pdf_shrink::{compress, compress_to_target, Preset};
//!
//! let input = std::fs::read("big.pdf")?;
//! // One fixed preset:
//! let out = compress(&input, &Preset::Medium.into())?;
//! // Or: the gentlest preset that gets the file under 25 MiB.
//! let out = compress_to_target(&input, 25 << 20, Preset::Low)?;
//! std::fs::write("small.pdf", &out.bytes)?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

mod images;

use std::collections::HashMap;
use std::fmt;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::Read;

use flate2::{Compression, read::ZlibDecoder, write::ZlibEncoder};
use lopdf::{Document, Object, ObjectId, SaveOptions};
use rayon::prelude::*;
use serde::Serialize;

/// Named quality levels, ordered from gentlest to most aggressive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Preset {
    /// Structural optimisation only; every image keeps exactly its pixels.
    Lossless,
    /// 200 dpi, JPEG quality 82. Hard to tell from the original.
    High,
    /// 150 dpi, JPEG quality 72. Fine for reading on screen.
    Medium,
    /// 96 dpi, JPEG quality 55. Smallest; visibly soft when zoomed.
    Low,
}

impl Preset {
    pub const ALL: [Preset; 4] = [Preset::Lossless, Preset::High, Preset::Medium, Preset::Low];

    pub fn name(self) -> &'static str {
        match self {
            Preset::Lossless => "lossless",
            Preset::High => "high",
            Preset::Medium => "medium",
            Preset::Low => "low",
        }
    }

    pub fn images(self) -> Option<ImageOptions> {
        let (max_dpi, quality) = match self {
            Preset::Lossless => return None,
            Preset::High => (200.0, 82),
            Preset::Medium => (150.0, 72),
            Preset::Low => (96.0, 55),
        };
        Some(ImageOptions { max_dpi, quality })
    }
}

impl std::str::FromStr for Preset {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        Preset::ALL
            .into_iter()
            .find(|p| p.name().eq_ignore_ascii_case(s))
            .ok_or_else(|| format!("unknown preset `{s}` (expected lossless, high, medium or low)"))
    }
}

impl fmt::Display for Preset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// How raster images are recompressed.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct ImageOptions {
    /// Images drawn at a higher resolution than this are downsampled to it.
    /// One-bit images (scanned text) are kept at twice this.
    pub max_dpi: f64,
    /// JPEG quality, 1-100.
    pub quality: u8,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Options {
    /// `None` keeps every image untouched (lossless).
    pub images: Option<ImageOptions>,
}

impl From<Preset> for Options {
    fn from(preset: Preset) -> Self {
        Options {
            images: preset.images(),
        }
    }
}

/// What a compression pass did.
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub original_bytes: usize,
    pub compressed_bytes: usize,
    pub pages: usize,
    pub images_total: usize,
    pub images_recompressed: usize,
    pub duplicate_streams_merged: usize,
    /// The preset that produced this result, when one was chosen by
    /// [`compress_to_target`].
    pub preset: Option<Preset>,
}

impl Report {
    /// Compressed size as a fraction of the original (0.25 = a quarter of the size).
    pub fn ratio(&self) -> f64 {
        self.compressed_bytes as f64 / self.original_bytes.max(1) as f64
    }
}

#[derive(Debug, Clone)]
pub struct Output {
    pub bytes: Vec<u8>,
    pub report: Report,
}

#[derive(Debug)]
pub enum Error {
    /// The input could not be parsed as a PDF.
    Parse(lopdf::Error),
    /// Encrypted documents are refused rather than silently stripped of protection.
    Encrypted,
    /// The document could not be serialised.
    Write(lopdf::Error),
    /// The result did not read back as the same document; nothing should be kept.
    Verify(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Parse(e) => write!(f, "not a readable PDF: {e}"),
            Error::Encrypted => f.write_str("the PDF is encrypted"),
            Error::Write(e) => write!(f, "could not write the PDF: {e}"),
            Error::Verify(why) => write!(f, "the compressed PDF failed verification: {why}"),
        }
    }
}

impl std::error::Error for Error {}

/// Compress a PDF held in memory with fixed options.
///
/// The result is always a valid PDF with the same page count, but it is not
/// guaranteed to be smaller than the input: compare `report.compressed_bytes`
/// with `report.original_bytes` before replacing anything.
pub fn compress(input: &[u8], options: &Options) -> Result<Output, Error> {
    let mut doc = Document::load_mem(input).map_err(Error::Parse)?;
    if doc.is_encrypted() || doc.was_encrypted() {
        return Err(Error::Encrypted);
    }
    let pages = doc.get_pages().len();

    let image_stats = images::recompress(&mut doc, options.images.as_ref());
    let duplicate_streams_merged = merge_duplicate_streams(&mut doc);
    doc.prune_objects();
    redeflate(&mut doc);
    doc.compress();

    let mut bytes = Vec::with_capacity(input.len() / 2);
    let save = SaveOptions::builder()
        .use_object_streams(true)
        .use_xref_streams(true)
        .compression_level(9)
        .build();
    doc.save_with_options(&mut bytes, save)
        .map_err(|e| Error::Write(e.into()))?;

    let reread = Document::load_mem(&bytes).map_err(|e| Error::Verify(e.to_string()))?;
    let reread_pages = reread.get_pages().len();
    if reread_pages != pages {
        return Err(Error::Verify(format!(
            "{pages} pages went in, {reread_pages} came out"
        )));
    }

    let report = Report {
        original_bytes: input.len(),
        compressed_bytes: bytes.len(),
        pages,
        images_total: image_stats.total,
        images_recompressed: image_stats.recompressed,
        duplicate_streams_merged,
        preset: None,
    };
    Ok(Output { bytes, report })
}

/// Compress with the gentlest preset whose result is at most `target_bytes`.
///
/// Presets are tried from [`Preset::Lossless`] up to and including `strongest`.
/// If none reaches the target, the smallest result is returned; check
/// `report.compressed_bytes` to see whether the target was met.
pub fn compress_to_target(
    input: &[u8],
    target_bytes: usize,
    strongest: Preset,
) -> Result<Output, Error> {
    let mut best: Option<Output> = None;
    for preset in Preset::ALL.into_iter().filter(|p| *p <= strongest) {
        let mut out = compress(input, &preset.into())?;
        out.report.preset = Some(preset);
        let done = out.bytes.len() <= target_bytes;
        if best
            .as_ref()
            .is_none_or(|b| out.bytes.len() < b.bytes.len())
        {
            best = Some(out);
        }
        // Without images a stronger preset has nothing more to remove.
        if done || best.as_ref().is_some_and(|b| b.report.images_total == 0) {
            break;
        }
    }
    Ok(best.expect("at least the lossless preset runs"))
}

/// Point every reference to a stream that is byte-identical to an earlier one at
/// that earlier stream. The orphans are removed by the following prune.
fn merge_duplicate_streams(doc: &mut Document) -> usize {
    let mut by_hash: HashMap<u64, Vec<ObjectId>> = HashMap::new();
    for (id, object) in &doc.objects {
        if let Object::Stream(stream) = object {
            let mut hasher = DefaultHasher::new();
            stream.content.hash(&mut hasher);
            stream.dict.len().hash(&mut hasher);
            by_hash.entry(hasher.finish()).or_default().push(*id);
        }
    }
    let mut replace: HashMap<ObjectId, ObjectId> = HashMap::new();
    for ids in by_hash.values().filter(|ids| ids.len() > 1) {
        let mut kept: Vec<ObjectId> = Vec::new();
        for id in ids {
            match kept.iter().find(|k| doc.objects[*k] == doc.objects[id]) {
                Some(k) => {
                    replace.insert(*id, *k);
                }
                None => kept.push(*id),
            }
        }
    }
    if !replace.is_empty() {
        doc.traverse_objects(|object| {
            if let Object::Reference(id) = object
                && let Some(kept) = replace.get(id)
            {
                *id = *kept;
            }
        });
    }
    replace.len()
}

/// Re-deflate streams whose only filter is FlateDecode, keeping the result when it
/// is smaller. The inflated bytes are not interpreted, so predictors stay valid.
fn redeflate(doc: &mut Document) {
    const MAX_INFLATED: u64 = 1 << 30;
    let streams: Vec<&mut lopdf::Stream> = doc
        .objects
        .values_mut()
        .filter_map(|object| match object {
            Object::Stream(stream) => Some(stream),
            _ => None,
        })
        .filter(|stream| matches!(stream.filters().as_deref(), Ok([b"FlateDecode"])))
        .collect();
    streams.into_par_iter().for_each(|stream| {
        let mut raw = Vec::new();
        if ZlibDecoder::new(stream.content.as_slice())
            .take(MAX_INFLATED + 1)
            .read_to_end(&mut raw)
            .is_err()
            || raw.len() as u64 > MAX_INFLATED
        {
            return;
        }
        let mut encoder = ZlibEncoder::new(
            Vec::with_capacity(stream.content.len()),
            Compression::best(),
        );
        if std::io::Write::write_all(&mut encoder, &raw).is_err() {
            return;
        }
        if let Ok(packed) = encoder.finish()
            && packed.len() < stream.content.len()
        {
            stream.set_content(packed);
        }
    });
}
