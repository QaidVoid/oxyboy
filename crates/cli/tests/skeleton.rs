//! The walking-skeleton gate: boot the script-built mode-3 test ROM
//! through the CLI to a dumped frame and match it against a golden PNG.
//!
//! This is the project's definition of "boot a trivial homebrew to a
//! visible frame": one CLI invocation exercises the loader (image parse),
//! the decoder and interpreter (every fetched instruction), the bus
//! (DISPCNT and VRAM writes), and the mode-3 PPU (rendered pixels).
//!
//! Skips loudly when the ROM is not built; see scripts/build-test-roms.sh.

use std::fs;
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::process::Command;

use testutil::testrom;

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("oxyboy-skeleton-{}-{name}", std::process::id()));
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn oxyboy() -> Command {
    Command::new(env!("CARGO_BIN_EXE_oxyboy"))
}

/// Decodes a PNG into `(width, height, RGBA bytes)`.
fn decode(path: &Path) -> (u32, u32, Vec<u8>) {
    let file = File::open(path).unwrap_or_else(|e| panic!("open {}: {e}", path.display()));
    let mut reader = png::Decoder::new(BufReader::new(file))
        .read_info()
        .expect("PNG header");
    let mut buffer = vec![0; reader.output_buffer_size().expect("output size")];
    let info = reader.next_frame(&mut buffer).expect("PNG frame");
    buffer.truncate(info.buffer_size());
    (info.width, info.height, buffer)
}

/// Asserts `actual` matches `golden`, naming the mismatching pixel region.
fn assert_same_pixels(golden_path: &Path, golden: (u32, u32, &[u8]), actual: (u32, u32, &[u8])) {
    let (gw, gh, gold) = golden;
    let (aw, ah, act) = actual;
    assert_eq!(
        (aw, ah),
        (gw, gh),
        "frame is {aw}x{ah}, golden is {gw}x{gh}"
    );
    if act == gold {
        return;
    }
    let stride = aw as usize * 4;
    let diff = |x: usize, y: usize| {
        let at = y * stride + x * 4;
        act[at..at + 4] != gold[at..at + 4]
    };
    let first = (0..ah as usize)
        .flat_map(|y| (0..aw as usize).map(move |x| (x, y)))
        .find(|&(x, y)| diff(x, y))
        .expect("pixels differ but no differing pixel found");
    let region = (
        (0..aw as usize)
            .filter(|&x| (0..ah as usize).any(|y| diff(x, y)))
            .min(),
        (0..aw as usize)
            .filter(|&x| (0..ah as usize).any(|y| diff(x, y)))
            .max(),
        (0..ah as usize)
            .filter(|&y| (0..aw as usize).any(|x| diff(x, y)))
            .min(),
        (0..ah as usize)
            .filter(|&y| (0..aw as usize).any(|x| diff(x, y)))
            .max(),
    );
    let at = first.1 * stride + first.0 * 4;
    panic!(
        "frame differs from golden {} \
         (first differing pixel ({}, {}): actual {:?}, golden {:?}; \
         differences span columns {:?}..{:?}, rows {:?}..{:?})",
        golden_path.display(),
        first.0,
        first.1,
        &act[at..at + 4],
        &gold[at..at + 4],
        region.0,
        region.1,
        region.2,
        region.3,
    );
}

#[test]
fn walking_skeleton_boots_to_golden_frame() {
    let Some(rom) = testrom::require("mode3") else {
        return;
    };
    let dir = temp_dir("gate");
    let dump = dir.join("frame.png");

    let status = oxyboy()
        .args([
            "run",
            "--rom",
            rom.to_str().expect("utf-8 path"),
            "--headless",
            "--frames",
            "60",
            "--dump-frame",
            dump.to_str().expect("utf-8 path"),
        ])
        .status()
        .expect("spawn oxyboy");
    assert!(status.success(), "run failed: {status}");

    let golden_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/skeleton-frame.png");
    let (gw, gh, gold) = decode(&golden_path);
    let (aw, ah, act) = decode(&dump);
    assert_same_pixels(&golden_path, (gw, gh, &gold), (aw, ah, &act));

    // Explicit call points: the ROM paints the first six mode-3 pixels and
    // idles, so these values pin the whole pipeline (DISPCNT store, VRAM
    // stores, render) to visible output.
    let pixel = |x: u32, y: u32| -> [u8; 3] {
        let stride = aw as usize * 4;
        let at = y as usize * stride + x as usize * 4;
        [act[at], act[at + 1], act[at + 2]]
    };
    assert_eq!(pixel(0, 0), [255, 255, 255], "pixel 0 must be white");
    assert_eq!(pixel(1, 0), [255, 255, 255], "pixel 1 must be white");
    assert_eq!(pixel(2, 0), [255, 0, 0], "pixel 2 must be red");
    assert_eq!(pixel(3, 0), [255, 0, 0], "pixel 3 must be red");
    assert_eq!(pixel(4, 0), [0, 0, 255], "pixel 4 must be blue");
    assert_eq!(pixel(5, 0), [0, 0, 255], "pixel 5 must be blue");
    assert_eq!(pixel(6, 0), [0, 0, 0], "backdrop must stay black");
}
