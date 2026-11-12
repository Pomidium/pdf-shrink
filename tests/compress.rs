use lopdf::{Document, Object, Stream, dictionary};
use pdf_shrink::{Error, Preset, compress, compress_to_target};

/// A one-page Letter PDF showing a noisy 1200x1200 RGB photo and a 2400x2400
/// one-bit "scan", each drawn four inches wide, plus an unreferenced object.
fn sample() -> Vec<u8> {
    let mut doc = Document::with_version("1.5");
    let mut noise = 0x2545F491u32;
    let photo: Vec<u8> = (0..1200 * 1200 * 3)
        .map(|i| {
            noise = noise.wrapping_mul(1664525).wrapping_add(1013904223);
            ((i / 3 % 1200) / 5) as u8 ^ (noise >> 28) as u8
        })
        .collect();
    let scan: Vec<u8> = (0..300 * 2400)
        .map(|i| if (i / 300 / 40) % 2 == 0 { 0xFF } else { 0x0F })
        .collect();
    let photo = doc.add_object(Stream::new(
        dictionary! { "Type" => "XObject", "Subtype" => "Image", "Width" => 1200, "Height" => 1200,
        "ColorSpace" => "DeviceRGB", "BitsPerComponent" => 8 },
        photo,
    ));
    let scan = doc.add_object(Stream::new(
        dictionary! { "Type" => "XObject", "Subtype" => "Image", "Width" => 2400, "Height" => 2400,
        "ImageMask" => true, "BitsPerComponent" => 1 },
        scan,
    ));
    doc.add_object(Stream::new(dictionary! {}, vec![7; 50_000]));
    let content = doc.add_object(Stream::new(
        dictionary! {},
        b"q 288 0 0 288 36 400 cm /Photo Do Q q 288 0 0 288 36 72 cm /Scan Do Q".to_vec(),
    ));
    let pages = doc.new_object_id();
    let page = doc.add_object(dictionary! {
        "Type" => "Page", "Parent" => pages, "Contents" => content,
        "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        "Resources" => dictionary! { "XObject" => dictionary! { "Photo" => photo, "Scan" => scan } },
    });
    doc.objects.insert(
        pages,
        dictionary! { "Type" => "Pages", "Kids" => vec![page.into()], "Count" => 1 }.into(),
    );
    let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages });
    doc.trailer.set("Root", catalog);
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();
    bytes
}

/// (filter, width) of every image in the document.
fn images(pdf: &[u8]) -> Vec<(String, i64)> {
    let doc = Document::load_mem(pdf).unwrap();
    let mut found: Vec<(String, i64)> = doc
        .objects
        .values()
        .filter_map(|o| o.as_stream().ok())
        .filter(|s| {
            s.dict
                .get(b"Subtype")
                .and_then(Object::as_name)
                .is_ok_and(|n| n == b"Image")
        })
        .map(|s| {
            let filter = String::from_utf8_lossy(s.dict.get(b"Filter").unwrap().as_name().unwrap())
                .into_owned();
            (filter, s.dict.get(b"Width").unwrap().as_i64().unwrap())
        })
        .collect();
    found.sort();
    found
}

#[test]
fn lossless_keeps_every_pixel() {
    let input = sample();
    let out = compress(&input, &Preset::Lossless.into()).unwrap();
    assert!(out.bytes.len() < input.len());
    assert_eq!(out.report.pages, 1);
    // The scan is repacked as Group 4 at full size; the photo is only deflated.
    assert_eq!(
        images(&out.bytes),
        [
            ("CCITTFaxDecode".to_string(), 2400),
            ("FlateDecode".to_string(), 1200)
        ]
    );

    let doc = Document::load_mem(&out.bytes).unwrap();
    let original = Document::load_mem(&input).unwrap();
    let pixels = |d: &Document, width: i64| {
        let stream = d.objects.values().filter_map(|o| o.as_stream().ok());
        let mut stream = stream.filter(|s| {
            s.dict
                .get(b"Width")
                .and_then(Object::as_i64)
                .is_ok_and(|w| w == width)
        });
        stream.next().unwrap().get_plain_content().unwrap()
    };
    assert_eq!(pixels(&doc, 1200), pixels(&original, 1200));
}

#[test]
fn lossy_downsamples_to_the_drawn_size() {
    let input = sample();
    let out = compress(&input, &Preset::Medium.into()).unwrap();
    // Four inches at 150 dpi; the one-bit image keeps twice that.
    assert_eq!(
        images(&out.bytes),
        [
            ("CCITTFaxDecode".to_string(), 1200),
            ("DCTDecode".to_string(), 600)
        ]
    );
    assert_eq!(out.report.images_recompressed, 2);
    assert!(
        out.bytes.len()
            < compress(&input, &Preset::Lossless.into())
                .unwrap()
                .bytes
                .len()
    );
}

#[test]
fn target_picks_the_gentlest_preset_that_fits() {
    let input = sample();
    let lossless = compress(&input, &Preset::Lossless.into())
        .unwrap()
        .bytes
        .len();
    let easy = compress_to_target(&input, lossless, Preset::Low).unwrap();
    assert_eq!(easy.report.preset, Some(Preset::Lossless));

    let hard = compress_to_target(&input, lossless - 1, Preset::Low).unwrap();
    assert_eq!(hard.report.preset, Some(Preset::High));

    // An unreachable target still returns the smallest allowed attempt.
    let capped = compress_to_target(&input, 1, Preset::Medium).unwrap();
    assert_eq!(capped.report.preset, Some(Preset::Medium));
}

#[test]
fn rejects_what_is_not_a_pdf() {
    assert!(matches!(
        compress(b"hello", &Preset::Medium.into()),
        Err(Error::Parse(_))
    ));
}
