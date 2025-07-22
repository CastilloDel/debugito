use gimli::Register;
use nix::libc::user_regs_struct;

pub fn get_register_value(regs: &user_regs_struct, register: Register) -> anyhow::Result<u64> {
    match register.0 {
        0 => Ok(regs.rax),
        1 => Ok(regs.rdx),
        2 => Ok(regs.rcx),
        3 => Ok(regs.rbx),
        4 => Ok(regs.rsi),
        5 => Ok(regs.rdi),
        6 => Ok(regs.rbp),
        7 => Ok(regs.rsp),
        8 => Ok(regs.r8),
        9 => Ok(regs.r9),
        10 => Ok(regs.r10),
        11 => Ok(regs.r11),
        12 => Ok(regs.r12),
        13 => Ok(regs.r13),
        14 => Ok(regs.r14),
        15 => Ok(regs.r15),
        16 => Ok(regs.rip),
        _ => anyhow::bail!("Invalid register number"),
    }
}
