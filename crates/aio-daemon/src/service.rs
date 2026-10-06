//! Windows service entry point. Installing and removing the service is done
//! by the installer (`installer/aio-display.iss`) or `scripts/install.ps1`,
//! not by this binary.
//!
//! The service runs as LocalSystem and starts automatically at boot, before
//! anyone logs in, so it holds the display before Steam or other software
//! can grab it.

use std::ffi::OsString;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tracing::error;
use windows_service::service::{
    ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult, ServiceStatusHandle};
use windows_service::{define_windows_service, service_dispatcher};

/// Must match the name the installer and `scripts/install.ps1` register.
pub const SERVICE_NAME: &str = "aio-daemon";
/// Argument the SCM passes so the exe knows it runs as a service.
pub const SERVICE_ARG: &str = "--service";

define_windows_service!(ffi_service_main, service_main);

/// Hands the process to the SCM; returns when the service has stopped.
pub fn run_dispatcher() -> Result<()> {
    service_dispatcher::start(SERVICE_NAME, ffi_service_main).context("not started by the service manager")
}

fn service_main(_args: Vec<OsString>) {
    if let Err(e) = run_service() {
        error!("service failed: {e:#}");
    }
}

fn set_state(handle: &ServiceStatusHandle, state: ServiceState, exit_code: u32) -> Result<()> {
    let controls_accepted = if state == ServiceState::Running {
        ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN
    } else {
        ServiceControlAccept::empty()
    };
    handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: state,
        controls_accepted,
        exit_code: if exit_code == 0 { ServiceExitCode::Win32(0) } else { ServiceExitCode::ServiceSpecific(exit_code) },
        checkpoint: 0,
        wait_hint: Duration::from_secs(10),
        process_id: None,
    })?;
    Ok(())
}

fn run_service() -> Result<()> {
    let stop = Arc::new(tokio::sync::Notify::new());
    let stop_signal = stop.clone();
    let handle = service_control_handler::register(SERVICE_NAME, move |control| match control {
        ServiceControl::Stop | ServiceControl::Shutdown => {
            stop_signal.notify_one(); // stores a permit if nobody is waiting yet
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    })?;
    set_state(&handle, ServiceState::Running, 0)?;

    // Report "stopping" right away; the fade to black takes about a second.
    let stopping = async {
        stop.notified().await;
        let _ = set_state(&handle, ServiceState::StopPending, 0);
    };
    let result = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(anyhow::Error::from)
        .and_then(|rt| rt.block_on(crate::app::run(stopping)));

    if let Err(e) = &result {
        error!("{e:#}");
    }
    set_state(&handle, ServiceState::Stopped, u32::from(result.is_err()))?;
    result
}
