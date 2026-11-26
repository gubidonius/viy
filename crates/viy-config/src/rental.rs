//! Machine-wide rental selection. The project and model configuration never supply this value.
use std::fs;
use std::io;

use crate::Paths;
use crate::atomic::write_atomically;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Rental {
    #[default]
    Runpod,
    Nebius {
        project: String,
        region: String,
    },
}

impl Rental {
    pub const NAMES: [&'static str; 2] = ["runpod", "nebius"];

    pub fn display_name(&self) -> &'static str {
        match self {
            Self::Runpod => "RunPod",
            Self::Nebius { .. } => "Nebius",
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::Runpod => "runpod",
            Self::Nebius { .. } => "nebius",
        }
    }

    pub fn nebius(project: &str, region: &str) -> io::Result<Self> {
        let safe = |s: &str| {
            !s.is_empty()
                && s.len() <= 100
                && s.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        };
        if !(safe(project) && project.starts_with("project-") && safe(region)) {
            return Err(io::Error::other("invalid Nebius project or region"));
        }
        Ok(Self::Nebius {
            project: project.into(),
            region: region.into(),
        })
    }

    pub fn load(paths: &Paths) -> io::Result<Self> {
        let file = paths.config_dir().join("gpu-provider");
        let value = match fs::read_to_string(file) {
            Ok(value) => value,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Self::Runpod),
            Err(err) => return Err(err),
        };
        match value.split_whitespace().collect::<Vec<_>>().as_slice() {
            ["runpod"] => Ok(Self::Runpod),
            ["nebius", project, region] => Self::nebius(project, region),
            _ => Err(io::Error::other("invalid gpu-provider selection")),
        }
    }

    pub fn save(&self, paths: &Paths) -> io::Result<()> {
        let value = match self {
            Self::Runpod => "runpod\n".to_string(),
            Self::Nebius { project, region } => format!("nebius {project} {region}\n"),
        };
        write_atomically(&paths.config_dir().join("gpu-provider"), &value)
    }
}
