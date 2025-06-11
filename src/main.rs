mod repl;
use anyhow::Context;
use gimli::{AttributeValue, LittleEndian, Reader};
use nix::{
    sys::{
        ptrace::{self, cont, step, traceme},
        wait::wait,
    },
    unistd::{ForkResult, Pid, execv, fork},
};
use object::{Object, ObjectSection};
use repl::Repl;
use std::{
    collections::HashMap,
    ffi::CString,
    fs,
    path::{Path, PathBuf},
    str::FromStr,
};

#[derive(Default)]
struct ProgramContext {
    breakpoints: Vec<Breakpoint>,
    binary_path: Option<PathBuf>,
    // Matches source file + line number to the address from the DWARF
    // These addresses aren't final, they need to take into account
    // where the file is loaded into memory
    possible_breakpoints: HashMap<(PathBuf, u64), Address>,
    // Matches the address in memory where there is a breakpoint to
    // its original instruction (after substituting it for a trap instruction)
    set_breakpoints: HashMap<Address, u8>,
}

type Address = u64;

fn main() -> anyhow::Result<()> {
    let mut repl = Repl::new(ProgramContext::default())
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
            clap::Command::new("run")
                .alias("r")
                .about("run the specified binary until finding a breakpoint"),
            run_program,
        );
    repl.run()
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

fn load_program(args: &clap::ArgMatches, context: &mut ProgramContext) -> anyhow::Result<String> {
    if context.binary_path.is_some() {
        return Ok(String::from("Another binary was already loaded"));
    }
    let exec_path = PathBuf::from(args.get_one::<String>("binary").unwrap()).canonicalize()?;
    let buffer = fs::read(&exec_path).expect("Failed to read file");

    let obj_file = object::File::parse(&*buffer).expect("Failed to parse ELF file");

    let dwarf = gimli::Dwarf::load(|name| -> Result<_, ()> {
        Ok(obj_file
            .section_by_name(name.name())
            .and_then(|section| section.data().ok())
            .map(|data| gimli::EndianReader::new(data, LittleEndian))
            .unwrap_or(gimli::EndianReader::new(&[], LittleEndian)))
    })
    .unwrap();

    context.binary_path = Some(exec_path);
    context.possible_breakpoints = get_breakpoints_from_dwarf(dwarf)?;
    Ok(String::from("Binary loaded"))
}

fn get_breakpoints_from_dwarf<R>(
    dwarf: gimli::Dwarf<R>,
) -> Result<HashMap<(PathBuf, u64), u64>, anyhow::Error>
where
    R: gimli::Reader + Copy,
{
    let mut breakpoints = HashMap::new();
    let mut units = dwarf.units();

    while let Some(header) = units.next()? {
        let unit = dwarf.unit(header)?;
        let mut entries = unit.entries();

        while let Some((_, entry)) = entries.next_dfs()? {
            if entry.tag() != gimli::constants::DW_TAG_compile_unit {
                continue;
            }

            let offset = match get_line_program_offset(entry) {
                Some(offset) => offset,
                None => continue,
            };

            let line_program = dwarf.debug_line.program(
                offset,
                header.address_size(),
                unit.comp_dir,
                unit.name,
            )?;

            let (program, sequences) = line_program.sequences()?;

            for sequence in sequences {
                breakpoints.extend(process_sequence(&program, &sequence)?);
            }
        }
    }

    Ok(breakpoints)
}

fn process_sequence<R>(
    program: &gimli::CompleteLineProgram<R>,
    sequence: &gimli::LineSequence<R>,
) -> Result<HashMap<(PathBuf, u64), u64>, anyhow::Error>
where
    R: gimli::Reader,
{
    let mut rows = program.resume_from(sequence);
    let mut breakpoints = HashMap::new();

    while let Ok(Some((_, row))) = rows.next_row() {
        if row.end_sequence() {
            continue;
        }

        let path = match extract_path(program, row.file_index()) {
            Some(p) => p,
            None => continue,
        };

        if let Some(line) = row.line() {
            let address = row.address();
            breakpoints.insert((path, line.get()), address);
        }
    }

    Ok(breakpoints)
}

fn extract_path<R>(program: &gimli::CompleteLineProgram<R>, file_index: u64) -> Option<PathBuf>
where
    R: gimli::Reader,
{
    let header = program.header();
    let file = header.file(file_index)?;

    let dir = match file.directory(header)? {
        gimli::AttributeValue::String(s) => PathBuf::from(s.to_string().ok()?.into_owned()),
        _ => return None,
    };

    let file_name = match file.path_name() {
        gimli::AttributeValue::String(s) => s.to_string().ok()?.into_owned(),
        _ => return None,
    };

    dir.join(file_name).canonicalize().ok()
}

fn get_line_program_offset<R>(
    entry: &gimli::DebuggingInformationEntry<'_, '_, R, <R as Reader>::Offset>,
) -> Option<gimli::DebugLineOffset<R::Offset>>
where
    R: Reader + Copy,
{
    if let AttributeValue::DebugLineRef(offset) =
        entry.attr(gimli::constants::DW_AT_stmt_list).ok()??.value()
    {
        return Some(offset);
    }
    None
}

fn run_program(_: &clap::ArgMatches, context: &mut ProgramContext) -> anyhow::Result<String> {
    let binary = match &context.binary_path {
        Some(binary) => binary,
        None => return Ok(String::from("You need to load a binary first")),
    };
    let pid = launch_fork(&binary);
    let status = wait().unwrap();
    if let nix::sys::wait::WaitStatus::Exited(_, _) = status {
        panic!("Child exited")
    }
    if context.breakpoints.is_empty() {
        anyhow::bail!("Please set at least one breakpoint first");
    }
    let proc_map = get_range_for_program_source_code(pid.as_raw() as u64, &binary);
    setup_breakpoints(pid, context, &proc_map);
    cont(pid, None).unwrap();
    let status = wait().unwrap();
    if let nix::sys::wait::WaitStatus::Exited(_, _) = status {
        panic!("Child exited")
    }
    Ok(String::from("Reached breakpoint"))
}

fn setup_breakpoints(pid: Pid, context: &mut ProgramContext, proc_map: &rsprocmaps::Map) {
    for breakpoint in &context.breakpoints {
        let virtual_address =
            context.possible_breakpoints[&(breakpoint.file.clone(), breakpoint.line_number)];
        let real_address = virtual_address + proc_map.address_range.begin - proc_map.offset;
        let mut word = ptrace::read(pid, real_address as ptrace::AddressType).unwrap();
        const TRAP_INSTRUCTION: i64 = 0xCC; // Only valid for x86
        let original_instruction = (word & (!0xFF)) as u8;
        word = (word & (!0xFF)) | TRAP_INSTRUCTION;
        ptrace::write(pid, real_address as ptrace::AddressType, word).unwrap();
        context
            .set_breakpoints
            .insert(real_address, original_instruction);
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

fn do_step(pid: Pid) {
    step(pid, None).unwrap();
    let status = wait().unwrap();
    if let nix::sys::wait::WaitStatus::Exited(_, _) = status {
        panic!("Child exited")
    }
}

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
