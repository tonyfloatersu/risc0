// Copyright 2023 RISC Zero, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! The execution phase is implemented by this module.
//!
//! The result of the execution phase is a [Session]. Each [Session] contains
//! one or more [Segment]s, each of which contains an execution trace of the
//! specified program.

#![allow(missing_docs)]
mod env;
pub(crate) mod io;
// mod monitor;
mod ecall;
mod memory;
use std::collections::BTreeSet;
#[cfg(feature = "profiler")]
pub(crate) mod profiler;
mod rv32im;
#[cfg(test)]
mod tests;

use std::{cell::RefCell, fmt::Debug, io::Write, mem::take, rc::Rc};

use anyhow::{bail, Result};
use ecall::{exec_ecall, PendingECall};
use memory::{image_to_ram, ram_to_image, Dir, PageTable, CYCLES_PER_FULL_PAGE};
use risc0_zkp::{core::log2_ceil, MAX_CYCLES_PO2, MIN_CYCLES_PO2, ZK_CYCLES};
use risc0_zkvm_platform::{
    fileno,
    memory::{MEM_SIZE, SYSTEM},
    PAGE_SIZE, WORD_SIZE,
};
use rv32im::{exec_rv32im, MachineState, PendingInst};
use serde::{Deserialize, Serialize};

pub use self::env::{ExecutorEnv, ExecutorEnvBuilder};
use crate::{
    exec::io::SyscallContext, receipt::ExitCode, Loader, MemoryImage, Program, Segment, SegmentRef,
    Session, SimpleSegmentRef,
};

#[derive(Debug)]
enum PendingOp {
    PendingInst(PendingInst),
    PendingECall(PendingECall),
}

/// The number of cycles required to compress a SHA-256 block.
const SHA_CYCLES: usize = 72;

/// The Executor provides an implementation for the execution phase.
///
/// The proving phase uses an execution trace generated by the Executor.
pub struct Executor<'a> {
    env: ExecutorEnv<'a>,

    /// Current segment being executed
    cur_segment: Segment,
    /// Cycles allowed for this segment; same as (1 << cur_segment.po2).
    /// Exceeding this will either resize the segment or start a new
    /// segment, depending on the value of env.get_segment_limit().
    segment_limit: usize,

    /// Current cycle number in this segment; count includes init_cycles but not
    /// write_cycles.
    segment_cycle: usize,

    /// Number of cycles needed to page in all necessary pages known about so
    /// far
    read_cycles: usize,
    /// Number of cycles needed to flush dirty pages known about so far
    write_cycles: usize,

    /// Number of cycles in fixed parts of a segment including setup, reset,
    /// ZK_QUERIES, etc.
    init_cycles: usize,
    fini_cycles: usize,

    /// Cycles used in previous segments.
    prev_segment_cycles: usize,

    /// Current program counter and registers
    pc: u32,
    regs: [u32; 32],
    ram: Vec<u8>,
    page_table: PageTable,

    /// Operation that's been executed but not applied to the current state.
    pending_op: Option<PendingOp>,

    /// Accumulated segments
    segments: Vec<Box<dyn SegmentRef>>,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct SyscallRecord {
    pub to_guest: Vec<u32>,
    pub regs: (u32, u32),
}

// Capture the journal output in a buffer that we can access afterwards.
#[derive(Clone, Default)]
struct Journal {
    buf: Rc<RefCell<Vec<u8>>>,
}

impl Write for Journal {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.buf.borrow_mut().write(bytes)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.buf.borrow_mut().flush()
    }
}

impl<'a> MachineState for Executor<'a> {
    fn load_ram(&self, addr: u32) -> u32 {
        u32::from_le_bytes(
            self.ram[addr as usize..addr as usize + WORD_SIZE]
                .try_into()
                .unwrap(),
        )
    }
    fn load_reg(&self, reg_idx: usize) -> u32 {
        self.regs[reg_idx]
    }
}

impl<'a> SyscallContext for Executor<'a> {
    fn get_cycle(&self) -> usize {
        self.prev_segment_cycles + self.segment_cycle
    }

    fn load_register(&self, reg: usize) -> u32 {
        self.regs[reg]
    }

    fn load_u32(&self, addr: u32) -> u32 {
        u32::from_le_bytes(
            self.ram[addr as usize..addr as usize + WORD_SIZE]
                .try_into()
                .unwrap(),
        )
    }

