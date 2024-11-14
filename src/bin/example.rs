use std::{thread::sleep, time::Duration};

fn main() {
    loop {
        sleep(Duration::from_secs(2));
        println!("Still alive");
    }
}
