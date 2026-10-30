use std::ffi::OsString;

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

#[derive(Debug, thiserror::Error)]
pub enum SamplerError {
    #[error("failed to initialize process sampler: {0}")]
    Init(sysinfo::Error),
}

#[derive(Debug, Clone, Copy)]
pub struct Sample {
    pub pid: Pid,
    pub cpu: f32,
}

pub struct Sampler {
    system: System,
    name: OsString,
}

impl Sampler {
    pub fn new(name: &str) -> Result<Self, SamplerError> {
        let mut sampler = Self {
            system: System::new().map_err(SamplerError::Init)?,
            name: name.into(),
        };
        sampler.refresh();
        Ok(sampler)
    }

    pub fn sample(&mut self) -> Option<Sample> {
        self.refresh();
        self.system
            .processes_by_exact_name(&self.name)
            .map(|process| Sample {
                pid: process.pid(),
                cpu: process.cpu_usage(),
            })
            .min_by_key(|sample| sample.pid)
    }

    fn refresh(&mut self) {
        self.system.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing().with_cpu(),
        );
    }
}
