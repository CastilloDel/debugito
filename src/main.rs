use anyhow::Context;
use nix::{
    sys::{
        ptrace::{self, cont, getregs, setregs, step, traceme},
        wait::wait,
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
mod repl;

use dwarf::{get_breakpoints_from_dwarf, get_dwarf_info, print_line_info};
use repl::Repl;

type Address = u64;

#[derive(Default)]
struct ProgramContext {
    breakpoints: Vec<Breakpoint>,
    binary_path: Option<PathBuf>,
    file_buffer: Option<Vec<u8>>,
    // Matches source file + line number to the address from the DWARF
    // These addresses aren't final, they need to take into account
    // where the file is loaded into memory
    possible_breakpoints: HashMap<(PathBuf, u64), Address>,
    // Matches the address in memory where there is a breakpoint to
    // its original instruction (after substituting it for a trap instruction)
    set_breakpoints: HashMap<Address, i64>,
    pid: Option<Pid>,
    in_breakpoint: bool,
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
                .about("Keep running the program until another breakpoint"),
            continue_program,
        );
    repl.run()
}

fn load_program(args: &clap::ArgMatches, context: &mut ProgramContext) -> anyhow::Result<String> {
    if context.binary_path.is_some() {
        return Ok(String::from("Another binary was already loaded"));
    }
    let exec_path = PathBuf::from(args.get_one::<String>("binary").unwrap()).canonicalize()?;
    let buffer = fs::read(&exec_path).expect("Failed to read file");

    let dwarf = get_dwarf_info(&buffer);

    context.binary_path = Some(exec_path);
    context.possible_breakpoints = get_breakpoints_from_dwarf(&dwarf)?;
    context.file_buffer = Some(buffer);
    Ok(String::from("Binary loaded"))
}

fn add_breakpoint(args: &clap::ArgMatches, context: &mut ProgramContext) -> anyhow::Result<String> {
    let breakpoint_str = args.get_one::<String>("where").unwrap();
    let mut breakpoint: Breakpoint = breakpoint_str.parse()?;
    breakpoint.file = breakpoint.file.canonicalize()?;
    if context.binary_path.is_none() {
        return Ok("Please load a binary first".to_owned());
    };
    if !context
        .possible_breakpoints
        .contains_key(&(breakpoint.file.clone(), breakpoint.line_number))
    {
        return Ok("Not a valid breakpoint position".to_owned());
    }
    context.breakpoints.push(breakpoint);
    Ok(String::from("Breakpoint added to ") + breakpoint_str)
}

fn run_program(_: &clap::ArgMatches, context: &mut ProgramContext) -> anyhow::Result<String> {
    let binary = match &context.binary_path {
        Some(binary) => binary,
        None => return Ok(String::from("You need to load a binary first")),
    };
    if !context.set_breakpoints.is_empty() {
        println!("A program is already being run, do you want to rerun it? (y/n)");
        let stdin = io::stdin();
        if stdin.lines().next().unwrap().unwrap() != "y" {
            return Ok("The original program will be left running".to_owned());
        }
    }
    let pid = launch_fork(&binary);
    context.pid = Some(pid);
    let status = wait().unwrap();
    if let nix::sys::wait::WaitStatus::Exited(_, _) = status {
        panic!("Child exited")
    }
    if context.breakpoints.is_empty() {
        anyhow::bail!("Please set at least one breakpoint first");
    }
    let proc_map = get_range_for_program_source_code(pid.as_raw() as u64, &binary);
    for breakpoint in context.breakpoints.clone() {
        setup_breakpoint(pid, context, &proc_map, &breakpoint);
    }
    cont(pid, None).unwrap();
    let status = wait().unwrap();
    if let nix::sys::wait::WaitStatus::Exited(_, _) = status {
        panic!("Child exited")
    }
    print_line_info(context, pid, &proc_map)?;
    context.in_breakpoint = true;
    Ok(String::from("Reached breakpoint"))
}

fn continue_program(
    _args: &clap::ArgMatches,
    context: &mut ProgramContext,
) -> anyhow::Result<String> {
    let pid = match context.pid {
        Some(pid) => pid,
        None => return Ok(String::from("You need to run a binary first")),
    };
    let binary = context.binary_path.as_ref().unwrap(); // If there's a pid, there's a binary
    let proc_map = get_range_for_program_source_code(pid.as_raw() as u64, binary);
    // Before actually calling cont, we need to run the original instruction
    if context.in_breakpoint {
        let mut registers = getregs(pid).unwrap();
        // We subtract an extra 1 because the rip was already increased by the trap instruction
        registers.rip -= 1;
        setregs(pid, registers).unwrap();
        let original_word = context.set_breakpoints[&registers.rip];
        ptrace::write(pid, registers.rip as ptrace::AddressType, original_word).unwrap();
        do_step(pid);
        let word = add_trap_instruction(original_word);
        ptrace::write(pid, registers.rip as ptrace::AddressType, word).unwrap();
    }
    cont(pid, None).unwrap();
    if let nix::sys::wait::WaitStatus::Exited(_, _) = wait().unwrap() {
        panic!("Child exited")
    }
    print_line_info(context, pid, &proc_map)?;
    context.in_breakpoint = true;
    Ok(String::from("Reached breakpoint"))
}

fn setup_breakpoint(
    pid: Pid,
    context: &mut ProgramContext,
    proc_map: &rsprocmaps::Map,
    breakpoint: &Breakpoint,
) {
    let virtual_address =
        context.possible_breakpoints[&(breakpoint.file.clone(), breakpoint.line_number)];
    let real_address = virtual_address + proc_map.address_range.begin - proc_map.offset;
    let original_word = ptrace::read(pid, real_address as ptrace::AddressType).unwrap();
    let word = add_trap_instruction(original_word);
    ptrace::write(pid, real_address as ptrace::AddressType, word).unwrap();
    context.set_breakpoints.insert(real_address, original_word);
}

fn add_trap_instruction(original_word: i64) -> i64 {
    const TRAP_INSTRUCTION: i64 = 0xCC;
    // Only valid for x86
    let word = (original_word & (!0xFF)) | TRAP_INSTRUCTION;
    word
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

#[derive(Clone, Debug)]
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
