//! Lossy image recompression: downsample to the drawn resolution, re-encode as JPEG.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use std::io::Read;

use flate2::read::ZlibDecoder;
use lopdf::content::{Content, Operation};
use lopdf::{Dictionary, Document, Object, ObjectId, Stream};
use rayon::prelude::*;

use crate::ImageOptions;

pub(crate) struct Stats {
    pub total: usize,
    pub recompressed: usize,
}

/// Width and height, in points, of the largest place something is drawn.
type Extent = (f64, f64);

/// `[a b c d e f]` as in the PDF `cm` operator.
type Matrix = [f64; 6];
const IDENTITY: Matrix = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];

/// `first` applied before `second`.
fn concat(first: &Matrix, second: &Matrix) -> Matrix {
    let ([a1, b1, c1, d1, e1, f1], [a2, b2, c2, d2, e2, f2]) = (*first, *second);
    [
        a1 * a2 + b1 * c2,
        a1 * b2 + b1 * d2,
        c1 * a2 + d1 * c2,
        c1 * b2 + d1 * d2,
        e1 * a2 + f1 * c2 + e2,
        e1 * b2 + f1 * d2 + f2,
    ]
}

fn resolve<'a>(doc: &'a Document, object: &'a Object) -> &'a Object {
    doc.dereference(object).map(|(_, o)| o).unwrap_or(object)
}

fn dict_entry<'a>(doc: &'a Document, dict: &'a Dictionary, key: &[u8]) -> Option<&'a Dictionary> {
    resolve(doc, dict.get(key).ok()?).as_dict().ok()
}

fn is_image(stream: &Stream) -> bool {
    stream
        .dict
        .get(b"Subtype")
        .and_then(Object::as_name)
        .is_ok_and(|n| n == b"Image")
}

fn image_ids(doc: &Document) -> Vec<ObjectId> {
    doc.objects
        .iter()
        .filter(|(_, o)| matches!(o, Object::Stream(s) if is_image(s)))
        .map(|(id, _)| *id)
        .collect()
}

pub(crate) fn recompress(doc: &mut Document, options: Option<&ImageOptions>) -> Stats {
    let ids = image_ids(doc);
    let (drawn, largest_page) = drawn_extents(doc);

    // Masks carry transparency, where resampling and JPEG ringing show up as halos:
    // they are only ever repacked losslessly.
    let masks: HashSet<ObjectId> = ids
        .iter()
        .filter_map(|id| doc.get_object(*id).ok()?.as_stream().ok())
        .flat_map(|s| {
            [b"SMask".as_slice(), b"Mask"]
                .map(|k| s.dict.get(k).and_then(Object::as_reference).ok())
        })
        .flatten()
        .collect();

    let shared: &Document = doc;
    let replacements: Vec<(ObjectId, Stream)> = ids
        .par_iter()
        .filter_map(|id| {
            let stream = shared.get_object(*id).ok()?.as_stream().ok()?;
            let image = Image::read(shared, stream)?;
            // An image we never saw drawn (a pattern, an annotation) may fill a page.
            let extent = drawn.get(id).copied().unwrap_or(largest_page);
            let lossy = options.filter(|_| !masks.contains(id));
            let new = if image.bits == 1 {
                image.repack_bilevel(extent, lossy)
            } else {
                image.to_jpeg(extent, lossy?)
            };
            Some((*id, new?))
        })
        .collect();

    let recompressed = replacements.len();
    for (id, stream) in replacements {
        doc.objects.insert(id, Object::Stream(stream));
    }
    Stats {
        total: ids.len(),
        recompressed,
    }
}

/// Number of colour components, for colour spaces whose samples are plain
/// intensities. Indexed is left out: its samples are palette indices.
fn components(doc: &Document, color_space: &Object) -> Option<usize> {
    match resolve(doc, color_space) {
        Object::Name(name) => match name.as_slice() {
            b"DeviceGray" | b"G" => Some(1),
            b"DeviceRGB" | b"RGB" => Some(3),
            b"DeviceCMYK" | b"CMYK" => Some(4),
            _ => None,
        },
        Object::Array(parts) => match resolve(doc, parts.first()?).as_name().ok()? {
            b"CalGray" | b"Separation" => Some(1),
            b"CalRGB" => Some(3),
            b"ICCBased" => {
                let profile = resolve(doc, parts.get(1)?).as_stream().ok()?;
                usize::try_from(profile.dict.get(b"N").and_then(Object::as_i64).ok()?).ok()
            }
            _ => None,
        },
        _ => None,
    }
    .filter(|n| matches!(n, 1 | 3 | 4))
}

