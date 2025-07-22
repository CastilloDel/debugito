use anyhow::{Context, anyhow};
use nix::{
    sys::{
        ptrace::{self, cont, getregs, setregs, step, traceme},
        signal::Signal::SIGTRAP,
        wait::{WaitStatus, wait},
    },
    unistd::{ForkResult, Pid, execv, fork},
};
use std::{
    collections::HashMap,
    ffi::CString,
    fs, io,
    path::{Path, PathBuf},
    str::FromStr,
};

mod dwarf;
mod registers;
mod repl;

use dwarf::DwarfInfo;
use repl::Repl;

type Address = u64;

#[derive(Default)]
struct ProgramContext {
    binary: Option<LoadedBinary>,
    running_program: Option<RunningProgram>,
    breakpoints: Vec<Breakpoint>,
}

struct LoadedBinary {
    binary_path: PathBuf,
    // Matches a breakpoint location to the address from the DWARF
    // These addresses aren't final, they need to take into account
    // where the file is loaded into memory
    possible_breakpoints: HashMap<Breakpoint, Address>,
    dwarf: DwarfInfo,
}

struct RunningProgram {
    proc_map: rsprocmaps::Map,
    // Matches the address in memory where there is a breakpoint to
    // its original instruction (after substituting it for a trap instruction)
    set_breakpoints: HashMap<Address, i64>,
    pid: Pid,
    last_status: WaitStatus,
}

fn main() -> anyhow::Result<()> {
    let mut repl = Repl::new(ProgramContext::default())
        .add_command(
            clap::Command::new("load")
                .alias("l")
                .arg(
                    clap::Arg::new("binary")
                        .required(true)
                        .help("the path to the executable binary"),
                )
                .about("load a binary to prepare for debugging"),
            load_program,
        )
        .add_command(
            clap::Command::new("breakpoint")
                .alias("b")
                .arg(
                    clap::Arg::new("where")
                        .required(true)
                        .help("in the form \"source_file:line_number\""),
                )
                .about("set a breakpoint"),
            add_breakpoint,
        )
        .add_command(
            clap::Command::new("run")
                .alias("r")
                .about("run the specified binary until finding a breakpoint"),
            run_program,
        )
        .add_command(
            clap::Command::new("continue")
                .alias("c")
                .about("Keep running the program until a breakpoint"),
            continue_program,
        )
        .add_command(
            clap::Command::new("print")
                .alias("p")
                .arg(
                    clap::Arg::new("var")
                        .required(true)
                        .help("name of the variable"),
                )
                .about("Print the value of a variable"),
            print_var,
        );
    repl.run()
}

fn load_program(args: &clap::ArgMatches, context: &mut ProgramContext) -> anyhow::Result<String> {
    if context.binary.is_some() {
        if !ask_for_confirmation(
            "Another binary was already loaded, do you want to load a new one?",
        ) {
            return Ok(String::from("Kept original binary"));
        }
    }
    let binary_path = PathBuf::from(args.get_one::<String>("binary").unwrap()).canonicalize()?;
    let file_buffer = fs::read(&binary_path).expect("Failed to read file");
    let dwarf = DwarfInfo::new(file_buffer);
    let possible_breakpoints = dwarf.get_breakpoints_from_dwarf()?;

    context.binary = Some(LoadedBinary {
        binary_path,
        dwarf,
        possible_breakpoints,
    });
    Ok(String::from("Binary loaded"))
}

fn add_breakpoint(args: &clap::ArgMatches, context: &mut ProgramContext) -> anyhow::Result<String> {
    // TODO: Support adding breakpoints while program is already running
    let loaded_binary = context
        .binary
        .as_ref()
        .ok_or(anyhow!("Please load a binary first"))?;
    let breakpoint_str = args.get_one::<String>("where").unwrap();
    let mut breakpoint: Breakpoint = breakpoint_str.parse()?;
    breakpoint.file = breakpoint.file.canonicalize()?;
    if !loaded_binary.possible_breakpoints.contains_key(&breakpoint) {
        return Ok("Not a valid breakpoint position".to_owned());
    }
    context.breakpoints.push(breakpoint);
    Ok(String::from("Breakpoint added to ") + breakpoint_str)
}