    fn load_u8(&self, addr: u32) -> u8 {
        self.ram[addr as usize]
    }
}

impl<'a> Executor<'a> {
    fn segment_cycles_remaining(&self) -> usize {
        self.segment_limit
            - self.segment_cycle
            - self.read_cycles
            - self.write_cycles
            - self.fini_cycles
            - 1
    }

    /// Construct a new [Executor] from a [MemoryImage] and entry point.
    ///
    /// Before a guest program is proven, the [Executor] is responsible for
    /// deciding where a zkVM program should be split into [Segment]s and what
    /// work will be done in each segment. This is the execution phase:
    /// the guest program is executed to determine how its proof should be
    /// divided into subparts.
    pub fn new(env: ExecutorEnv<'a>, image: MemoryImage, pc: u32) -> Self {
        let page_table = PageTable::new(image.info.clone(), MEM_SIZE);
        let cur_segment = Segment::new(
            image,
            Default::default(),
            Default::default(),
            Vec::new(),
            ExitCode::SystemSplit,
            None,
            MIN_CYCLES_PO2,
            0,
            0,
        );
        let loader = Loader::new();
        let mut exec = Self {
            env,
            segment_limit: 0, // Filled in by start_segment.
            init_cycles: loader.init_cycles(),
            fini_cycles: loader.fini_cycles()
                + SHA_CYCLES        // Final journal digest.
                + ZK_CYCLES, // Cycles reserved for ZK elements

            cur_segment,
            page_table,

            segment_cycle: 0,
            read_cycles: 0,
            write_cycles: 0,
            prev_segment_cycles: 0,

            pc,
            regs: Default::default(),
            ram: Vec::new(),

            pending_op: None,
            segments: Vec::new(),
        };
        exec.ram.resize(MEM_SIZE, 0);

        image_to_ram(&exec.cur_segment.pre_image, &mut exec.ram);
        exec.image_to_regs();
        exec.start_segment();
        exec
    }

    fn regs_to_image(&mut self) {
        self.cur_segment
            .pre_image
            .store_region_in_page(SYSTEM.start() as u32, bytemuck::cast_slice(&self.regs));
        self.cur_segment.pre_image.pc = self.pc;
    }

    fn image_to_regs(&mut self) {
        self.cur_segment.pre_image.load_region_in_page(
            SYSTEM.start() as u32,
            bytemuck::cast_slice_mut(&mut self.regs),
        );
    }

    fn handle_out_of_cycles(&mut self) -> Result<Option<ExitCode>> {
        if self.segment_limit < self.env.get_segment_limit() {
            // Expand in place
            self.cur_segment.po2 += 1;
            assert!(self.cur_segment.po2 < MAX_CYCLES_PO2);
            self.segment_limit = 1 << self.cur_segment.po2;
            Ok(None)
        } else {
            Ok(Some(ExitCode::SystemSplit))
        }
    }

    fn start_segment(&mut self) {
        assert_eq!(self.segment_cycle, 0);
        self.segment_cycle = self.init_cycles;
        let (read_cycles, write_cycles) = self.page_table.mark_root();
        self.read_cycles += read_cycles;
        self.write_cycles = write_cycles;
        self.cur_segment.po2 =
            log2_ceil(self.segment_cycle + self.read_cycles + self.write_cycles + self.fini_cycles);
        self.segment_limit = 1 << self.cur_segment.po2;
        self.cur_segment.insn_cycles = 0;
        log::debug!(
            "Starting new segment with cycles: init {} read {} write {} fini {} limit {}",
            self.init_cycles,
            self.read_cycles,
            self.write_cycles,
            self.fini_cycles,
            self.segment_limit
        );
    }

