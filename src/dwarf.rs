use anyhow::bail;
use gimli::{AttributeValue, LittleEndian, Reader};
use nix::{sys::ptrace::getregs, unistd::Pid};
use object::{Object, ObjectSection};
use std::{collections::HashMap, path::PathBuf, rc::Rc};

use crate::Breakpoint;

pub struct DwarfInfo {
    inner: gimli::Dwarf<gimli::EndianReader<LittleEndian, Rc<[u8]>>>,
}

impl DwarfInfo {
    pub fn new(buffer: Vec<u8>) -> Self {
        let obj_file = object::File::parse(buffer.as_slice()).expect("Failed to parse ELF file");

        let dwarf = gimli::Dwarf::load(
            |name| -> Result<gimli::EndianReader<LittleEndian, Rc<[u8]>>, ()> {
                let section = obj_file
                    .section_by_name(name.name())
                    .and_then(|section| section.data().ok())
                    .map(|data| gimli::EndianReader::new(data, LittleEndian))
                    .unwrap_or(gimli::EndianReader::new(&[], LittleEndian))
                    .to_vec();

                Ok(gimli::EndianReader::new(Rc::from(section), LittleEndian))
            },
        )
        .expect("Failed to load DWARF data");

        DwarfInfo { inner: dwarf }
    }

    pub fn get_breakpoints_from_dwarf(&self) -> Result<HashMap<Breakpoint, u64>, anyhow::Error> {
        let mut breakpoints = HashMap::new();
        let mut units = self.inner.units();

        while let Some(header) = units.next()? {
            let unit = self.inner.unit(header.clone())?;
            let comp_dir = unit.comp_dir.clone();
            let comp_name = unit.name.clone();
            let mut entries = unit.entries();

            while let Some((_, entry)) = entries.next_dfs()? {
                if entry.tag() != gimli::constants::DW_TAG_compile_unit {
                    continue;
                }

                let offset = match get_line_program_offset(entry) {
                    Some(offset) => offset,
                    None => continue,
                };

                let line_program = self.inner.debug_line.program(
                    offset,
                    header.address_size(),
                    comp_dir.clone(),
                    comp_name.clone(),
                )?;

                let (program, sequences) = line_program.sequences()?;

                for sequence in sequences {
                    breakpoints.extend(process_sequence(&program, &sequence)?);
                }
            }
        }

        Ok(breakpoints)
    }

    pub fn get_line_from_pid(
        &self,
        pid: Pid,
        proc_map: &rsprocmaps::Map,
    ) -> anyhow::Result<String> {
        let registers = getregs(pid).unwrap();
        // We subtract an extra 1 because the rip was already increased by the trap instruction
        let address = registers.rip - proc_map.address_range.begin + proc_map.offset - 1;
        let mut units = self.inner.units();

        while let Some(header) = units.next()? {
            let unit = self.inner.unit(header.clone())?;
            let comp_dir = unit.comp_dir.clone();
            let comp_name = unit.name.clone();
            let mut entries = unit.entries();

            while let Some((_, entry)) = entries.next_dfs()? {
                if entry.tag() != gimli::constants::DW_TAG_compile_unit {
                    continue;
                }

                let offset = match get_line_program_offset(entry) {
                    Some(offset) => offset,
                    None => continue,
                };

                let line_program = self.inner.debug_line.program(
                    offset,
                    header.address_size(),
                    comp_dir.clone(),
                    comp_name.clone(),
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
                                return Ok(format!("{}:{}", path.to_str().unwrap(), line.get()));
                            }
                        }
                    }
                }
            }
        }
        bail!("Couldn't find the source code for the address")
    }
}

fn process_sequence<R>(
    program: &gimli::CompleteLineProgram<R>,
    sequence: &gimli::LineSequence<R>,
) -> Result<HashMap<Breakpoint, u64>, anyhow::Error>
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
            let breakpoint = Breakpoint {
                file: path,
                line_number: line.get(),
            };
            // We only add the first address for each line
            if !breakpoints.contains_key(&breakpoint) {
                breakpoints.insert(breakpoint, address);
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
    R: Reader,
{
    if let AttributeValue::DebugLineRef(offset) =
        entry.attr(gimli::constants::DW_AT_stmt_list).ok()??.value()
    {
        return Some(offset);
    }
    None
}