struct Image<'a> {
    stream: &'a Stream,
    width: u32,
    height: u32,
    /// Bits per component: 1 or 8.
    bits: u8,
    channels: usize,
}

/// Filters that only pack bytes; lopdf undoes them, leaving raw samples.
const BYTE_FILTERS: [&[u8]; 5] = [
    b"FlateDecode",
    b"LZWDecode",
    b"RunLengthDecode",
    b"ASCII85Decode",
    b"ASCIIHexDecode",
];

impl<'a> Image<'a> {
    fn read(doc: &Document, stream: &'a Stream) -> Option<Self> {
        let dict = &stream.dict;
        let int = |key: &[u8]| {
            dict.get(key)
                .ok()
                .and_then(|o| resolve(doc, o).as_i64().ok())
        };
        let width = u32::try_from(int(b"Width")?).ok().filter(|w| *w > 0)?;
        let height = u32::try_from(int(b"Height")?).ok().filter(|h| *h > 0)?;
        let stencil = dict
            .get(b"ImageMask")
            .and_then(Object::as_bool)
            .unwrap_or(false);
        let bits = if stencil {
            1
        } else {
            u8::try_from(int(b"BitsPerComponent")?).ok()?
        };
        let channels = match (stencil, dict.get(b"ColorSpace")) {
            (true, _) => 1,
            // Two palette entries survive anything done to a one-bit image.
            (false, Ok(cs)) if bits == 1 => components(doc, cs).or_else(|| {
                let parts = resolve(doc, cs).as_array().ok()?;
                (resolve(doc, parts.first()?).as_name().ok()? == b"Indexed").then_some(1)
            })?,
            (false, Ok(cs)) => components(doc, cs)?,
            (false, Err(_)) => return None,
        };
        // Colour-key masking compares exact sample values, which lossy coding changes.
        let color_keyed = dict
            .get(b"Mask")
            .is_ok_and(|m| resolve(doc, m).as_array().is_ok());
        if !matches!(bits, 1 | 8)
            || (bits == 1 && channels != 1)
            || color_keyed
            || int(b"SMaskInData").unwrap_or(0) != 0
        {
            return None;
        }
        Some(Image {
            stream,
            width,
            height,
            bits,
            channels,
        })
    }

    /// Raw samples, when the stream holds them under byte-packing filters only.
    fn samples(&self) -> Option<Vec<u8>> {
        let filters = self.stream.filters().unwrap_or_default();
        if !filters.iter().all(|f| BYTE_FILTERS.contains(f)) {
            return None;
        }
        let samples = self.stream.get_plain_content().ok()?;
        let row = (self.width as usize * self.channels * self.bits as usize).div_ceil(8);
        (samples.len() >= row * self.height as usize).then_some(samples)
    }

    /// The factor to resize by so the image has `dpi` where it is drawn largest,
    /// or `None` when it is not worth resampling.
    fn downscale(&self, drawn: Extent, dpi: f64) -> Option<(u32, u32)> {
        let needed = |points: f64, have: u32| (points / 72.0 * dpi).ceil().max(16.0) / have as f64;
        let scale = needed(drawn.0, self.width).max(needed(drawn.1, self.height));
        let size = |have: u32| ((have as f64 * scale).round() as u32).max(1);
        (scale < 0.8).then(|| (size(self.width), size(self.height)))
    }

    fn replaced(
        &self,
        width: u32,
        height: u32,
        filter: &[u8],
        parms: Option<Dictionary>,
        data: Vec<u8>,
    ) -> Stream {
        let mut dict = self.stream.dict.clone();
        dict.set("Filter", Object::Name(filter.to_vec()));
        match parms {
            Some(parms) => dict.set("DecodeParms", parms),
            None => drop(dict.remove(b"DecodeParms")),
        }
        dict.set("Width", width as i64);
        dict.set("Height", height as i64);
        Stream::new(dict, data).with_compression(false)
    }