    fn split<F>(&mut self, exit_code: ExitCode, callback: &mut F) -> Result<()>
    where
        F: FnMut(Segment) -> Result<Box<dyn SegmentRef>>,
    {
        let read_cycles = take(&mut self.read_cycles);
        let write_cycles = match exit_code {
            ExitCode::Paused(_) => take(&mut self.write_cycles),
            ExitCode::SystemSplit => take(&mut self.write_cycles),
            ExitCode::Halted(_) => {
                self.write_cycles = 0;
                0
            }
            ExitCode::SessionLimit => bail!("Session limit exceeded"),
        };

        log::debug!("{:?}: Finishing segment with cur = {}, {read_cycles} read, {write_cycles} write = {} total, pc = {:#08x}", exit_code, self.segment_cycle,
        self.segment_cycle + read_cycles + write_cycles, self.pc);

        let old_segment_cycle = take(&mut self.segment_cycle);
        self.prev_segment_cycles +=
            old_segment_cycle + read_cycles + write_cycles + self.fini_cycles;

        let syscalls = take(&mut self.cur_segment.syscalls);
        let mut old_segment = self.cur_segment.clone();
        self.cur_segment.index += 1;

        let faults = self.page_table.calc_page_faults();
        self.page_table.clear();

        ram_to_image(
            &mut self.cur_segment.pre_image,
            &self.ram,
            faults.writes.iter().cloned(),
        );
        self.regs_to_image();

        self.cur_segment.pre_image.hash_pages();
        old_segment.post_image_id = self.cur_segment.pre_image.compute_id();
        old_segment.exit_code = exit_code;
        log::trace!("Faults: {faults:?}");
        old_segment.syscalls = syscalls;
        old_segment.faults = faults;
        old_segment.split_insn = Some(old_segment.insn_cycles as u32);

        #[cfg(feature = "cycle_count_debug")]
        {
            // Since reads are all done at the beginning, apply this offset to all our cycle
            // counts.
            let mut cycle_pc: alloc::collections::VecDeque<(u32, u32)> =
                take(&mut old_segment.cycle_pc)
                    .into_iter()
                    .map(|(cycle, pc)| (cycle + read_cycles as u32, pc))
                    .collect();

            if let ExitCode::Paused(_) = exit_code {
                // Paused flushes writes prior to the ecall through
                // the magic of get_major.
                cycle_pc.back_mut().unwrap().0 += write_cycles as u32;
            };
            // Make sure we correctly calculated write_cycles
            cycle_pc.push_back((
                (old_segment_cycle + read_cycles + write_cycles + 1) as u32,
                u32::MAX,
            ));
            old_segment.cycle_pc = cycle_pc;
            self.cur_segment.cycle_pc.clear();
        }

        self.start_segment();
        self.segments.push(callback(old_segment)?);

        Ok(())
    }

    /// Construct a new [Executor] from the ELF binary of the guest program you
    /// want to run and an [ExecutorEnv] containing relevant environmental
    /// configuration details.
    /// # Example
    /// ```
    /// use risc0_zkvm::{serde::to_vec, Executor, ExecutorEnv, Session};
    /// use risc0_zkvm_methods::{BENCH_ELF, bench::{BenchmarkSpec, SpecWithIters}};
    ///
    /// let spec = SpecWithIters(BenchmarkSpec::SimpleLoop, 1);
    /// let env = ExecutorEnv::builder()
    ///     .add_input(&to_vec(&spec).unwrap())
    ///     .build();
    /// let mut exec = Executor::from_elf(env, BENCH_ELF).unwrap();
    /// ```
    pub fn from_elf(env: ExecutorEnv<'a>, elf: &[u8]) -> Result<Self> {
        let program = Program::load_elf(&elf, MEM_SIZE as u32)?;
        let image = MemoryImage::new(&program, PAGE_SIZE as u32)?;
        Ok(Self::new(env, image, program.entry))
    }

    /// Run the executor until [ExitCode::Paused] or [ExitCode::Halted] is
    /// reached, producing a [Session] as a result.
    /// # Example
    /// ```
    /// use risc0_zkvm::{serde::to_vec, Executor, ExecutorEnv, Session};
    /// use risc0_zkvm_methods::{BENCH_ELF, bench::{BenchmarkSpec, SpecWithIters}};
    ///
    /// let spec = SpecWithIters(BenchmarkSpec::SimpleLoop, 1);
    /// let env = ExecutorEnv::builder()
    ///    .add_input(&to_vec(&spec).unwrap())
    ///    .build();
    /// let mut exec = Executor::from_elf(env, BENCH_ELF).unwrap();
    /// let session = exec.run().unwrap();
    /// ```
    pub fn run(&mut self) -> Result<Session> {
        self.run_with_callback(|segment| Ok(Box::new(SimpleSegmentRef::new(segment))))
    }

    pub fn trace(&mut self, event: TraceEvent) -> Result<()> {
        #[cfg(feature = "cycle_count_debug")]
        if let TraceEvent::InstructionStart { cycle, pc } = event {
            self.cur_segment.cycle_pc.push_back((cycle, pc));
        }

        if let Some(cb) = &self.env.trace_callback {
            cb.borrow_mut()(event)?
        }
        Ok(())
    }

