//! Tally the images in a PDF by encoding, to see what a file is made of.
//! `cargo run --release --example survey -- file.pdf`
use lopdf::{Document, Object};
use std::collections::BTreeMap;

fn main() {
    let path = std::env::args().nth(1).expect("usage: survey <file.pdf>");
    let doc = Document::load(&path).expect("readable PDF");
    let mut tally: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    let mut other = 0;
    for object in doc.objects.values() {
        let Object::Stream(s) = object else { continue };
        if !s
            .dict
            .get(b"Subtype")
            .and_then(Object::as_name)
            .is_ok_and(|n| n == b"Image")
        {
            other += s.content.len();
            continue;
        }
        let show = |key: &[u8]| match s
            .dict
            .get(key)
            .map(|o| doc.dereference(o).map(|(_, o)| o).unwrap_or(o))
        {
            Ok(Object::Array(a)) => format!("{:?}", a.iter().take(2).collect::<Vec<_>>()),
            Ok(o) => format!("{o:?}"),
            Err(_) => "-".into(),
        };
        let flags: String = ["ImageMask", "Decode", "SMask", "Mask"]
            .iter()
            .filter(|k| s.dict.has(k.as_bytes()))
            .map(|k| format!(" +{k}"))
            .collect();
        let adobe = adobe_transform(&s.content)
            .map(|t| format!(" adobe={t}"))
            .unwrap_or_default();
        let key = format!(
            "{} cs={} bpc={}{}{}",
            show(b"Filter"),
            show(b"ColorSpace"),
            show(b"BitsPerComponent"),
            flags,
            adobe
        );
        let entry = tally.entry(key).or_default();
        entry.0 += 1;
        entry.1 += s.content.len();
    }
    for (key, (count, bytes)) in tally {
        println!(
            "{count:6} images {:9.2} MiB  {key}",
            bytes as f64 / 1048576.0
        );
    }
    println!(
        "{:23.2} MiB  non-image streams; {} pages",
        other as f64 / 1048576.0,
        doc.get_pages().len()
    );
}

/// The colour transform declared by a JPEG's Adobe APP14 segment, if it has one.
fn adobe_transform(jpeg: &[u8]) -> Option<u8> {
    let mut at = 2;
    while jpeg.len() >= at + 4 && jpeg[at] == 0xFF && jpeg[at + 1] != 0xDA {
        let length = u16::from_be_bytes([jpeg[at + 2], jpeg[at + 3]]) as usize;
        let body = jpeg.get(at + 4..at + 2 + length)?;
        if jpeg[at + 1] == 0xEE && body.starts_with(b"Adobe") {
            return body.get(11).copied();
        }
        at += 2 + length;
    }
    None
}