    /// Continuous-tone image: downsample and encode as JPEG.
    fn to_jpeg(&self, drawn: Extent, options: &ImageOptions) -> Option<Stream> {
        use jpeg_encoder::ColorType;
        let filters = self.stream.filters().unwrap_or_default();
        let (pixels, color) = if filters.last().is_some_and(|f| *f == b"DCTDecode") {
            let jpeg = match filters.as_slice() {
                [_] => std::borrow::Cow::Borrowed(&self.stream.content),
                [b"FlateDecode", _] => {
                    let mut inflated = Vec::new();
                    ZlibDecoder::new(self.stream.content.as_slice())
                        .read_to_end(&mut inflated)
                        .ok()?;
                    std::borrow::Cow::Owned(inflated)
                }
                _ => return None,
            };
            decode_jpeg(&jpeg, self.width, self.height, self.channels)?
        } else {
            let color = match self.channels {
                1 => ColorType::Luma,
                3 => ColorType::Rgb,
                _ => ColorType::Cmyk,
            };
            let mut samples = self.samples()?;
            samples.truncate(self.width as usize * self.height as usize * self.channels);
            (samples, color)
        };

        let (width, height, pixels) = match self.downscale(drawn, options.max_dpi) {
            Some((w, h)) => (
                w,
                h,
                resize(pixels, self.channels, self.width, self.height, w, h)?,
            ),
            None => (self.width, self.height, pixels),
        };

        let mut jpeg = Vec::new();
        let mut encoder = jpeg_encoder::Encoder::new(&mut jpeg, options.quality.clamp(1, 100));
        encoder.set_progressive(true);
        encoder.set_optimized_huffman_tables(true);
        encoder.set_sampling_factor(if options.quality >= 80 {
            jpeg_encoder::SamplingFactor::R_4_4_4
        } else {
            jpeg_encoder::SamplingFactor::R_4_2_0
        });
        encoder
            .encode(
                &pixels,
                u16::try_from(width).ok()?,
                u16::try_from(height).ok()?,
                color,
            )
            .ok()?;

        // Re-encoding costs a generation of quality; only pay it for a real saving.
        if jpeg.len() * 10 >= self.stream.content.len() * 9 {
            return None;
        }
        let mut new = self.replaced(width, height, b"DCTDecode", None, jpeg);
        new.dict.set("BitsPerComponent", 8);
        Some(new)
    }

    /// One-bit image (scanned text, stencil): optionally downsample, then store as
    /// CCITT Group 4, which is lossless and beats Flate on such images.
    fn repack_bilevel(&self, drawn: Extent, lossy: Option<&ImageOptions>) -> Option<Stream> {
        let samples = self.samples()?;
        // One-bit pixels have no shades to hide coarseness, so keep twice the resolution.
        let resized = lossy
            .and_then(|options| self.downscale(drawn, options.max_dpi * 2.0))
            .and_then(|(w, h)| {
                let row = (self.width as usize).div_ceil(8);
                let grey: Vec<u8> = samples
                    .chunks_exact(row)
                    .take(self.height as usize)
                    .flat_map(|line| {
                        (0..self.width as usize).map(|x| {
                            if line[x / 8] & (0x80 >> (x % 8)) != 0 {
                                255
                            } else {
                                0
                            }
                        })
                    })
                    .collect();
                let grey = resize(grey, 1, self.width, self.height, w, h)?;
                let mut packed = vec![0u8; (w as usize).div_ceil(8) * h as usize];
                for (line, pixels) in packed
                    .chunks_exact_mut((w as usize).div_ceil(8))
                    .zip(grey.chunks_exact(w as usize))
                {
                    for (x, _) in pixels.iter().enumerate().filter(|(_, p)| **p >= 128) {
                        line[x / 8] |= 0x80 >> (x % 8);
                    }
                }
                Some((w, h, packed))
            });
        let (width, height, samples) = resized.unwrap_or((self.width, self.height, samples));

        let fax = encode_g4(&samples, width, height)?;
        if fax.len() * 10 >= self.stream.content.len() * 9 {
            return None;
        }
        let mut parms = Dictionary::new();
        parms.set("K", -1);
        parms.set("Columns", width as i64);
        parms.set("Rows", height as i64);
        Some(self.replaced(width, height, b"CCITTFaxDecode", Some(parms), fax))
    }
}

