//! The Strata release a pod runs, pinned (kb d170, d172): the two archives of
//! v0.2.16 the first boot downloads, with the sha256 GitHub publishes for
//! each. Moving to another release is a kb decision with a re-measure.

/// One release archive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Archive {
    pub url: &'static str,
    pub sha256: &'static str,
}

/// Strata's release tag, without the `v`.
pub const VERSION: &str = "0.2.16";

/// The engine for CUDA 13 on x86-64 Linux: the `strata` binary and its CUDA
/// libraries (580095243 bytes).
pub const ENGINE: Archive = Archive {
    url: "https://github.com/Niko1221/Strata/releases/download/v0.2.16/strata-engine-0.1.62-linux-x86_64-cuda-cuda13.tar.gz",
    sha256: "6723e1b6b6042bfe1d368723ebf873afb0fa65f4f1908d948dd865098f6cfb9e",
};

/// The control archive: the server (`serve/server.py`), its Python and the
/// packing and draft tools (94383559 bytes).
pub const CONTROL: Archive = Archive {
    url: "https://github.com/Niko1221/Strata/releases/download/v0.2.16/strata-control-0.1.62-linux-x86_64.tar.gz",
    sha256: "045b6a7d7fbfbddab814794db5763d6244c887a394403b9fe0e8a4714d9bef8c",
};