    /// Run the executor until [ExitCode::Paused] or [ExitCode::Halted] is
    /// reached, producing a [Session] as a result.
    pub fn run_with_callback<F>(&mut self, mut callback: F) -> Result<Session>
    where
        F: FnMut(Segment) -> Result<Box<dyn SegmentRef>>,
    {
        let journal = Journal::default();
        self.env
            .io
            .borrow_mut()
            .with_write_fd(fileno::JOURNAL, journal.clone());

        let mut exit_code;
        loop {
            exit_code = self.step()?;
            if let Some(exit_code) = exit_code {
                log::debug!("Exiting at cycle {}: {:?}", self.segment_cycle, exit_code);
                self.split(exit_code, &mut callback)?;
                match exit_code {
                    ExitCode::SystemSplit => {
                        // Keep going generating more segments
                    }
                    _ => break,
                }
            }

            if self.prev_segment_cycles + self.segment_cycle > self.env.get_session_limit() {
                bail!("Session limit exceeded")
            }
        }
        Ok(Session::new(
            take(&mut self.segments),
            journal.buf.take(),
            exit_code.unwrap(),
        ))
    }

    pub fn step(&mut self) -> Result<Option<ExitCode>> {
        log::trace!(
            "Step at pc={:#08x}, pending_op = {:?}, cycles = {} + {} read + {} write + {} fini, limit = {}",
            self.pc,
            &self.pending_op,
            self.segment_cycle,
            self.read_cycles,
            self.write_cycles,
            self.fini_cycles,
            self.segment_limit
        );
        match self.pending_op.take() {
            Some(op) => self.apply(op),
            None => {
                let op = PendingOp::PendingInst(exec_rv32im(self.pc, self)?);
                self.apply(op)
            }
        }
    }

    fn calc_ecall_pages(&self, ecall: &PendingECall) -> (BTreeSet<u32>, BTreeSet<u32>) {
        match ecall {
            PendingECall {
                ram_writes,
                page_loads,
                ..
            } => {
                let mut page_loads = page_loads.clone();
                let page_stores: BTreeSet<u32> = ram_writes
                    .iter()
                    .map(|(addr, _val)| addr / PAGE_SIZE as u32)
                    .collect();
                page_loads.extend(page_stores.iter());

                let page_loads_needed: BTreeSet<u32> = page_loads
                    .iter()
                    .flat_map(|page_idx| self.page_table.pages_needed(*page_idx, Dir::Load))
                    .collect();
                let page_stores_needed: BTreeSet<u32> = page_stores
                    .iter()
                    .flat_map(|page_idx| self.page_table.pages_needed(*page_idx, Dir::Store))
                    .collect();

                (page_loads_needed, page_stores_needed)
            }
        }
    }

