fn main() {
    let boolean = false;
    let float = 3.1234567890123456789012345678901234567890;
    let float2 = float as f64 + 5.3;
    let unsigned = u32::MAX;
    let unsigned2 = u32::MAX as u64 + 5;
    let signed = i32::MAX;
    let signed2 = i32::MAX as i64 + 5;
    for arg in std::env::args() {
        let arg_len = arg.len();
        println!("{}", arg_len);
    }
    let mut a = 0;
    loop {
        a += 1;
        a += 1;
    }
}
