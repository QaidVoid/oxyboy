//! Flag-map tests over a synthetic ROM image.

use analysis::{FlagMap, Flags, Seed, State};
use rom::Image;
use testutil::synthetic::RomBuilder;

#[test]
fn seeds_and_round_trips_a_synthetic_rom() {
    let image = Image::from_bytes(RomBuilder::new().title("FLAGS").build()).expect("parse");
    let mut map = FlagMap::from_image(&image);
    assert_eq!(map.len(), image.bytes().len());
    assert_eq!(map.canonical().base(), 0x0800_0000);

    let seeds = map.seed(&[]);
    assert_eq!(
        seeds,
        vec![Seed {
            addr: 0x0800_0000,
            state: State::Arm,
        }]
    );
    assert!(map.get(0x0800_0000).contains(Flags::FUNC_START));
    assert!(!map.get(0x0800_0004).contains(Flags::FUNC_START));

    // Simulate a discovered instruction word and literal pool.
    map.mark_range(0x0800_0000, 0x0800_0008, Flags::EXEC | Flags::WORD);
    map.mark_range(0x0800_0020, 0x0800_0024, Flags::LITERAL);

    let json = map.to_json().expect("serialize");
    let restored = FlagMap::from_json(&json).expect("restore");
    assert_eq!(restored, map);
}
