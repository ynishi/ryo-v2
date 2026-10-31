//! `ledger-merge`: the git merge driver of the ledgers.
//!
//! ```text
//! ledger-merge <base> <ours> <theirs> [<marker-size>] [<path>]
//! ```
//!
//! git runs it as `ledger-merge %O %A %B %L %P` (the driver `just ledger-merge-install` sets up).
//! It writes the merge into `<ours>`, as git expects, and exits 0 when the merge is clean, 1 when
//! it left conflict markers, and 2 when it could not run; then `<ours>` is unchanged.
//! `<path>` only names the file in messages.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::{env, fs, io};

use ryo_v2_ledger_merge::{DEFAULT_MARKER_SIZE, merge};

/// Why the driver could not merge.
#[derive(Debug, thiserror::Error)]
enum DriverError {
    #[error("usage: ledger-merge <base> <ours> <theirs> [<marker-size>] [<path>]")]
    Usage,
    #[error("marker size `{0}` is not a number of at least 1")]
    MarkerSize(String),
    #[error("cannot read {}: {source}", .path.display())]
    Read { path: PathBuf, source: io::Error },
    #[error("cannot write {}: {source}", .path.display())]
    Write { path: PathBuf, source: io::Error },
}

fn read(path: &Path) -> Result<String, DriverError> {
    fs::read_to_string(path).map_err(|source| DriverError::Read {
        path: path.to_path_buf(),
        source,
    })
}

/// Merges and writes `<ours>`; the number of conflicts left.
fn run(args: &[String]) -> Result<usize, DriverError> {
    let [base, ours, theirs, rest @ ..] = args else {
        return Err(DriverError::Usage);
    };
    if rest.len() > 2 {
        return Err(DriverError::Usage);
    }
    let marker_size = match rest.first() {
        None => DEFAULT_MARKER_SIZE,
        Some(s) => match s.parse::<usize>() {
            Ok(n) if n >= 1 => n,
            _ => return Err(DriverError::MarkerSize(s.clone())),
        },
    };
    let (base, ours, theirs) = (
        PathBuf::from(base),
        PathBuf::from(ours),
        PathBuf::from(theirs),
    );
    let merged = merge(&read(&base)?, &read(&ours)?, &read(&theirs)?, marker_size);
    // Written beside `<ours>` and renamed over it, so a failed write leaves `<ours>` unchanged.
    let mut tmp = ours.clone().into_os_string();
    tmp.push(".ledger-merge");
    let tmp = PathBuf::from(tmp);
    fs::write(&tmp, &merged.text)
        .and_then(|()| fs::rename(&tmp, &ours))
        .map_err(|source| {
            let _ = fs::remove_file(&tmp);
            DriverError::Write {
                path: ours.clone(),
                source,
            }
        })?;
    Ok(merged.conflicts)
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let path = args
        .get(4)
        .cloned()
        .unwrap_or_else(|| "the ledger".to_owned());
    match run(&args) {
        Ok(0) => ExitCode::SUCCESS,
        Ok(n) => {
            eprintln!("ledger-merge: {n} conflict(s) left in {path}");
            ExitCode::from(1)
        }
        Err(e) => {
            eprintln!("ledger-merge: {e}");
            ExitCode::from(2)
        }
    }
}