/// Decode a JPEG to the samples a PDF reader would see, along with how to encode
/// them again without changing what they mean.
///
/// Four-component JPEGs are only taken when an Adobe marker says how they are
/// stored, and their components are passed through untouched: readers disagree
/// about CMYK polarity, so the samples must not be reinterpreted here.
fn decode_jpeg(
    jpeg: &[u8],
    width: u32,
    height: u32,
    channels: usize,
) -> Option<(Vec<u8>, jpeg_encoder::ColorType)> {
    use jpeg_encoder::ColorType;
    use zune_jpeg::zune_core::{
        bytestream::ZCursor, colorspace::ColorSpace, options::DecoderOptions,
    };

    let mut probe = zune_jpeg::JpegDecoder::new(ZCursor::new(jpeg));
    probe.decode_headers().ok()?;
    let (output, color) = match (channels, probe.input_colorspace()?, adobe_transform(jpeg)) {
        (1, ColorSpace::Luma, _) => (ColorSpace::Luma, ColorType::Luma),
        (3, ColorSpace::YCbCr | ColorSpace::RGB, _) => (ColorSpace::RGB, ColorType::Rgb),
        (4, ColorSpace::CMYK, Some(0)) => (ColorSpace::CMYK, ColorType::Cmyk),
        (4, ColorSpace::YCCK, Some(2)) => (ColorSpace::YCCK, ColorType::Ycck),
        _ => return None,
    };
    let options = DecoderOptions::default().jpeg_set_out_colorspace(output);
    let mut decoder = zune_jpeg::JpegDecoder::new_with_options(ZCursor::new(jpeg), options);
    let pixels = decoder.decode().ok()?;
    let info = decoder.info()?;
    let matches = info.width as u32 == width
        && info.height as u32 == height
        && pixels.len() == width as usize * height as usize * channels;
    matches.then_some((pixels, color))
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

fn resize(
    pixels: Vec<u8>,
    channels: usize,
    width: u32,
    height: u32,
    w: u32,
    h: u32,
) -> Option<Vec<u8>> {
    use image::{
        ImageBuffer, Luma, Rgb, Rgba,
        imageops::{FilterType, resize},
    };
    let filter = FilterType::CatmullRom;
    Some(match channels {
        1 => resize(
            &ImageBuffer::<Luma<u8>, _>::from_raw(width, height, pixels)?,
            w,
            h,
            filter,
        )
        .into_raw(),
        3 => resize(
            &ImageBuffer::<Rgb<u8>, _>::from_raw(width, height, pixels)?,
            w,
            h,
            filter,
        )
        .into_raw(),
        // Four independent channels; nothing here treats the fourth as alpha.
        _ => resize(
            &ImageBuffer::<Rgba<u8>, _>::from_raw(width, height, pixels)?,
            w,
            h,
            filter,
        )
        .into_raw(),
    })
}

/// Encode packed one-bit rows as CCITT Group 4 with `/BlackIs1 false` (a 0 bit is
/// black), then decode the result and insist it matches before trusting it.
fn encode_g4(samples: &[u8], width: u32, height: u32) -> Option<Vec<u8>> {
    use fax::{Color, VecWriter, decoder, encoder::Encoder};
    let row = (width as usize).div_ceil(8);
    let rows = || samples.chunks_exact(row).take(height as usize);
    fn pels(line: &[u8], width: u32) -> impl Iterator<Item = Color> + '_ {
        (0..width as usize).map(move |x| {
            if line[x / 8] & (0x80 >> (x % 8)) != 0 {
                Color::White
            } else {
                Color::Black
            }
        })
    }

    let mut encoder = Encoder::new(VecWriter::with_capacity(samples.len()));
    for line in rows() {
        encoder.encode_line(pels(line, width), width).ok()?;
    }
    let fax = encoder.finish().ok()?.finish();

    let mut source = rows();
    let mut lines = 0;
    let mut same = true;
    decoder::decode_g4(fax.iter().copied(), width, Some(height), |transitions| {
        lines += 1;
        same &= source
            .next()
            .is_some_and(|line| decoder::pels(transitions, width).eq(pels(line, width)));
    })?;
    (same && lines == height).then_some(fax)
}

/// For every image drawn from page content (directly or through form XObjects),
/// the largest size it is drawn at; plus the largest page, as the bound for
/// images reached some other way.
fn drawn_extents(doc: &Document) -> (HashMap<ObjectId, Extent>, Extent) {
    let pages: Vec<ObjectId> = doc.page_iter().collect();
    let per_page: Vec<(HashMap<ObjectId, Extent>, Extent)> = pages
        .par_iter()
        .map(|page_id| {
            let mut walker = Walker {
                doc,
                forms: HashMap::new(),
                drawn: HashMap::new(),
                budget: 100_000,
            };
            if let Some(resources) =
                inherited(doc, *page_id, b"Resources").and_then(|o| o.as_dict().ok())
                && let Ok(content) = Content::decode(&doc.get_page_content(*page_id))
            {
                walker.walk(&content.operations, resources, IDENTITY, 0);
            }
            let page_size = inherited(doc, *page_id, b"MediaBox")
                .and_then(|o| o.as_array().ok())
                .and_then(|b| {
                    let n: Vec<f64> = b
                        .iter()
                        .filter_map(|v| resolve(doc, v).as_float().ok())
                        .map(f64::from)
                        .collect();
                    (n.len() == 4).then(|| ((n[2] - n[0]).abs(), (n[3] - n[1]).abs()))
                })
                .unwrap_or((612.0, 792.0));
            (walker.drawn, page_size)
        })
        .collect();

    let mut drawn: HashMap<ObjectId, Extent> = HashMap::new();
    let mut largest_page: Extent = (0.0, 0.0);
    for (page_drawn, page_size) in per_page {
        for (id, extent) in page_drawn {
            grow(drawn.entry(id).or_default(), extent);
        }
        grow(&mut largest_page, page_size);
    }
    if largest_page.0 <= 0.0 || largest_page.1 <= 0.0 {
        largest_page = (612.0, 792.0);
    }
    (drawn, largest_page)
}

