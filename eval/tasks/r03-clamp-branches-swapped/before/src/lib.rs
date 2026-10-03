pub fn clamp(x: i32, lo: i32, hi: i32) -> i32 {
    if x < lo {
        hi
    } else if x > hi {
        lo
    } else {
        x
    }
}
