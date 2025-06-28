use gimli::{AttributeValue, LittleEndian, Reader};
use nix::{sys::ptrace::getregs, unistd::Pid};
use object::{Object, ObjectSection};
use std::{collections::HashMap, path::PathBuf};

use crate::ProgramContext;

pub fn get_dwarf_info(buffer: &Vec<u8>) -> gimli::Dwarf<gimli::EndianReader<LittleEndian, &[u8]>> {
    let obj_file = object::File::parse(&buffer[..]).expect("Failed to parse ELF file");

    let dwarf = gimli::Dwarf::load(|name| -> Result<_, ()> {
        Ok(obj_file
            .section_by_name(name.name())
            .and_then(|section| section.data().ok())
            .map(|data| gimli::EndianReader::new(data, LittleEndian))
            .unwrap_or(gimli::EndianReader::new(&[], LittleEndian)))
    })
    .unwrap();
    dwarf
}

pub fn get_breakpoints_from_dwarf<R>(
    dwarf: &gimli::Dwarf<R>,
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
            let position = (path, line.get());
            // We only add the first address for each line
            if !breakpoints.contains_key(&position) {
                breakpoints.insert(position, address);
            }
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

pub fn print_line_info(
    context: &ProgramContext,
    pid: Pid,
    proc_map: &rsprocmaps::Map,
) -> anyhow::Result<()> {
    let registers = getregs(pid).unwrap();
    // We subtract an extra 1 because the rip was already increased by the trap instruction
    let address = registers.rip - proc_map.address_range.begin + proc_map.offset - 1;
    let dwarf = get_dwarf_info(context.file_buffer.as_ref().unwrap());
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
                let mut rows = program.resume_from(&sequence);

                while let Ok(Some((_, row))) = rows.next_row() {
                    if row.end_sequence() {
                        continue;
                    }

                    let path = match extract_path(&program, row.file_index()) {
                        Some(p) => p,
                        None => continue,
                    };

                    if let Some(line) = row.line() {
                        if address == row.address() {
                            println!("Breakpoint at {}:{}", path.to_str().unwrap(), line.get(),);
                        }
                    }
                }
            }
        }
    }
    Ok(())
}