fn grow(extent: &mut Extent, other: Extent) {
    extent.0 = extent.0.max(other.0);
    extent.1 = extent.1.max(other.1);
}

/// A page attribute, looked up through the page tree as inheritable attributes are.
fn inherited<'a>(doc: &'a Document, page_id: ObjectId, key: &[u8]) -> Option<&'a Object> {
    let mut node = doc.get_dictionary(page_id).ok()?;
    for _ in 0..64 {
        if let Ok(value) = node.get(key) {
            return Some(resolve(doc, value));
        }
        node = doc
            .get_dictionary(node.get(b"Parent").ok()?.as_reference().ok()?)
            .ok()?;
    }
    None
}

struct Walker<'a> {
    doc: &'a Document,
    forms: HashMap<ObjectId, Option<Rc<Vec<Operation>>>>,
    drawn: HashMap<ObjectId, Extent>,
    /// Bounds the work a maliciously self-nesting document can cause.
    budget: u32,
}

impl<'a> Walker<'a> {
    fn walk(
        &mut self,
        operations: &[Operation],
        resources: &'a Dictionary,
        mut ctm: Matrix,
        depth: u8,
    ) {
        let doc = self.doc;
        let xobjects = dict_entry(doc, resources, b"XObject");
        let mut stack: Vec<Matrix> = Vec::new();
        for op in operations {
            match op.operator.as_str() {
                "q" => stack.push(ctm),
                "Q" => ctm = stack.pop().unwrap_or(ctm),
                "cm" => {
                    let n: Vec<f64> = op
                        .operands
                        .iter()
                        .filter_map(|o| o.as_float().ok())
                        .map(f64::from)
                        .collect();
                    if let Ok(matrix) = <Matrix>::try_from(n) {
                        ctm = concat(&matrix, &ctm);
                    }
                }
                "Do" => {
                    let target = op.operands.first().and_then(|o| o.as_name().ok());
                    let Some(id) =
                        target.and_then(|name| xobjects?.get(name).ok()?.as_reference().ok())
                    else {
                        continue;
                    };
                    let Some(stream) = doc.get_object(id).ok().and_then(|o| o.as_stream().ok())
                    else {
                        continue;
                    };
                    if is_image(stream) {
                        // An image fills the unit square, which the CTM maps onto the page.
                        let extent = (ctm[0].hypot(ctm[1]), ctm[2].hypot(ctm[3]));
                        grow(self.drawn.entry(id).or_default(), extent);
                    } else if depth < 8 && self.budget > 0 {
                        self.budget -= 1;
                        let Some(operations) = self
                            .forms
                            .entry(id)
                            .or_insert_with(|| {
                                let data = stream.get_plain_content().ok()?;
                                Some(Rc::new(Content::decode(&data).ok()?.operations))
                            })
                            .clone()
                        else {
                            continue;
                        };
                        let matrix = stream
                            .dict
                            .get(b"Matrix")
                            .ok()
                            .and_then(|m| resolve(doc, m).as_array().ok())
                            .map(|m| {
                                m.iter()
                                    .filter_map(|v| v.as_float().ok())
                                    .map(f64::from)
                                    .collect::<Vec<_>>()
                            })
                            .and_then(|m| <Matrix>::try_from(m).ok())
                            .unwrap_or(IDENTITY);
                        let form_resources =
                            dict_entry(doc, &stream.dict, b"Resources").unwrap_or(resources);
                        self.walk(
                            &operations,
                            form_resources,
                            concat(&matrix, &ctm),
                            depth + 1,
                        );
                    }
                }
                _ => {}
            }
        }
    }
}
