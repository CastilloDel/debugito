use nix::{
    sys::{
        ptrace::{getregs, step, traceme},
        wait::wait,
    },
    unistd::{ForkResult, execv, fork},
};
use std::{env, ffi::CString, path::Path, thread::sleep, time::Duration};

fn main() {
    let exec_path = Path::new(&env::args().nth(1).unwrap())
        .canonicalize()
        .unwrap();
    let loader = addr2line::Loader::new(exec_path.clone()).unwrap();
    match unsafe { fork() }.unwrap() {
        ForkResult::Child => {
            traceme().expect("I don't want to be traced");
            execv(&CString::new(exec_path.to_str().unwrap()).unwrap(), &[c""]).unwrap();
        }
        ForkResult::Parent { child: pid } => loop {
            loop {
                let status = wait().unwrap();
                match status {
                    nix::sys::wait::WaitStatus::Exited(_, _) => panic!("Child exited"),
                    _ => {}
                }
                let proc_map = get_range_for_program_source_code(pid.as_raw() as u64, &exec_path);
                let registers = getregs(pid).unwrap();

                if registers.rip > proc_map.address_range.begin
                    && registers.rip < proc_map.address_range.end
                {
                    match loader.find_location(
                        registers.rip - proc_map.address_range.begin + proc_map.offset,
                    ) {
                        Ok(Some(location)) => {
                            match (location.line, location.file) {
                                (Some(line), Some(file)) => {
                                    if file.contains("example") {
                                        println!("{}", exec_path.to_str().unwrap());
                                        println!("line:{} {:?}", file, line);
                                        sleep(Duration::from_secs(1));
                                    }
                                }
                                _ => {}
                            };
                        }
                        _ => {}
                    }
                }
                step(pid, None).unwrap();
            }
        },
    }
}

fn get_range_for_program_source_code(pid: u64, executable: &Path) -> rsprocmaps::Map {
    let maps = rsprocmaps::from_pid(pid as i32).unwrap();
    let executable_pathname = rsprocmaps::Pathname::Path(executable.to_str().unwrap().to_string());
    maps.into_iter()
        .map(Result::unwrap)
        .find(|map| &map.pathname == &executable_pathname && map.permissions.executable)
        .unwrap()
}
