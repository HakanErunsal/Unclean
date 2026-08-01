#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]
#![doc = "Provides the Unclean desktop entry point over the shared product core."]

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

use unclean_core::elevation::{
    ELEVATED_REQUEST_OPTION, ELEVATED_WORKER_COMMAND, run_elevated_worker,
};
use unclean_core::{Error, ErrorCode, Result};

fn main() -> ExitCode {
    match worker_request_path(std::env::args_os().skip(1)) {
        Ok(Some(request_path)) => {
            return ExitCode::from(run_elevated_worker(&request_path));
        }
        Ok(None) => {}
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(error.code().exit_code());
        }
    }

    match unclean_gui::run_gui() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let error = Error::Internal {
                message: format!("desktop startup failed: {error}"),
            };
            eprintln!("{error}");
            ExitCode::from(ErrorCode::Internal.exit_code())
        }
    }
}

fn worker_request_path(arguments: impl IntoIterator<Item = OsString>) -> Result<Option<PathBuf>> {
    let mut arguments = arguments.into_iter();
    let Some(command) = arguments.next() else {
        return Ok(None);
    };
    if command != ELEVATED_WORKER_COMMAND {
        return Ok(None);
    }
    let option = arguments.next();
    let path = arguments.next();
    if option.as_deref() != Some(ELEVATED_REQUEST_OPTION.as_ref())
        || path.as_ref().is_none_or(|value| value.is_empty())
        || arguments.next().is_some()
    {
        return Err(Error::InvalidInput {
            message: "the elevated worker requires one --request path".to_owned(),
        });
    }
    Ok(path.map(PathBuf::from))
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::PathBuf;

    use super::worker_request_path;

    #[test]
    fn worker_mode_accepts_only_one_request_path() -> Result<(), Box<dyn std::error::Error>> {
        let parsed = worker_request_path([
            OsString::from("__elevated-worker"),
            OsString::from("--request"),
            OsString::from("request.json"),
        ])?;
        assert_eq!(parsed, Some(PathBuf::from("request.json")));

        assert!(
            worker_request_path([
                OsString::from("__elevated-worker"),
                OsString::from("--request"),
            ])
            .is_err()
        );
        Ok(())
    }
}
