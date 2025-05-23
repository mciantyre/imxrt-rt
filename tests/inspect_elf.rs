//! Automatically inspect the programs generated by the examples.
//!
//! Do not refer to this as a specification for the runtime. These values
//! are subject to change.

#![allow(clippy::unusual_byte_groupings)] // Spacing delimits ITCM / DTCM / OCRAM banks.

use goblin::elf::Elf;
use std::{fs, path::PathBuf, process::Command};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// Build an example with optional environment variables, returning a path to the ELF.
fn cargo_build_with_envs(board: &str, envs: &[(&str, &str)]) -> Result<PathBuf> {
    let status = Command::new("cargo")
        .arg("build")
        .arg("--example=blink-rtic")
        .arg(format!("--features=board/{},board/rtic", board))
        .arg("--target=thumbv7em-none-eabihf")
        .arg(format!("--target-dir=target/{}", board))
        .arg("--quiet")
        .envs(envs.iter().copied())
        .spawn()?
        .wait()?;

    // TODO(summivox): `ExitStatus::exit_ok()` stabilization (can be chained after the `.wait()?)
    if !status.success() {
        return Err(format!(
            "Building board '{}' failed: process returned {:?}",
            board, status,
        )
        .into());
    }

    let path = PathBuf::from(format!(
        "target/{}/thumbv7em-none-eabihf/debug/examples/blink-rtic",
        board
    ));
    Ok(path)
}

fn cargo_build_nonboot(board: &str) -> Result<PathBuf> {
    let status = Command::new("cargo")
        .arg("build")
        .arg("--example=blink-rtic")
        .arg(format!(
            "--features=board/{},board/rtic,board/nonboot",
            board
        ))
        .arg("--target=thumbv7em-none-eabihf")
        .arg(format!("--target-dir=target/{}-nonboot", board))
        .arg("--quiet")
        .spawn()?
        .wait()?;

    // TODO(summivox): `ExitStatus::exit_ok()` stabilization (can be chained after the `.wait()?)
    if !status.success() {
        return Err(format!(
            "Building board '{}' failed: process returned {:?}",
            board, status,
        )
        .into());
    }

    let path = PathBuf::from(format!(
        "target/{}-nonboot/thumbv7em-none-eabihf/debug/examples/blink-rtic",
        board
    ));
    Ok(path)
}

/// Build an example, returning a path to the ELF.
fn cargo_build(board: &str) -> Result<PathBuf> {
    cargo_build_with_envs(board, &[])
}

struct ImxrtBinary<'a> {
    elf: &'a Elf<'a>,
    contents: &'a [u8],
}

/// Image vector table.
///
/// Not to be confused with the ARM vector table. See the linker
/// script for more information.
struct Ivt {
    magic_header: u32,
    interrupt_vector_table: u32,
    device_configuration_data: u32,
    boot_data: u32,
}

impl<'a> ImxrtBinary<'a> {
    fn new(elf: &'a Elf<'a>, contents: &'a [u8]) -> Self {
        Self { elf, contents }
    }

    fn symbol(&self, symbol_name: &str) -> Option<goblin::elf::Sym> {
        self.elf
            .syms
            .iter()
            .flat_map(|sym| self.elf.strtab.get_at(sym.st_name).map(|name| (sym, name)))
            .find(|(_, name)| symbol_name == *name)
            .map(|(sym, _)| sym)
    }

    fn symbol_value(&self, symbol_name: &str) -> Option<u64> {
        self.symbol(symbol_name).map(|sym| sym.st_value)
    }

    fn fcb(&self) -> Result<Fcb> {
        self.symbol("FLEXSPI_CONFIGURATION_BLOCK")
            .map(|sym| Fcb {
                address: sym.st_value,
                size: sym.st_size,
            })
            .ok_or_else(|| {
                String::from("Could not find FLEXSPI_CONFIGURATION_BLOCK in program").into()
            })
    }

    fn read_u32(&self, offset: usize) -> u32 {
        u32::from_le_bytes(self.contents[offset..offset + 4].try_into().unwrap())
    }

    fn ivt(&self) -> Result<Ivt> {
        let ivt_at_runtime = self
            .symbol_value("__ivt")
            .ok_or_else(|| String::from("Could not find __ivt symbol"))?;
        let (boot_section_offset, boot_section_at_runtime) = self
            .section_header(".boot")
            .map(|sec| (sec.sh_offset, sec.sh_addr))
            .ok_or_else(|| String::from("Could not find '.boot' section"))?;
        let ivt_offset =
            (boot_section_offset + (ivt_at_runtime - boot_section_at_runtime)) as usize;
        Ok(Ivt {
            magic_header: self.read_u32(ivt_offset),
            interrupt_vector_table: self.read_u32(ivt_offset + 4),
            device_configuration_data: self.read_u32(ivt_offset + 12),
            boot_data: self.read_u32(ivt_offset + 16),
        })
    }

