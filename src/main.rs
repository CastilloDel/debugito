use std::{cmp::min, env, ffi::CString};

use nix::{
    libc::{SYS_write, ORIG_RAX, RDI, RDX, RSI},
    sys::{
        ptrace::{getregs, read, read_user, syscall, traceme, write, AddressType},
        wait::wait,
    },
    unistd::{execv, fork, ForkResult, Pid},
};

fn main() {
    let exec_path = env::args().nth(1).unwrap();
    match unsafe { fork() }.unwrap() {
        ForkResult::Child => {
            traceme().expect("I don't want to be traced");
            execv(&CString::new(exec_path).unwrap(), &[c""]).unwrap();
        }
        ForkResult::Parent { child: pid } => loop {
            let status = wait().unwrap();
            match status {
                nix::sys::wait::WaitStatus::Exited(_, _) => panic!("Child exited"),
                _ => {}
            }
            let registers = getregs(pid).unwrap();
            if registers.orig_rax == SYS_write as u64 {
                let rbx_content = registers.rbx;
                let string_address = registers.rsi;
                let string_length = registers.rdx;
                println!(
                    "Write with params rdi: {rbx_content} rsi: {string_address:?} rdx: {string_length}"
                );
                let read_data =
                    get_data(pid, string_address as AddressType, string_length as usize);
                let string = String::from_utf8_lossy(&read_data).into_owned();

                println!("From parent: String is {}", string);
            }
            syscall(pid, None).unwrap();
        },
    }
}

fn put_data(pid: Pid, mut address: AddressType, bytes: &[u8]) {
    let mut bytes_written = 0;
    while bytes.len() > bytes_written {
        let bytes_to_write = min(8, bytes.len() - bytes_written);
        let word = get_word_from_bytes(&bytes[bytes_written..bytes_written + bytes_to_write]);
        write(pid, address, word).expect("Couldn't write word");
        bytes_written += bytes_to_write;
        address = address.wrapping_add(bytes_to_write);
    }
}

fn get_data(pid: Pid, mut address: AddressType, length: usize) -> Vec<u8> {
    let mut bytes = Vec::<u8>::new();
    while bytes.len() < length {
        let word = read(pid, address).expect("Couldn't read word");
        let mut new_bytes = get_bytes_from_word(word);
        new_bytes.truncate(min(8, length - bytes.len()));
        bytes.append(&mut new_bytes);
        address = address.wrapping_add(8 as usize);
    }
    bytes
}

fn get_bytes_from_word(word: i64) -> Vec<u8> {
    let mut bytes = Vec::new();
    for i in 0..8 {
        let char = ((word >> (8 * i)) & 0xFF) as u8;
        bytes.push(char);
    }
    bytes
}

fn get_word_from_bytes(bytes: &[u8]) -> i64 {
    let mut word = 0;
    assert!(bytes.len() <= 8);
    for (i, &byte) in bytes.iter().enumerate() {
        word |= (byte as i64) << (i * 8);
    }
    word
}
