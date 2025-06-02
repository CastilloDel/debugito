mod repl;
use anyhow::Context;
use gimli::{AttributeValue, LittleEndian, Reader};
use nix::{
    sys::{
        ptrace::{getregs, step, traceme},
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
    binary: Option<LoadedBinary>,
}

struct LoadedBinary {
    possible_breakpoints: HashMap<(PathBuf, u64), u64>,
    path: PathBuf,
}

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
    let breakpoint: Breakpoint = breakpoint_str.parse()?;
    let binary = match &context.binary {
        Some(binary) => binary,
        None => return Ok("Please load a binary first".to_owned()),
    };
    if !binary
        .possible_breakpoints
        .contains_key(&(breakpoint.file.clone(), breakpoint.line_number))
    {
        return Ok("Not a valid breakpoint position".to_owned());
    }
    context.breakpoints.push(breakpoint);
    Ok(String::from("Breakpoint added to ") + breakpoint_str)
}

fn load_program(args: &clap::ArgMatches, context: &mut ProgramContext) -> anyhow::Result<String> {
    if context.binary.is_some() {
        return Ok(String::from("Another binary was already loaded"));
    }
    let exec_path = PathBuf::from(args.get_one::<String>("binary").unwrap());
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

    context.binary = Some(LoadedBinary {
        possible_breakpoints: get_breakpoints_from_dwarf(dwarf)?,
        path: exec_path,
    });
    Ok(String::from("Binary loaded"))
}

fn get_breakpoints_from_dwarf<R>(
    dwarf: gimli::Dwarf<R>,
) -> Result<HashMap<(PathBuf, u64), u64>, anyhow::Error>
where
    R: Reader + Copy,
{
    let mut units = dwarf.units();
    let mut possible_breakpoints = HashMap::new();
    while let Some(header) = units.next()? {
        let unit = dwarf.unit(header).expect("unit load");
        let mut entries = unit.entries();

        // Search for the compile_unit DIE
        while let Some((_, entry)) = entries.next_dfs()? {
            if entry.tag() != gimli::constants::DW_TAG_compile_unit {
                continue;
            }
            // Get DW_AT_stmt_list, which points to the line program
            let mut line_program_offset = None;

            if let Some(attr) = entry.attr(gimli::constants::DW_AT_stmt_list)? {
                if let AttributeValue::DebugLineRef(offset) = attr.value() {
                    line_program_offset = Some(offset);
                }
            }

            if let Some(offset) = line_program_offset {
                let program = dwarf
                    .debug_line
                    .program(offset, header.address_size(), unit.comp_dir, unit.name)
                    .expect("failed to parse line program");

                let (line_program, sequences) =
                    program.sequences().expect("failed to run line program");

                // Now get file/line info for each address in the line table
                for sequence in sequences {
                    let mut rows = line_program.resume_from(&sequence);
                    while let Ok(Some((_, row))) = rows.next_row() {
                        if row.end_sequence() {
                            continue;
                        }

                        // Translate file index to filename
                        let file = line_program
                            .header()
                            .file(row.file_index())
                            .and_then(|f| {
                                let dir_str = match f.directory(line_program.header()).unwrap() {
                                    AttributeValue::String(s) => {
                                        s.to_string().unwrap().into_owned()
                                    }
                                    _ => unreachable!(),
                                };

                                let file_str = match f.path_name() {
                                    AttributeValue::String(s) => {
                                        s.to_string().unwrap().into_owned()
                                    }
                                    _ => unreachable!(),
                                };

                                let mut dir = PathBuf::from(dir_str);
                                dir.push(file_str);
                                Some(dir)
                            })
                            .unwrap();

                        if let Some(line) = row.line() {
                            let address = row.address();
                            println!("Breakpointable: 0x{:x} => {:?}:{:?}", address, file, line);
                            possible_breakpoints.insert((file, line.get()), address);
                        }
                    }
                }
            }
        }
    }
    Ok(possible_breakpoints)
}

fn run_program(_: &clap::ArgMatches, context: &mut ProgramContext) -> anyhow::Result<String> {
    let binary = match &context.binary {
        Some(binary) => binary,
        None => return Ok(String::from("You need to load a binary first")),
    };
    let loader = addr2line::Loader::new(&binary.path).unwrap();
    let pid = launch_fork(&binary.path);
    let status = wait().unwrap();
    if let nix::sys::wait::WaitStatus::Exited(_, _) = status {
        panic!("Child exited")
    }
    if context.breakpoints.is_empty() {
        anyhow::bail!("Please set at least one breakpoint first");
    }
    setup_breakpoint(&context.breakpoints[0]);
    let proc_map = get_range_for_program_source_code(pid.as_raw() as u64, &binary.path);
    continue_until_breakpoint(pid, &context.breakpoints[0], &proc_map, &loader);
    Ok(String::from("Reached breakpoint"))
}

fn setup_breakpoint(breakpoint: &Breakpoint) {
    // TODO: Actually set up the break point before hand with a trap instruction,
    // instead of just single stepping until the correct line is reached
    todo!()
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
                    && line as u64 == breakpoint.line_number
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