    fn flexram_config(&self) -> Result<u64> {
        self.symbol("__flexram_config")
            .map(|sym| sym.st_value)
            .ok_or_else(|| String::from("Could not find FlexRAM configuration in program").into())
    }

    fn section_header(&self, section_name: &str) -> Option<&goblin::elf::SectionHeader> {
        self.elf
            .section_headers
            .iter()
            .flat_map(|sec| {
                self.elf
                    .shdr_strtab
                    .get_at(sec.sh_name)
                    .map(|name| (sec, name))
            })
            .find(|(_, name)| section_name == *name)
            .map(|(sec, _)| sec)
    }

    fn section(&self, section_name: &str) -> Result<Section> {
        self.section_header(section_name)
            .map(|sec| Section {
                address: sec.sh_addr,
                size: sec.sh_size,
            })
            .ok_or_else(|| format!("Could not find {section_name} in program").into())
    }

    fn section_lma(&self, section_name: &str) -> u64 {
        let sec = self
            .section_header(section_name)
            .unwrap_or_else(|| panic!("Section {section_name} not found"));

        let contains_section = |phdr: &&goblin::elf::ProgramHeader| {
            // The section resides in this part of the program.
            sec.sh_offset >= phdr.p_offset
                && (sec.sh_offset - phdr.p_offset) + sec.sh_size <= phdr.p_filesz
            // The section's address fits in the program's memory.
                && sec.sh_addr >= phdr.p_vaddr
                && (sec.sh_addr - phdr.p_vaddr) + sec.sh_size <= phdr.p_memsz
        };

        self.elf
            .program_headers
            .iter()
            .filter(|phdr| goblin::elf::program_header::PT_LOAD == phdr.p_type)
            .find(contains_section)
            .map(|phdr| sec.sh_addr + phdr.p_paddr - phdr.p_vaddr)
            .unwrap_or(sec.sh_addr) // VMA == LMA
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Fcb {
    address: u64,
    size: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Section {
    address: u64,
    size: u64,
}

const DTCM: u64 = 0x2000_0000;
const ITCM: u64 = 0x0000_0000;

const fn aligned(value: u64, alignment: u64) -> u64 {
    (value + (alignment - 1)) & !(alignment - 1)
}

#[test]
#[ignore = "building an example can take time"]
fn imxrt1010evk() {
    let path = cargo_build("imxrt1010evk").expect("Unable to build example");
    let contents = fs::read(path).expect("Could not read ELF file");
    let elf = Elf::parse(&contents).expect("Could not parse ELF");

    let binary = ImxrtBinary::new(&elf, &contents);
    assert_eq!(
        binary.symbol_value("__dcd_start"),
        binary.symbol_value("__dcd_end")
    );
    assert_eq!(
        Fcb {
            address: 0x6000_0400,
            size: 512
        },
        binary.fcb().unwrap()
    );
    assert_eq!(binary.flexram_config().unwrap(), 0b11_10_0101);

    let ivt = binary.ivt().unwrap();
    assert_eq!(ivt.magic_header, 0x402000D1);
    assert_eq!(ivt.interrupt_vector_table, 0x6000_2000);
    assert_eq!(ivt.device_configuration_data, 0);
    assert_eq!(
        ivt.boot_data as u64,
        binary.symbol_value("__ivt").unwrap() + 32
    );

    let stack = binary.section(".stack").unwrap();
    assert_eq!(
        Section {
            address: DTCM,
            size: 8 * 1024
        },
        stack,
        "stack not at ORIGIN(DTCM), or not 8 KiB large"
    );
    assert_eq!(binary.section_lma(".stack"), stack.address);

    let vector_table = binary.section(".vector_table").unwrap();
    assert_eq!(
        Section {
            address: stack.address + stack.size,
            size: 16 * 4 + 240 * 4
        },
        vector_table,
        "vector table not at expected VMA behind the stack"
    );
    assert!(
        vector_table.address % 1024 == 0,
        "vector table is not 1024-byte aligned"
    );
    assert_eq!(binary.section_lma(".vector_table"), 0x6000_2000);

    let xip = binary.section(".xip").unwrap();
    let text = binary.section(".text").unwrap();
    assert_eq!(text.address, ITCM, "text");
    assert_eq!(
        binary.section_lma(".text"),
        aligned(0x6000_2000 + vector_table.size + xip.size, 4),
        "text VMA expected behind vector table"
    );

    let rodata = binary.section(".rodata").unwrap();
    assert_eq!(
        rodata.address,
        aligned(0x6000_2000 + vector_table.size + text.size + xip.size, 4),
        "rodata LMA & VMA expected behind text"
    );
    assert_eq!(rodata.address, binary.section_lma(".rodata"));

    let data = binary.section(".data").unwrap();
    assert_eq!(data.address, 0x2020_0000, "data VMA in OCRAM");
    assert_eq!(
        data.size, 4,
        "blink-rtic expected to have a single static mut u32"
    );
    assert_eq!(
        binary.section_lma(".data"),
        rodata.address + aligned(rodata.size, 4),
        "data LMA starts behind rodata"
    );

    let bss = binary.section(".bss").unwrap();
    assert_eq!(
        bss.address,
        data.address + aligned(data.size, 4),
        "bss in OCRAM behind data"
    );
    assert_eq!(binary.section_lma(".bss"), bss.address, "bss is NOLOAD");

    let uninit = binary.section(".uninit").unwrap();
    assert_eq!(
        uninit.address,
        bss.address + aligned(bss.size, 4),
        "uninit in OCRAM behind bss"
    );
    assert_eq!(
        binary.section_lma(".uninit"),
        uninit.address,
        "uninit is NOLOAD"
    );

    let heap = binary.section(".heap").unwrap();
    assert_eq!(
        Section {
            address: vector_table.address + vector_table.size,
            size: 1024
        },
        heap,
        "1 KiB heap in DTCM behind vector table"
    );
    assert_eq!(heap.size, 1024);
    assert_eq!(binary.section_lma(".heap"), heap.address, "Heap is NOLOAD");

    let increment_data_xip = binary.symbol_value("increment_data").unwrap();
    assert!(
        0x6000_0000 < increment_data_xip && increment_data_xip < 0x7000_0000,
        "increment_data is not XiP"
    );
}

fn baseline_teensy4(binary: &ImxrtBinary, dcd_at_runtime: u32, stack_size: u64, heap_size: u64) {
    assert_eq!(
        Fcb {
            address: 0x6000_0000,
            size: 512
        },
        binary.fcb().unwrap()
    );
    assert_eq!(
        binary.flexram_config().unwrap(),
        0b11111111_101010101010101010101010
    );

    let ivt = binary.ivt().unwrap();
    assert_eq!(ivt.magic_header, 0x402000D1);
    assert_eq!(ivt.interrupt_vector_table, 0x6000_2000);
    assert_eq!(ivt.device_configuration_data, dcd_at_runtime);
    assert_eq!(
        ivt.boot_data as u64,
        binary.symbol_value("__ivt").unwrap() + 32
    );

    let stack = binary.section(".stack").unwrap();
    assert_eq!(
        Section {
            address: DTCM,
            size: stack_size
        },
        stack,
        "stack not at ORIGIN(DTCM), or not {stack_size} bytes large"
    );
    assert_eq!(binary.section_lma(".stack"), stack.address);

    let vector_table = binary.section(".vector_table").unwrap();
    let xip = binary.section(".xip").unwrap();
    assert_eq!(
        Section {
            address: stack.address + stack.size,
            size: 16 * 4 + 240 * 4
        },
        vector_table,
        "vector table not at expected VMA behind the stack"
    );
    assert!(
        vector_table.address % 1024 == 0,
        "vector table is not 1024-byte aligned"
    );
    assert_eq!(binary.section_lma(".vector_table"), 0x6000_2000);

    let text = binary.section(".text").unwrap();
    let expected_text_address = aligned(
        binary.section_lma(".vector_table") + vector_table.size + xip.size,
        4,
    );
    assert_eq!(text.address, expected_text_address, "text");
    assert_eq!(
        binary.section_lma(".text"),
        aligned(0x6000_2000 + vector_table.size + xip.size, 4),
        "text VMA expected behind vector table"
    );

    let rodata = binary.section(".rodata").unwrap();
    assert_eq!(
        rodata.address,
        vector_table.address + vector_table.size,
        "rodata LMA & VMA expected behind text"
    );
    assert!(binary.section_lma(".rodata") >= binary.section_lma(".text") + aligned(text.size, 4));

    let data = binary.section(".data").unwrap();
    assert_eq!(
        data.address,
        rodata.address + rodata.size,
        "data VMA in DTCM behind rodata"
    );
    assert_eq!(
        data.size, 4,
        "blink-rtic expected to have a single static mut u32"
    );
    assert_eq!(
        binary.section_lma(".data"),
        binary.section_lma(".rodata") + aligned(rodata.size, 4),
        "data LMA starts behind rodata"
    );

    let bss = binary.section(".bss").unwrap();
    assert_eq!(
        bss.address,
        data.address + aligned(data.size, 4),
        "bss in DTCM behind data"
    );
    assert_eq!(binary.section_lma(".bss"), bss.address, "bss is NOLOAD");

    let uninit = binary.section(".uninit").unwrap();
    assert_eq!(
        uninit.address,
        bss.address + aligned(bss.size, 4),
        "uninit in DTCM behind bss"
    );
    assert_eq!(
        binary.section_lma(".uninit"),
        uninit.address,
        "uninit is NOLOAD"
    );

    let heap = binary.section(".heap").unwrap();
    assert_eq!(
        Section {
            address: uninit.address + aligned(uninit.size, 4),
            size: heap_size
        },
        heap,
        "{heap_size} byte heap in DTCM behind uninit"
    );
    assert_eq!(binary.section_lma(".heap"), heap.address, "Heap is NOLOAD");

    let increment_data_xip = binary.symbol_value("increment_data").unwrap();
    assert!(
        0x6000_0000 < increment_data_xip && increment_data_xip < 0x7000_0000,
        "increment_data is not XiP"
    );
}

#[test]
#[ignore = "building an example can take time"]
fn teensy4() {
    let path = cargo_build("teensy4").expect("Unable to build example");
    let contents = fs::read(path).expect("Could not read ELF file");
    let elf = Elf::parse(&contents).expect("Could not parse ELF");

    let binary = ImxrtBinary::new(&elf, &contents);
    assert!(binary.symbol("DEVICE_CONFIGURATION_DATA").is_none());
    assert_eq!(
        binary.symbol_value("__dcd_start"),
        binary.symbol_value("__dcd_end")
    );
    assert_eq!(binary.symbol_value("__dcd"), Some(0));
    baseline_teensy4(&binary, 0, 8 * 1024, 1024);
}

#[test]
#[ignore = "building an example can take time"]
fn teensy4_fake_dcd() {
    let path = cargo_build("__dcd").expect("Unable to build example");
    let contents = fs::read(path).expect("Could not read ELF file");
    let elf = Elf::parse(&contents).expect("Could not parse ELF");

    let binary = ImxrtBinary::new(&elf, &contents);
    let dcd = binary.symbol("DEVICE_CONFIGURATION_DATA").unwrap();
    let dcd_start = binary.symbol_value("__dcd_start").unwrap();
    assert_eq!(
        Some(dcd_start + dcd.st_size),
        binary.symbol_value("__dcd_end"),
    );
    assert_eq!(
        binary.symbol_value("__dcd"),
        binary.symbol_value("__dcd_start"),
    );
    assert_eq!(dcd.st_size % 4, 0);
    baseline_teensy4(&binary, dcd_start as u32, 8 * 1024, 1024);
}

#[test]
#[ignore = "building an example can take time"]
fn teensy4_fake_dcd_missize_fail() {
    cargo_build("__dcd_missize").expect_err("Build should fail for missized DCD section.");
    eprintln!();
    eprintln!("NOTE: Linker failures above are intentional --- this test has succeeded.");
}

#[test]
#[ignore = "building an example can take time"]
fn teensy4_env_overrides() {
    let path = cargo_build_with_envs(
        "teensy4",
        &[
            ("BOARD_STACK", "4096"),
            ("THIS_WONT_BE_CONSIDERED", "12288"),
            ("BOARD_HEAP", "8192"),
        ],
    )
    .expect("Unable to build example");
    let contents = fs::read(path).expect("Could not read ELF file");
    let elf = Elf::parse(&contents).expect("Could not parse ELF");

    let binary = ImxrtBinary::new(&elf, &contents);
    baseline_teensy4(&binary, 0, 4 * 1024, 8 * 1024);
}

#[test]
#[ignore = "building an example can take time"]
fn teensy4_env_overrides_kib() {
    let path = cargo_build_with_envs("teensy4", &[("BOARD_STACK", "5K"), ("BOARD_HEAP", "9k")])
        .expect("Unable to build example");
    let contents = fs::read(path).expect("Could not read ELF file");
    let elf = Elf::parse(&contents).expect("Could not parse ELF");

    let binary = ImxrtBinary::new(&elf, &contents);
    baseline_teensy4(&binary, 0, 5 * 1024, 9 * 1024);
}

#[test]
#[should_panic]
#[ignore = "building an example can take time"]
fn teensy4_env_override_fail() {
    cargo_build_with_envs("teensy4", &[("BOARD_STACK", "1o24")])
        .expect("Build should fail since BOARD_STACK can't be parsed");
}

#[test]
#[ignore = "building an example can take time"]
fn imxrt1170evk_cm7() {
    let path = cargo_build("imxrt1170evk-cm7").expect("Unable to build example");
    let contents = fs::read(path).expect("Could not read ELF file");
    let elf = Elf::parse(&contents).expect("Could not parse ELF");

    let binary = ImxrtBinary::new(&elf, &contents);
    assert_eq!(
        binary.symbol_value("__dcd_start"),
        binary.symbol_value("__dcd_end")
    );
    assert_eq!(binary.symbol_value("__dcd"), Some(0));
    assert_eq!(
        Fcb {
            address: 0x3000_0400,
            size: 512
        },
        binary.fcb().unwrap()
    );
    assert_eq!(
        binary.flexram_config().unwrap(),
        0b1111111111111111_1010101010101010
    );

    let ivt = binary.ivt().unwrap();
    assert_eq!(ivt.magic_header, 0x402000D1);
    assert_eq!(ivt.interrupt_vector_table, 0x3000_2000);
    assert_eq!(ivt.device_configuration_data, 0);
    assert_eq!(
        ivt.boot_data as u64,
        binary.symbol_value("__ivt").unwrap() + 32
    );

    let stack = binary.section(".stack").unwrap();
    assert_eq!(
        Section {
            address: DTCM,
            size: 8 * 1024
        },
        stack,
        "stack not at ORIGIN(DTCM), or not 8 KiB large"
    );
    assert_eq!(binary.section_lma(".stack"), stack.address);

    let vector_table = binary.section(".vector_table").unwrap();
    assert_eq!(
        Section {
            address: stack.address + stack.size,
            size: 16 * 4 + 240 * 4
        },
        vector_table,
        "vector table not at expected VMA behind the stack"
    );
    assert!(
        vector_table.address % 1024 == 0,
        "vector table is not 1024-byte aligned"
    );
    assert_eq!(binary.section_lma(".vector_table"), 0x3000_2000);

    let xip = binary.section(".xip").unwrap();
    let text = binary.section(".text").unwrap();
    assert_eq!(text.address, ITCM, "text");
    assert_eq!(
        binary.section_lma(".text"),
        aligned(0x3000_2000 + vector_table.size + xip.size, 4),
        "text VMA expected behind vector table"
    );

    let rodata = binary.section(".rodata").unwrap();
    assert_eq!(
        rodata.address,
        vector_table.address + vector_table.size,
        "rodata moved to DTCM behind vector table"
    );
    assert!(
        binary.section_lma(".rodata") >= 0x3000_2000 + vector_table.size + aligned(text.size, 4),
    );

    let data = binary.section(".data").unwrap();
    assert_eq!(data.address, 0x2024_0000, "data VMA in OCRAM");
    assert_eq!(
        data.size, 4,
        "blink-rtic expected to have a single static mut u32"
    );
    assert_eq!(
        binary.section_lma(".data"),
        binary.section_lma(".rodata") + aligned(rodata.size, 4),
        "data LMA starts behind rodata"
    );

    let bss = binary.section(".bss").unwrap();
    assert_eq!(
        bss.address,
        data.address + aligned(data.size, 4),
        "bss in OCRAM behind data"
    );
    assert_eq!(binary.section_lma(".bss"), bss.address, "bss is NOLOAD");

    let uninit = binary.section(".uninit").unwrap();
    assert_eq!(
        uninit.address,
        bss.address + aligned(bss.size, 4),
        "uninit in OCRAM behind bss"
    );
    assert_eq!(
        binary.section_lma(".uninit"),
        uninit.address,
        "uninit is NOLOAD"
    );

    let heap = binary.section(".heap").unwrap();
    assert_eq!(
        Section {
            address: rodata.address + aligned(rodata.size, 4),
            size: 0,
        },
        heap,
        "0 byte heap in DTCM behind rodata table"
    );
    assert_eq!(binary.section_lma(".heap"), heap.address, "Heap is NOLOAD");

    let increment_data_xip = binary.symbol_value("increment_data").unwrap();
    assert!(
        0x3000_0000 < increment_data_xip && increment_data_xip < 0x4000_0000,
        "increment_data is not XiP"
    );
}

#[test]
#[ignore = "building an example can take time"]
fn imxrt1170evk_cm7_nonboot() {
    const IMAGE_OFFSET: u64 = 16 * 1024;
    let path = cargo_build_nonboot("imxrt1170evk-cm7").expect("Unable to build example");
    let contents = fs::read(path).expect("Could not read ELF file");
    let elf = Elf::parse(&contents).expect("Could not parse ELF");

    let binary = ImxrtBinary::new(&elf, &contents);
    assert_eq!(binary.symbol_value("__dcd_start"), None);
    assert_eq!(binary.symbol_value("__dcd_end"), None);
    assert_eq!(binary.symbol_value("__dcd"), None);
    assert!(binary.fcb().is_err());
    assert_eq!(
        binary.flexram_config().unwrap(),
        0b1111111111111111_1010101010101010
    );

    assert!(
        binary.ivt().is_err(),
        "Non boot image still contains boot IVT"
    );
    assert!(
        binary.section(".boot").is_err(),
        "Boot section is included in a non boot image"
    );

    let stack = binary.section(".stack").unwrap();
    assert_eq!(
        Section {
            address: DTCM,
            size: 8 * 1024
        },
        stack,
        "stack not at ORIGIN(DTCM), or not 8 KiB large"
    );
    assert_eq!(binary.section_lma(".stack"), stack.address);

    let vector_table = binary.section(".vector_table").unwrap();
    assert_eq!(
        Section {
            address: stack.address + stack.size,
            size: 16 * 4 + 240 * 4
        },
        vector_table,
        "vector table not at expected VMA behind the stack"
    );
    assert!(
        vector_table.address % 1024 == 0,
        "vector table is not 1024-byte aligned"
    );
    assert_eq!(
        binary.section_lma(".vector_table"),
        0x3000_0000 + IMAGE_OFFSET
    );

    let xip = binary.section(".xip").unwrap();
    // xip's lma==vma
    assert_eq!(
        xip.address,
        0x3000_0000 + IMAGE_OFFSET + vector_table.size,
        "xip"
    );
    assert_eq!(
        binary.section_lma(".xip"),
        0x3000_0000 + IMAGE_OFFSET + vector_table.size,
        "text VMA expected behind vector table"
    );

    let text = binary.section(".text").unwrap();
    assert_eq!(text.address, ITCM, "text");
    assert_eq!(
        binary.section_lma(".text"),
        0x3000_0000 + IMAGE_OFFSET + aligned(xip.size, 4) + vector_table.size,
        "text VMA expected behind vector table"
    );

    let rodata = binary.section(".rodata").unwrap();
    assert_eq!(
        rodata.address,
        vector_table.address + vector_table.size,
        "rodata moved to DTCM behind vector table"
    );
    assert!(
        binary.section_lma(".rodata") >= 0x3000_2000 + vector_table.size + aligned(text.size, 4),
    );

    let data = binary.section(".data").unwrap();
    assert_eq!(data.address, 0x2024_0000, "data VMA in OCRAM");
    assert_eq!(
        data.size, 4,
        "blink-rtic expected to have a single static mut u32"
    );
    assert_eq!(
        binary.section_lma(".data"),
        binary.section_lma(".rodata") + aligned(rodata.size, 4),
        "data LMA starts behind rodata"
    );

    let bss = binary.section(".bss").unwrap();
    assert_eq!(
        bss.address,
        data.address + aligned(data.size, 4),
        "bss in OCRAM behind data"
    );
    assert_eq!(binary.section_lma(".bss"), bss.address, "bss is NOLOAD");

    let uninit = binary.section(".uninit").unwrap();
    assert_eq!(
        uninit.address,
        bss.address + aligned(bss.size, 4),
        "uninit in OCRAM behind bss"
    );
    assert_eq!(
        binary.section_lma(".uninit"),
        uninit.address,
        "uninit is NOLOAD"
    );

    let heap = binary.section(".heap").unwrap();
    assert_eq!(
        Section {
            address: rodata.address + aligned(rodata.size, 4),
            size: 0,
        },
        heap,
        "0 byte heap in DTCM behind rodata table"
    );
    assert_eq!(binary.section_lma(".heap"), heap.address, "Heap is NOLOAD");
}
