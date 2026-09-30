use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("invalid Harbor version '{0}', expected vMAJOR.MINOR.PATCH")]
    InvalidVersion(String),

    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to start `{cmd}`: {source}")]
    CommandSpawn {
        cmd: String,
        #[source]
        source: std::io::Error,
    },

    #[error("`{cmd}` exited with {status}")]
    CommandFailed { cmd: String, status: String },

    #[error("Docker daemon architecture is {0}, a native arm64 host is required")]
    HostArch(String),

    #[error("patch target '{needle}' not found in {}, upstream changed", file.display())]
    PatchTarget { file: PathBuf, needle: String },

    #[error("no goharbor/photon base image referenced in {}", .0.display())]
    BaseImageNotFound(PathBuf),

    #[error("{image} is {arch}, expected arm64")]
    ImageArch { image: String, arch: String },

    #[error("{image}:{path} is ELF machine {machine}, expected aarch64")]
    BinaryArch {
        image: String,
        path: String,
        machine: u16,
    },

    #[error("{0} is not an ELF binary")]
    InvalidElf(String),
}

impl Error {
    pub fn io(context: impl Into<String>, source: std::io::Error) -> Self {
        Self::Io {
            context: context.into(),
            source,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;
