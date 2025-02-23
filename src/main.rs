use nix::{
    sys::{
        ptrace::{getregs, step, traceme},
        wait::wait,
    },
    unistd::{ForkResult, Pid, execv, fork},
};
use std::{
    ffi::CString,
    io::{self, Write},
    path::{Path, PathBuf},
    str::FromStr,
};

fn main() {
    println!("Debugito!");
    println!("For help, type help");
    let mut input = String::new();
    let mut breakpoints = Vec::new();
    loop {
        print!("debugito => ");
        io::stdout().flush().unwrap();

        input.clear();
        io::stdin().read_line(&mut input).unwrap();
        let (command, args) = input.trim().split_once(" ").unwrap_or((&input, ""));
        match command {
            "b" | "breakpoint" => breakpoints.push(args.parse().unwrap()),
            "r" | "run" => {
                let exec_path = Path::new(&args).canonicalize().unwrap();
                let loader = addr2line::Loader::new(exec_path.clone()).unwrap();
                let pid = launch_fork(&exec_path);
                let status = wait().unwrap();
                if let nix::sys::wait::WaitStatus::Exited(_, _) = status {
                    panic!("Child exited")
                }
                let proc_map = get_range_for_program_source_code(pid.as_raw() as u64, &exec_path);
                continue_until_breakpoint(pid, &breakpoints[0], &proc_map, &loader);
                println!("Here we are");
            }
            // "c" | "continue" =>
            //     continue_until_breakpoint(pid, &breakpoints[0], &proc_map, &loader);
            // ,
            "exit" => return,
            _ => println!("Unrecognized instruction. For help, type help"),
        }
    }
}

fn launch_fork(executable: &Path) -> Pid {
    match unsafe { fork() }.unwrap() {
        ForkResult::Child => {
            traceme().expect("I don't want to be traced");
            execv(&CString::new(executable.to_str().unwrap()).unwrap(), &[c""]).unwrap();
            unreachable!()
        }
        ForkResult::Parent { child: pid } => return pid,
    }
}

fn continue_until_breakpoint(
    pid: Pid,
    breakpoint: &Breakpoint,
    proc_map: &rsprocmaps::Map,
    loader: &addr2line::Loader,
) {
    loop {
        do_step(pid);
        let registers = getregs(pid).unwrap();
        if registers.rip < proc_map.address_range.begin
            || registers.rip >= proc_map.address_range.end
        {
            continue;
        }
        let address_in_file = registers.rip - proc_map.address_range.begin + proc_map.offset;
        if let Ok(Some(location)) = loader.find_location(address_in_file) {
            if let (Some(line), Some(file)) = (location.line, location.file) {
                if file == breakpoint.file.to_str().unwrap()
                    && line as usize == breakpoint.line_number
                {
                    println!("line:{} {:?}", file, line);
                    return;
                }
            };
        }
    }
}

fn do_step(pid: Pid) {
    step(pid, None).unwrap();
    let status = wait().unwrap();
    if let nix::sys::wait::WaitStatus::Exited(_, _) = status {
        panic!("Child exited")
    }
}

struct Breakpoint {
    file: PathBuf,
    line_number: usize,
}

impl FromStr for Breakpoint {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (file, number) = s.split_once(":").unwrap();
        Ok(Self {
            file: Path::new(file).canonicalize().unwrap(),
            line_number: number.parse().unwrap(),
        })
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
