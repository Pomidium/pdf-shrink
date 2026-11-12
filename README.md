# pdf-shrink

Shrink PDF files. A Rust library and a command-line tool; no external programs
(no Ghostscript, no qpdf).

```sh
cargo install --path .

pdf-shrink book.pdf                      # writes book.compressed.pdf (medium preset)
pdf-shrink book.pdf -p low -o small.pdf
pdf-shrink book.pdf --target-size 25M    # gentlest preset that gets under 25 MiB
pdf-shrink book.pdf --in-place --json    # replace the file, machine-readable report
```

Nothing is written unless the result is smaller than the input.

## What it does

Always, losslessly:

- drops objects nothing refers to and merges byte-identical streams;
- re-deflates Flate streams at the best level;
- stores one-bit images (scanned text) as CCITT Group 4, verified by decoding the
  result and comparing every pixel;
- writes object streams and a cross-reference stream.

With a lossy preset, images are also downsampled to a maximum resolution and
re-encoded as JPEG. Resolution is measured against the size each image is
actually drawn at on the page, so a large photo shown as a thumbnail shrinks a
lot and a full-page figure stays sharp.

| Preset | Images | One-bit images |
| --- | --- | --- |
| `lossless` | untouched | repacked only |
| `high` | 200 dpi, JPEG quality 82 | 400 dpi |
| `medium` | 150 dpi, JPEG quality 72 | 300 dpi |
| `low` | 96 dpi, JPEG quality 55 | 192 dpi |

Grey, RGB and CMYK images are handled (CMYK JPEGs are re-encoded component for
component, so their colours are not reinterpreted). Left as they are: JPEG 2000,
JBIG2, indexed-colour and 16-bit images, soft masks, and anything whose exact
sample values matter. Encrypted PDFs are refused.

## Library

```rust
use pdf_shrink::{compress, compress_to_target, Preset};

let input = std::fs::read("big.pdf")?;
let out = compress(&input, &Preset::Medium.into())?;          // fixed preset
let out = compress_to_target(&input, 25 << 20, Preset::Low)?; // or aim for a size
println!("{} -> {} bytes ({:?})", out.report.original_bytes, out.report.compressed_bytes, out.report.preset);
std::fs::write("small.pdf", &out.bytes)?;
```

`compress` works on bytes in memory and returns the new file plus a `Report`. The
result always has the same page count (it is re-read to check) but is not
guaranteed to be smaller; compare the sizes before replacing anything.

`cargo run --release --example survey -- file.pdf` tallies a PDF's images by
encoding, which shows where its bytes are.

## JSON report

```json
{"input":"book.pdf","output":"book.compressed.pdf","written":true,
 "original_bytes":35729518,"compressed_bytes":19700402,"preset":"medium",
 "pages":625,"images_total":118,"images_recompressed":116,"duplicate_streams_merged":0,
 "target_bytes":26214400,"target_met":true}
```

On failure the exit status is 1 and, with `--json`, stdout is `{"error":"..."}`.
