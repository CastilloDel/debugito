fn main() {
    for arg in std::env::args() {
        println!("{}", arg);
    }
    let mut a = 0;
    loop {
        a += 1;
        a += 1;
    }
}
