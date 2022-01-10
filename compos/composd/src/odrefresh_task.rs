/*
 * Copyright 2021 The Android Open Source Project
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *      http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

//! Handle running odrefresh in the VM, with an async interface to allow cancellation

use crate::fd_server_helper::FdServerConfig;
use crate::instance_starter::CompOsInstance;
use android_system_composd::aidl::android::system::composd::{
    ICompilationTask::ICompilationTask, ICompilationTaskCallback::ICompilationTaskCallback,
};
use android_system_composd::binder::{Interface, Result as BinderResult, Strong};
use anyhow::{Context, Result};
use compos_aidl_interface::aidl::com::android::compos::ICompOsService::ICompOsService;
use compos_common::odrefresh::ExitCode;
use log::{error, warn};
use rustutils::system_properties;
use std::fs::{remove_dir_all, File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;

const ART_APEX_DATA: &str = "/data/misc/apexdata/com.android.art";

#[derive(Clone)]
pub struct OdrefreshTask {
    running_task: Arc<Mutex<Option<RunningTask>>>,
}

impl Interface for OdrefreshTask {}

impl ICompilationTask for OdrefreshTask {
    fn cancel(&self) -> BinderResult<()> {
        let task = self.take();
        // Drop the VM, which should end compilation - and cause our thread to exit
        drop(task);
        Ok(())
    }
}

struct RunningTask {
    callback: Strong<dyn ICompilationTaskCallback>,
    #[allow(dead_code)] // Keeps the CompOS VM alive
    comp_os: Arc<CompOsInstance>,
}

impl OdrefreshTask {
    /// Return the current running task, if any, removing it from this CompilationTask.
    /// Once removed, meaning the task has ended or been canceled, further calls will always return
    /// None.
    fn take(&self) -> Option<RunningTask> {
        self.running_task.lock().unwrap().take()
    }

    pub fn start(
        comp_os: Arc<CompOsInstance>,
        target_dir_name: String,
        callback: &Strong<dyn ICompilationTaskCallback>,
    ) -> Result<OdrefreshTask> {
        let service = comp_os.get_service();
        let task = RunningTask { comp_os, callback: callback.clone() };
        let task = OdrefreshTask { running_task: Arc::new(Mutex::new(Some(task))) };

        task.clone().start_thread(service, target_dir_name);

        Ok(task)
    }

    fn start_thread(self, service: Strong<dyn ICompOsService>, target_dir_name: String) {
        thread::spawn(move || {
            let exit_code = run_in_vm(service, &target_dir_name);

            let task = self.take();
            // We don't do the callback if cancel has already happened.
            if let Some(task) = task {
                let result = match exit_code {
                    Ok(ExitCode::CompilationSuccess) => task.callback.onSuccess(),
                    Ok(exit_code) => {
                        error!("Unexpected odrefresh result: {:?}", exit_code);
                        task.callback.onFailure()
                    }
                    Err(e) => {
                        error!("Running odrefresh failed: {:?}", e);
                        task.callback.onFailure()
                    }
                };
                if let Err(e) = result {
                    warn!("Failed to deliver callback: {:?}", e);
                }
            }
        });
    }
}

fn run_in_vm(service: Strong<dyn ICompOsService>, target_dir_name: &str) -> Result<ExitCode> {
    let output_root = Path::new(ART_APEX_DATA);

    // We need to remove the target directory because odrefresh running in compos will create it
    // (and can't see the existing one, since authfs doesn't show it existing files in an output
    // directory).
    let target_path = output_root.join(target_dir_name);
    if target_path.exists() {
        remove_dir_all(&target_path)
            .with_context(|| format!("Failed to delete {}", target_path.display()))?;
    }

    let staging_dir = open_dir(composd_native::palette_create_odrefresh_staging_directory()?)?;
    let system_dir = open_dir(Path::new("/system"))?;
    let output_dir = open_dir(output_root)?;

    // Spawn a fd_server to serve the FDs.
    let fd_server_config = FdServerConfig {
        ro_dir_fds: vec![system_dir.as_raw_fd()],
        rw_dir_fds: vec![staging_dir.as_raw_fd(), output_dir.as_raw_fd()],
        ..Default::default()
    };
    let fd_server_raii = fd_server_config.into_fd_server()?;

    let zygote_arch = system_properties::read("ro.zygote")?;
    let exit_code = service.odrefresh(
        system_dir.as_raw_fd(),
        output_dir.as_raw_fd(),
        staging_dir.as_raw_fd(),
        target_dir_name,
        &zygote_arch,
    )?;

    drop(fd_server_raii);
    ExitCode::from_i32(exit_code.into())
}

/// Returns an owned FD of the directory. It currently returns a `File` as a FD owner, but
/// it's better to use `std::os::unix::io::OwnedFd` once/if it becomes standard.
fn open_dir(path: &Path) -> Result<File> {
    OpenOptions::new()
        .custom_flags(libc::O_DIRECTORY)
        .read(true) // O_DIRECTORY can only be opened with read
        .open(path)
        .with_context(|| format!("Failed to open {:?} directory as path fd", path))
}