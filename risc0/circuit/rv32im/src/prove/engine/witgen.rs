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

use anyhow::Result;
use rand::thread_rng;
use rayon::prelude::*;
use risc0_zkp::{
    adapter::TapsProvider,
    field::{baby_bear::BabyBearElem, Elem as _},
    hal::cpu::CpuBuffer,
    ZK_CYCLES,
};

use super::machine::MachineContext;
use crate::{
    prove::{emu::preflight::PreflightTrace, engine::loader::Loader},
    CIRCUIT,
};

pub struct WitnessGenerator {
    pub steps: usize,
    pub ctrl: CpuBuffer<BabyBearElem>,
    pub data: CpuBuffer<BabyBearElem>,
    pub io: CpuBuffer<BabyBearElem>,
}

impl WitnessGenerator {
    pub fn new(po2: usize, io: &[BabyBearElem]) -> Self {
        let steps = 1 << po2;

        nvtx::range_push!("alloc(ctrl)");
        let ctrl = CpuBuffer::from_fn("ctrl", steps * CIRCUIT.ctrl_size(), |_| BabyBearElem::ZERO);
        nvtx::range_pop!();

        nvtx::range_push!("alloc(data)");
        let data = CpuBuffer::from_fn("data", steps * CIRCUIT.data_size(), |_| {
            BabyBearElem::INVALID
        });
        nvtx::range_pop!();

        nvtx::range_push!("alloc(io)");
        let io = CpuBuffer::from(Vec::from(io));
        nvtx::range_pop!();

        Self {
            steps,
            ctrl,
            data,
            io,
        }
    }

    #[tracing::instrument(skip_all)]
    pub fn execute(&mut self, trace: PreflightTrace) -> Result<()> {
        nvtx::range_push!("witgen");

        let mut machine = MachineContext::new(self.steps, trace);
        self.compute_execute(&mut machine)?;
        self.compute_verify_ram(&mut machine)?;
        self.compute_verify_bytes(&mut machine)?;
        let mut rng = thread_rng();

        {
            nvtx::range_push!("noise");
            let ctrl = self.ctrl.as_slice_sync();
            let data = self.data.as_slice_sync();

            for i in 0..ZK_CYCLES {
                let cycle = self.steps - ZK_CYCLES + i;
                // Set ctrl to all zeros for the ZK_CYCLES
                for j in 0..CIRCUIT.ctrl_size() {
                    ctrl.set(j * self.steps + cycle, BabyBearElem::ZERO);
                }
                // Set data to random for the ZK_CYCLES
                for j in 0..CIRCUIT.data_size() {
                    data.set(j * self.steps + cycle, BabyBearElem::random(&mut rng));
                }
            }
            nvtx::range_pop!();
        }

        // Zero out 'invalid' entries in data and output.
        nvtx::range_push!("zeroize");
        self.data
            .as_slice_mut()
            .par_iter_mut()
            .chain(self.io.as_slice_mut().par_iter_mut())
            .for_each(|value| *value = value.valid_or_zero());
        nvtx::range_pop!();

        nvtx::range_pop!();
        Ok(())
    }

    #[cfg(test)]
    pub fn test_step_execute(&mut self, trace: PreflightTrace, is_fwd: bool) -> Vec<BabyBearElem> {
        let machine = MachineContext::new(self.steps, trace);
        let mut loader = Loader::new(self.steps, &mut self.ctrl);
        let last_cycle = loader.load();

        if !is_fwd {
            for cycle in 0..last_cycle {
                machine.inject_exec_backs(self.steps, cycle, &self.data.as_slice_sync());
            }
        }

        {
            let args = &[
                self.ctrl.as_slice_sync(),
                self.io.as_slice_sync(),
                self.data.as_slice_sync(),
            ];

            if is_fwd {
                for cycle in 0..last_cycle {
                    machine.step_exec(self.steps, cycle, args).unwrap();
                }
            } else {
                machine.rev_step_exec(self.steps, last_cycle, args).unwrap();
            }
        }

        self.data
            .as_slice_mut()
            .par_iter_mut()
            .chain(self.io.as_slice_mut().par_iter_mut())
            .for_each(|value| *value = value.valid_or_zero());

        self.data.as_slice().to_vec()
    }

