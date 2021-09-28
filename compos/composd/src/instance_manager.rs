/*
 * Copyright (C) 2021 The Android Open Source Project
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

//! Starts and manages instances of the CompOS VM. At most one instance should be running at
//! a time.

use anyhow::{bail, Context, Result};
use compos_aidl_interface::aidl::com::android::compos::ICompOsService::ICompOsService;
use compos_aidl_interface::binder::Strong;
use compos_common::compos_client::VmInstance;
use compos_common::{COMPOS_DATA_ROOT, CURRENT_DIR, INSTANCE_IMAGE_FILE, PRIVATE_KEY_BLOB_FILE};
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};

pub struct CompOsInstance {
    #[allow(dead_code)] // Keeps VirtualizationService & the VM alive
    vm_instance: VmInstance,
    service: Strong<dyn ICompOsService>,
}

#[derive(Default)]
pub struct InstanceManager(Mutex<State>);

impl InstanceManager {
    pub fn get_running_service(&self) -> Result<Strong<dyn ICompOsService>> {
        let mut state = self.0.lock().unwrap();
        let instance = state.get_running_instance().context("No running instance")?;
        Ok(instance.service.clone())
    }

    pub fn start_current_instance(&self) -> Result<Arc<CompOsInstance>> {
        let mut state = self.0.lock().unwrap();
        state.mark_starting()?;
        // Don't hold the lock while we start the instance to avoid blocking other callers.
        drop(state);

        let instance = self.try_start_current_instance();

        let mut state = self.0.lock().unwrap();
        if let Ok(ref instance) = instance {
            state.mark_started(instance)?;
        } else {
            state.mark_stopped();
        }
        instance
    }

    fn try_start_current_instance(&self) -> Result<Arc<CompOsInstance>> {
        // TODO: Create instance_image & keys if needed
        // TODO: Hold on to an IVirtualizationService
        let instance_image: PathBuf =
            [COMPOS_DATA_ROOT, CURRENT_DIR, INSTANCE_IMAGE_FILE].iter().collect();

        let vm_instance = VmInstance::start(&instance_image).context("Starting VM")?;
        let service = vm_instance.get_service().context("Connecting to CompOS")?;

        let key_blob: PathBuf =
            [COMPOS_DATA_ROOT, CURRENT_DIR, PRIVATE_KEY_BLOB_FILE].iter().collect();
        let key_blob = fs::read(key_blob).context("Reading private key")?;
        service.initializeSigningKey(&key_blob).context("Loading key")?;

        Ok(Arc::new(CompOsInstance { vm_instance, service }))
    }
}

// Ensures we only run one instance at a time.
// Valid states:
// Starting: is_starting is true, running_instance is None.
// Started: is_starting is false, running_instance is Some(x) and there is a strong ref to x.
// Stopped: is_starting is false and running_instance is None or a weak ref to a dropped instance.
#[derive(Default)]
struct State {
    running_instance: Option<Weak<CompOsInstance>>,
    is_starting: bool,
}

impl State {
    // Move to Starting iff we are Stopped.
    fn mark_starting(&mut self) -> Result<()> {
        if self.is_starting {
            bail!("An instance is already starting");
        }
        if let Some(weak) = &self.running_instance {
            if weak.strong_count() != 0 {
                bail!("An instance is already running");
            }
        }
        self.running_instance = None;
        self.is_starting = true;
        Ok(())
    }

    // Move from Starting to Stopped.
    fn mark_stopped(&mut self) {
        if !self.is_starting || self.running_instance.is_some() {
            panic!("Tried to mark stopped when not starting");
        }
        self.is_starting = false;
    }

    // Move from Starting to Started.
    fn mark_started(&mut self, instance: &Arc<CompOsInstance>) -> Result<()> {
        if !self.is_starting {
            panic!("Tried to mark started when not starting")
        }
        if self.running_instance.is_some() {
            panic!("Attempted to mark started when already started");
        }
        self.is_starting = false;
        self.running_instance = Some(Arc::downgrade(instance));
        Ok(())
    }

    // Return the running instance if we are in the Started state.
    fn get_running_instance(&mut self) -> Option<Arc<CompOsInstance>> {
        if self.is_starting {
            return None;
        }
        let instance = self.running_instance.as_ref()?.upgrade();
        if instance.is_none() {
            // No point keeping an orphaned weak reference
            self.running_instance = None;
        }
        instance
    }
}