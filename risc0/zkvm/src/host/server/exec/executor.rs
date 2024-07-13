// Copyright 2024 RISC Zero, Inc.
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

use std::{cell::RefCell, io::Write, mem, rc::Rc, sync::Arc, time::Instant};

use anyhow::{Context as _, Result};
use risc0_binfmt::{MemoryImage, Program};
use risc0_circuit_rv32im::prove::emu::{
    addr::ByteAddr,
    exec::{
        Executor, Syscall as NewSyscall, SyscallContext as NewSyscallContext,
        DEFAULT_SEGMENT_LIMIT_PO2,
    },
};
use risc0_zkp::core::digest::Digest;
use risc0_zkvm_platform::{fileno, memory::GUEST_MAX_MEM, PAGE_SIZE};
use tempfile::tempdir;

use crate::{
    host::client::env::SegmentPath, Assumptions, ExecutorEnv, FileSegmentRef, Output, Segment,
    SegmentRef, Session,
};

use super::{
    profiler::Profiler,
    syscall::{SyscallContext, SyscallTable},
};

// The Executor provides an implementation for the execution phase.
///
/// The proving phase uses an execution trace generated by the Executor.
pub struct ExecutorImpl<'a> {
    env: ExecutorEnv<'a>,
    image: MemoryImage,
    pub(crate) syscall_table: SyscallTable<'a>,
    profiler: Option<Rc<RefCell<Profiler>>>,
}

impl<'a> ExecutorImpl<'a> {
    /// Construct a new [ExecutorImpl] from a [MemoryImage] and entry point.
    ///
    /// Before a guest program is proven, the [ExecutorImpl] is responsible for
    /// deciding where a zkVM program should be split into [Segment]s and what
    /// work will be done in each segment. This is the execution phase:
    /// the guest program is executed to determine how its proof should be
    /// divided into subparts.
    pub fn new(env: ExecutorEnv<'a>, image: MemoryImage) -> Result<Self> {
        Self::with_details(env, image, None)
    }

    /// Construct a new [ExecutorImpl] from the ELF binary of the guest program
    /// you want to run and an [ExecutorEnv] containing relevant
    /// environmental configuration details.
    ///
    /// # Example
    /// ```
    /// use risc0_zkvm::{ExecutorImpl, ExecutorEnv, Session};
    /// use risc0_zkvm_methods::{BENCH_ELF, bench::{BenchmarkSpec, SpecWithIters}};
    ///
    /// let env = ExecutorEnv::builder()
    ///     .write(&SpecWithIters(BenchmarkSpec::SimpleLoop, 1))
    ///     .unwrap()
    ///     .build()
    ///     .unwrap();
    /// let mut exec = ExecutorImpl::from_elf(env, BENCH_ELF).unwrap();
    /// ```
    pub fn from_elf(mut env: ExecutorEnv<'a>, elf: &[u8]) -> Result<Self> {
        let program = Program::load_elf(elf, GUEST_MAX_MEM as u32)?;
        let image = MemoryImage::new(&program, PAGE_SIZE as u32)?;

        let profiler = if env.pprof_out.is_some() {
            let profiler = Rc::new(RefCell::new(Profiler::new(elf, None)?));
            env.trace.push(profiler.clone());
            Some(profiler)
        } else {
            None
        };

        Self::with_details(env, image, profiler)
    }

    fn with_details(
        env: ExecutorEnv<'a>,
        image: MemoryImage,
        profiler: Option<Rc<RefCell<Profiler>>>,
    ) -> Result<Self> {
        let syscall_table = SyscallTable::from_env(&env);
        Ok(Self {
            env,
            image,
            syscall_table,
            profiler,
        })
    }

    /// This will run the executor to get a [Session] which contain the results
    /// of the execution.
    pub fn run(&mut self) -> Result<Session> {
        if self.env.segment_path.is_none() {
            self.env.segment_path = Some(SegmentPath::TempDir(Arc::new(tempdir()?)));
        }

        let path = self.env.segment_path.clone().unwrap();
        self.run_with_callback(|segment| Ok(Box::new(FileSegmentRef::new(&segment, &path)?)))
    }