fn run_program(_: &clap::ArgMatches, context: &mut ProgramContext) -> anyhow::Result<String> {
    let binary = context
        .binary
        .as_ref()
        .ok_or(anyhow!("You need to load a binary first"))?;
    if context.running_program.is_some() {
        if !ask_for_confirmation("A program is already being run, do you want to rerun it?") {
            return Ok("The original program is still running".to_owned());
        }
    }
    if context.breakpoints.is_empty() {
        anyhow::bail!("Please set at least one breakpoint first");
    }
    let pid = launch_fork(&binary.binary_path);
    if let nix::sys::wait::WaitStatus::Exited(_, _) = wait().unwrap() {
        panic!("Child exited")
    }
    let proc_map = get_range_for_program_source_code(pid.as_raw() as u64, &binary.binary_path);
    let set_breakpoints = context
        .breakpoints
        .iter()
        .map(|breakpoint| {
            let virtual_address = binary.possible_breakpoints[breakpoint];
            setup_breakpoint(pid, virtual_address, &proc_map)
        })
        .collect();
    cont(pid, None).unwrap();
    let status = wait().unwrap();
    if let nix::sys::wait::WaitStatus::Exited(_, _) = status {
        panic!("Child exited")
    }
    let line = binary.dwarf.get_line_from_pid(pid, &proc_map)?;
    println!("Breakpoint at {}", line);
    context.running_program = Some(RunningProgram {
        proc_map,
        set_breakpoints,
        pid,
        last_status: status,
    });
    Ok(String::from("Reached breakpoint"))
}

fn ask_for_confirmation(message: &str) -> bool {
    println!("{} (y/n)", message);
    let stdin = io::stdin();
    stdin.lines().next().unwrap().unwrap() == "y"
}

fn continue_program(_: &clap::ArgMatches, context: &mut ProgramContext) -> anyhow::Result<String> {
    let running_program = context
        .running_program
        .as_mut()
        .ok_or(anyhow!("You need to run a program first"))?;
    let binary = context.binary.as_ref().unwrap(); // If there's a pid, there's a binary
    let pid = running_program.pid;
    if let WaitStatus::Stopped(pid, SIGTRAP) = running_program.last_status {
        run_original_breakpoint_instruction(pid, &running_program.set_breakpoints);
    }
    cont(pid, None).unwrap();
    let status = wait().unwrap();
    if let nix::sys::wait::WaitStatus::Exited(_, _) = status {
        panic!("Child exited")
    }
    running_program.last_status = status;
    let line = binary
        .dwarf
        .get_line_from_pid(pid, &running_program.proc_map)?;
    println!("Breakpoint at {}", line);
    Ok(String::from("Reached breakpoint"))
}

fn print_var(args: &clap::ArgMatches, context: &mut ProgramContext) -> anyhow::Result<String> {
    let variable_name = args.get_one::<String>("var").unwrap();
    let program = context
        .running_program
        .as_mut()
        .ok_or(anyhow!("You need to run a program first"))?;
    let binary = context.binary.as_mut().unwrap();
    let address = binary
        .dwarf
        .get_address_of_variable(variable_name, program.pid)?;

    let word = ptrace::read(program.pid, address as ptrace::AddressType)?;
    // TODO: Take into account the variable type, instead of assumming u32
    println!("{}", word as u32);
    Ok("".to_string())
}

fn run_original_breakpoint_instruction(pid: Pid, set_breakpoints: &HashMap<u64, i64>) {
    let mut registers = getregs(pid).unwrap();
    // We subtract an extra 1 because the rip was already increased by the trap instruction
    registers.rip -= 1;
    setregs(pid, registers).unwrap();
    let original_word = set_breakpoints[&registers.rip];
    ptrace::write(pid, registers.rip as ptrace::AddressType, original_word).unwrap();
    do_step(pid);
    let word = add_trap_instruction(original_word);
    ptrace::write(pid, registers.rip as ptrace::AddressType, word).unwrap();
}

fn setup_breakpoint(pid: Pid, virtual_address: u64, proc_map: &rsprocmaps::Map) -> (u64, i64) {
    let real_address = virtual_address + proc_map.address_range.begin - proc_map.offset;
    let original_word = ptrace::read(pid, real_address as ptrace::AddressType).unwrap();
    let word = add_trap_instruction(original_word);
    ptrace::write(pid, real_address as ptrace::AddressType, word).unwrap();
    (real_address as u64, original_word)
}

fn add_trap_instruction(word: i64) -> i64 {
    const TRAP_INSTRUCTION: i64 = 0xCC;
    // Only valid for x86
    (word & (!0xFF)) | TRAP_INSTRUCTION
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

fn do_step(pid: Pid) {
    step(pid, None).unwrap();
    let status = wait().unwrap();
    if let nix::sys::wait::WaitStatus::Exited(_, _) = status {
        panic!("Child exited")
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Breakpoint {
    file: PathBuf,
    line_number: u64,
}

impl FromStr for Breakpoint {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<Self> {
        let (file, number) = s.split_once(":").ok_or(anyhow::anyhow!("Missing :"))?;
        Ok(Self {
            file: PathBuf::from(file),
            line_number: number.parse().context("Couldn't parse line number")?,
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