    fn apply(&mut self, op: PendingOp) -> Result<Option<ExitCode>> {
        assert!(
            self.pending_op.is_none(),
            "Apply needs to be able to reschedule a pending op"
        );
        let mut cycles_needed = match &op {
            PendingOp::PendingInst(PendingInst::ECall) => {
                // Execute the ecall, and try to apply it next loop.
                let ecall = exec_ecall(self, &self.env)?;
                self.pending_op = Some(PendingOp::PendingECall(ecall));
                return Ok(None);
            }
            PendingOp::PendingInst(PendingInst::MemoryLoad { addr, .. }) => {
                self.page_table
                    .cycles_needed(addr / PAGE_SIZE as u32, Dir::Load)
                    + 1
            }
            PendingOp::PendingInst(PendingInst::MemoryStore { addr, .. }) => {
                self.page_table
                    .cycles_needed(addr / PAGE_SIZE as u32, Dir::Load)
                    + self
                        .page_table
                        .cycles_needed(addr / PAGE_SIZE as u32, Dir::Store)
                    + 1
            }
            PendingOp::PendingInst(PendingInst::RegisterStore { cycles, .. }) => *cycles,
            PendingOp::PendingECall(ecall @ PendingECall { cycles, .. }) => {
                let (page_loads, page_stores) = self.calc_ecall_pages(ecall);
                page_loads.len() * CYCLES_PER_FULL_PAGE
                    + page_stores.len() * CYCLES_PER_FULL_PAGE
                    + cycles
            }
        };

        cycles_needed += self
            .page_table
            .cycles_needed(self.pc / PAGE_SIZE as u32, Dir::Load);

        if cycles_needed >= self.segment_cycles_remaining() {
            return self.handle_out_of_cycles();
        }

        self.trace(TraceEvent::InstructionStart {
            cycle: self.segment_cycle as u32,
            pc: self.pc,
        })?;

        self.read_cycles += self.page_table.mark_addr(self.pc, Dir::Load);
        self.cur_segment.insn_cycles += 1;

        match op {
            PendingOp::PendingInst(PendingInst::ECall) => {
                panic!("Encountered un-executed ECall PendingOp in second apply phase")
            }
            PendingOp::PendingInst(PendingInst::MemoryLoad { addr, val, reg }) => {
                self.segment_cycle += 1;
                self.read_cycles += self.page_table.mark_addr(addr, Dir::Load);
                self.regs[reg] = val;
                self.pc += WORD_SIZE as u32;
                self.trace(TraceEvent::RegisterSet { reg, value: val })?;
                Ok(None)
            }
            PendingOp::PendingInst(PendingInst::MemoryStore { addr, val }) => {
                let write_cycles = self.page_table.mark_addr(addr, Dir::Store);
                if write_cycles > 0 {
                    self.write_cycles += write_cycles;
                    self.read_cycles += self.page_table.mark_addr(addr, Dir::Load);
                }
                self.segment_cycle += 1;
                self.ram[addr as usize..addr as usize + WORD_SIZE]
                    .clone_from_slice(&val.to_le_bytes());
                self.pc += WORD_SIZE as u32;
                self.trace(TraceEvent::MemorySet { addr, value: val })?;
                Ok(None)
            }
            PendingOp::PendingInst(PendingInst::RegisterStore {
                reg,
                val,
                new_pc,
                cycles,
            }) => {
                self.segment_cycle += cycles;
                if reg != 0 {
                    self.regs[reg] = val;
                }
                self.pc = new_pc;
                self.trace(TraceEvent::RegisterSet { reg, value: val })?;
                Ok(None)
            }
            PendingOp::PendingECall(ecall) => {
                let (page_loads, page_stores) = self.calc_ecall_pages(&ecall);

                let PendingECall {
                    ram_writes,
                    reg_writes,
                    syscall,
                    exit_code,
                    cycles,
                    ..
                } = ecall;

                self.segment_cycle += cycles;
                for page_idx in page_loads {
                    self.read_cycles += self.page_table.mark_page(page_idx, Dir::Load);
                }
                for page_idx in page_stores {
                    self.write_cycles += self.page_table.mark_page(page_idx, Dir::Store);
                }

                for (reg, val) in reg_writes.iter() {
                    self.regs[*reg] = *val;
                    self.trace(TraceEvent::RegisterSet {
                        reg: *reg,
                        value: *val,
                    })?;
                }
                for (addr, val) in ram_writes.iter() {
                    self.ram[*addr as usize..*addr as usize + WORD_SIZE]
                        .clone_from_slice(&val.to_le_bytes());
                    self.trace(TraceEvent::MemorySet {
                        addr: *addr,
                        value: *val,
                    })?;
                }
                if let Some(syscall) = syscall {
                    log::trace!("Pushing ecall, len={:?}", self.cur_segment.syscalls.len());
                    self.cur_segment.syscalls.push(syscall);
                }
                self.pc += WORD_SIZE as u32;
                Ok(exit_code)
            }
        }
    }
}

/// An event traced from the running VM.
#[derive(Clone, Eq, Ord, PartialEq, PartialOrd)]
pub enum TraceEvent {
    /// An instruction has started at the given program counter
    InstructionStart {
        /// Cycle number since startup
        cycle: u32,
        /// Program counter of the instruction being executed
        pc: u32,
    },

    /// A register has been set
    RegisterSet {
        /// Register ID (0-16)
        reg: usize,
        /// New value in the register
        value: u32,
    },

    /// A memory location has been written
    MemorySet {
        /// Address of word that's been written
        addr: u32,
        /// Value of word that's been written
        value: u32,
    },
}

impl Debug for TraceEvent {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::InstructionStart { cycle, pc } => {
                write!(f, "InstructionStart({cycle}, 0x{pc:08X})")
            }
            Self::RegisterSet { reg, value } => write!(f, "RegisterSet({reg}, 0x{value:08X})"),
            Self::MemorySet { addr, value } => write!(f, "MemorySet(0x{addr:08X}, 0x{value:08X})"),
        }
    }
}
