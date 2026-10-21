fn main() {
    for w in [0xE0100291u32, 0x00000090u32] {
        println!("{w:#010X} -> {:?}", decode::decode_arm(w));
    }
}