    /// Run the executor until [crate::ExitCode::Halted] or
    /// [crate::ExitCode::Paused] is reached, producing a [Session] as a result.
    pub fn run_with_callback<F>(&mut self, mut callback: F) -> Result<Session>
    where
        F: FnMut(Segment) -> Result<Box<dyn SegmentRef>>,
    {
        nvtx::range_push!("execute");

        let journal = Journal::default();
        self.env
            .posix_io
            .borrow_mut()
            .with_write_fd(fileno::JOURNAL, journal.clone());

        let segment_limit_po2 = self
            .env
            .segment_limit_po2
            .unwrap_or(DEFAULT_SEGMENT_LIMIT_PO2 as u32) as usize;

        let mut refs = Vec::new();
        let mut exec = Executor::new(
            self.image.clone(),
            self,
            self.env.input_digest,
            self.env.trace.clone(),
        );

        let start_time = Instant::now();
        let result = exec.run(segment_limit_po2, self.env.session_limit, |inner| {
            let output = inner
                .exit_code
                .expects_output()
                .then(|| -> Option<Result<_>> {
                    inner
                        .output_digest
                        .and_then(|digest| {
                            (digest != Digest::ZERO).then(|| journal.buf.borrow().clone())
                        })
                        .map(|journal| {
                            Ok(Output {
                                journal: journal.into(),
                                assumptions: Assumptions(
                                    self.env
                                        .assumptions
                                        .borrow()
                                        .accessed
                                        .iter()
                                        .map(|(a, _)| a.clone().into())
                                        .collect::<Vec<_>>(),
                                )
                                .into(),
                            })
                        })
                })
                .flatten()
                .transpose()?;

            let segment = Segment {
                index: inner.index as u32,
                inner,
                output,
            };
            let segment_ref = callback(segment)?;
            refs.push(segment_ref);
            Ok(())
        })?;
        let elapsed = start_time.elapsed();

        // Set the session_journal to the committed data iff the guest set a non-zero output.
        let session_journal = result
            .output_digest
            .and_then(|digest| (digest != Digest::ZERO).then(|| journal.buf.take()));
        if !result.exit_code.expects_output() && session_journal.is_some() {
            tracing::debug!(
                "dropping non-empty journal due to exit code {:?}: 0x{}",
                result.exit_code,
                hex::encode(journal.buf.borrow().as_slice())
            );
        };

        // Take (clear out) the list of accessed assumptions.
        // Leave the assumptions cache so it can be used if execution is resumed from pause.
        let assumptions = mem::take(&mut self.env.assumptions.borrow_mut().accessed);

        if let Some(profiler) = self.profiler.take() {
            let report = profiler.borrow_mut().finalize_to_vec();
            std::fs::write(self.env.pprof_out.as_ref().unwrap(), report)?;
        }

        self.image = result.post_image.clone();

        let session = Session::new(
            refs,
            self.env.input_digest.unwrap_or_default(),
            session_journal,
            result.exit_code,
            result.post_image,
            assumptions,
            result.user_cycles,
            result.total_cycles,
            result.pre_state,
            result.post_state,
        );

        tracing::info_span!("executor").in_scope(|| {
            tracing::info!("execution time: {elapsed:?}");
            session.log();
        });

        nvtx::range_pop!();
        Ok(session)
    }
}

struct ContextAdapter<'a> {
    ctx: &'a mut dyn NewSyscallContext,
}

impl<'a> SyscallContext for ContextAdapter<'a> {
    fn get_pc(&self) -> u32 {
        self.ctx.get_pc()
    }

    fn get_cycle(&self) -> u64 {
        self.ctx.get_cycle()
    }

    fn load_register(&mut self, idx: usize) -> u32 {
        self.ctx.peek_register(idx).unwrap()
    }

    fn load_u8(&mut self, addr: u32) -> Result<u8> {
        self.ctx.peek_u8(ByteAddr(addr))
    }

    fn load_region(&mut self, addr: u32, size: u32) -> Result<Vec<u8>> {
        self.ctx.peek_region(ByteAddr(addr), size)
    }

    fn load_page(&mut self, page_idx: u32) -> Result<Vec<u8>> {
        self.ctx.peek_page(page_idx)
    }

    fn load_u32(&mut self, addr: u32) -> Result<u32> {
        self.ctx.peek_u32(ByteAddr(addr))
    }
}

impl<'a> NewSyscall for ExecutorImpl<'a> {
    fn syscall(
        &self,
        syscall: &str,
        ctx: &mut dyn NewSyscallContext,
        into_guest: &mut [u32],
    ) -> Result<(u32, u32)> {
        let mut ctx = ContextAdapter { ctx };
        self.syscall_table
            .get_syscall(syscall)
            .context(format!("Unknown syscall: {syscall:?}"))?
            .borrow_mut()
            .syscall(syscall, &mut ctx, into_guest)
    }
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
