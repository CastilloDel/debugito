use std::{env, ffi::CString};

use nix::{
    libc::{SYS_write, ORIG_RAX, RAX, RBX, RCX, RDX},
    sys::{
        ptrace::{read_user, syscall, traceme, AddressType},
        wait::wait,
    },
    unistd::{execv, fork, ForkResult},
};

fn main() {
    let exec_path = env::args().nth(1).unwrap();
    match unsafe { fork() }.unwrap() {
        ForkResult::Child => {
            traceme().expect("I don't want to be traced");
            execv(&CString::new(exec_path).unwrap(), &[c""]).unwrap();
        }
        ForkResult::Parent { child: pid } => {
            let mut in_syscall = false;
            loop {
                let status = wait().unwrap();
                match status {
                    nix::sys::wait::WaitStatus::Exited(_, _) => panic!("Child exited"),
                    _ => {}
                }
                let offset: AddressType = (8 * ORIG_RAX) as AddressType;
                let syscall_number = read_user(pid, offset).unwrap();
                if syscall_number == SYS_write {
                    if !in_syscall {
                        let offset: AddressType = (8 * RBX) as AddressType;
                        let rbx_content = read_user(pid, offset).unwrap();
                        let offset: AddressType = (8 * RCX) as AddressType;
                        let rcx_content = read_user(pid, offset).unwrap();
                        let offset: AddressType = (8 * RDX) as AddressType;
                        let rdx_content = read_user(pid, offset).unwrap();
                        println!(
                    "Write with params rbx: {rbx_content} rcx: {rcx_content} rdx: {rdx_content}"
                );
                        in_syscall = true;
                    } else {
                        let offset: AddressType = (8 * RAX) as AddressType;
                        let rax_content = read_user(pid, offset).unwrap();
                        println!("Write returned rax: {rax_content}");
                        in_syscall = false;
                    }
                }
                syscall(pid, None).unwrap();
            }
        }
    }
}