    #[tracing::instrument(skip_all)]
    fn compute_execute(&mut self, machine: &mut MachineContext) -> Result<()> {
        nvtx::range_push!("compute_execute");

        tracing::debug!("load");
        nvtx::range_push!("load");
        let mut loader = Loader::new(self.steps, &mut self.ctrl);
        let last_cycle = loader.load();
        nvtx::range_pop!();

        #[cfg(not(feature = "seq"))]
        {
            nvtx::range_push!("inject_exec_backs");
            tracing::debug!("inject_exec_backs");
            for cycle in 0..last_cycle {
                machine.inject_exec_backs(self.steps, cycle, &self.data.as_slice_sync());
            }
            nvtx::range_pop!();
        }

        {
            nvtx::range_push!("step_exec");
            tracing::debug!("step_exec");
            let args = &[
                self.ctrl.as_slice_sync(),
                self.io.as_slice_sync(),
                self.data.as_slice_sync(),
            ];

            #[cfg(not(feature = "seq"))]
            machine.par_step_exec(self.steps, last_cycle, args)?;

            #[cfg(feature = "seq")]
            for cycle in 0..last_cycle {
                machine.step_exec(self.steps, cycle, args)?;
            }
            nvtx::range_pop!();
        }

        nvtx::range_pop!();
        Ok(())
    }

    #[tracing::instrument(skip_all)]
    fn compute_verify_ram(&mut self, machine: &mut MachineContext) -> Result<()> {
        nvtx::range_push!("verify_ram");
        tracing::debug!("verify_ram");

        let last_cycle = self.steps - ZK_CYCLES;

        machine.sort("ram")?;

        #[cfg(not(feature = "seq"))]
        {
            nvtx::range_push!("inject_verify_mem_backs");
            tracing::debug!("inject_verify_mem_backs");
            for cycle in 0..last_cycle {
                machine.inject_verify_mem_backs(self.steps, cycle, self.data.as_slice_sync())?;
            }
            nvtx::range_pop!();
        }

        {
            nvtx::range_push!("step_verify_mem");
            tracing::debug!("step_verify_mem");
            let args = &[
                self.ctrl.as_slice_sync(),
                self.io.as_slice_sync(),
                self.data.as_slice_sync(),
            ];

            #[cfg(not(feature = "seq"))]
            machine.par_step_verify_mem(self.steps, last_cycle, args)?;

            #[cfg(feature = "seq")]
            for cycle in 0..last_cycle {
                machine.step_verify_mem(self.steps, cycle, args)?;
            }
            nvtx::range_pop!();
        }

        nvtx::range_pop!();
        Ok(())
    }

    #[tracing::instrument(skip_all)]
    fn compute_verify_bytes(&mut self, machine: &mut MachineContext) -> Result<()> {
        nvtx::range_push!("verify_bytes");
        tracing::debug!("verify_bytes");

        let last_cycle = self.steps - ZK_CYCLES;

        machine.sort("bytes")?;

        #[cfg(not(feature = "seq"))]
        {
            nvtx::range_push!("inject_verify_bytes_backs");
            tracing::debug!("inject_verify_bytes_backs");
            for cycle in 1..last_cycle {
                machine.inject_verify_bytes_backs(self.steps, cycle, self.data.as_slice_sync())?;
            }
            nvtx::range_pop!();
        }

        {
            nvtx::range_push!("step_verify_bytes");
            tracing::debug!("step_verify_bytes");
            let args = &[
                self.ctrl.as_slice_sync(),
                self.io.as_slice_sync(),
                self.data.as_slice_sync(),
            ];

            #[cfg(not(feature = "seq"))]
            (0..last_cycle)
                .into_par_iter()
                .try_for_each(|cycle| machine.step_verify_bytes(self.steps, cycle, args))?;

            #[cfg(feature = "seq")]
            for cycle in 0..last_cycle {
                machine.step_verify_bytes(self.steps, cycle, args)?;
            }
            nvtx::range_pop!();
        }

        nvtx::range_pop!();
        Ok(())
    }
}
