// Copyright (c) 2016 Chef Software Inc. and/or applicable contributors
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

pub mod config;

use std;
use std::fmt;
use std::fs::File;
use std::io::prelude::*;
use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc::{sync_channel, SyncSender, Receiver, TryRecvError};

use ansi_term::Colour::{Yellow, Red, Green};
use hcore::package::PackageIdent;
use hcore::service::ServiceGroup;
use hcore::crypto::hash;
use hcore::fs::{self, CACHE_ARTIFACT_PATH, FS_ROOT_PATH};
use hcore::util::perm::{set_owner, set_permissions};

use {PRODUCT, VERSION};
use common::ui::UI;
use config::{gconfig, UpdateStrategy, Topology};
use depot_client::Client;
use error::Result;
use health_check;
use manager::signals;
use manager::census::CensusList;
use manager::service::config::ServiceConfig;
use package::Package;
use supervisor::{Supervisor, RuntimeConfig};
use util;

static LOGKEY: &'static str = "SR";

#[derive(Debug, PartialEq, Eq, RustcEncodable)]
enum LastRestartDisplay {
    None,
    ElectionInProgress,
    ElectionNoQuorum,
    ElectionFinished,
}

#[derive(Debug, RustcEncodable)]
pub struct Service {
    pub needs_restart: bool,
    pub package: Package,
    pub service_config_incarnation: Option<u64>,
    pub service_group: ServiceGroup,
    pub update_strategy: UpdateStrategy,
    initialized: bool,
    last_restart_display: LastRestartDisplay,
    supervisor: Supervisor,
    topology: Topology,
}

impl Service {
    pub fn new(service_group: ServiceGroup,
               package: Package,
               topology: Topology,
               update_strategy: UpdateStrategy)
               -> Result<Service> {
        let (svc_user, svc_group) = try!(util::users::get_user_and_group(&package.pkg_install));
        let sg = format!("{}.{}", service_group.service, service_group.group);
        outputln!(preamble sg, "Process will run as user={}, group={}",
                  &svc_user,
                  &svc_group);
        let runtime_config = RuntimeConfig::new(svc_user, svc_group);
        let supervisor = Supervisor::new(package.ident().clone(), runtime_config);
        Ok(Service {
            service_group: service_group,
            supervisor: supervisor,
            package: package,
            topology: topology,
            needs_restart: false,
            update_strategy: update_strategy,
            last_restart_display: LastRestartDisplay::None,
            initialized: false,
            service_config_incarnation: None,
        })
    }

    pub fn service_group_str(&self) -> String {
        format!("{}.{}",
                self.service_group.service,
                self.service_group.group)
    }

    pub fn start(&mut self) -> Result<()> {
        self.supervisor.start()
    }

    pub fn restart(&mut self, census_list: &CensusList) -> Result<()> {
        match self.topology {
            Topology::Leader | Topology::Initializer => {
                if let Some(census) = census_list.get(&format!("{}.{}",
                                                               self.service_group.service,
                                                               self.service_group.group)) {
                    let me = census.me();
                    if me.get_election_is_running() {
                        if self.last_restart_display != LastRestartDisplay::ElectionInProgress {
                            outputln!(preamble self.service_group_str(),
                                      "Not restarting service; {}",
                                      Yellow.bold().paint("election in progress."));
                            self.last_restart_display = LastRestartDisplay::ElectionInProgress;
                        }
                    } else if me.get_election_is_no_quorum() {
                        if self.last_restart_display != LastRestartDisplay::ElectionNoQuorum {
                            outputln!(preamble self.service_group_str(),
                                      "Not restarting service; {}, {}.",
                                      Yellow.bold().paint("election in progress"),
                                      Red.bold().paint("and we have no quorum"));
                            self.last_restart_display = LastRestartDisplay::ElectionNoQuorum;
                        }
                    } else if me.get_election_is_finished() {
                        // We know we have a leader, so this is fine
                        let leader_id = census.get_leader().unwrap().get_member_id();
                        if self.last_restart_display != LastRestartDisplay::ElectionFinished {
                            outputln!(preamble self.service_group_str(),
                                      "Restarting service; {} is the leader",
                                      Green.bold().paint(leader_id));
                            self.last_restart_display = LastRestartDisplay::ElectionFinished;
                        }
                        self.needs_restart = false;
                        try!(self.supervisor.restart());
                    }
                }
            }
            Topology::Standalone => {
                self.needs_restart = false;
                try!(self.supervisor.restart());
            }
        }
        Ok(())
    }

    pub fn down(&mut self) -> Result<()> {
        self.supervisor.down()
    }

    pub fn send_signal(&self, signal: u32) -> Result<()> {
        if self.supervisor.pid.is_some() {
            signals::send_signal(self.supervisor.pid.unwrap(), signal)
        } else {
            debug!("No process to send the signal to");
            Ok(())
        }
    }

    pub fn is_down(&self) -> bool {
        self.supervisor.pid.is_none()
    }

    pub fn check_process(&mut self) -> Result<()> {
        self.supervisor.check_process()
    }

    pub fn write_butterfly_service_config(&mut self, config: String) -> bool {
        let on_disk_path = fs::svc_path(&self.service_group.service).join("gossip.toml");
        let current_checksum = match hash::hash_file(&on_disk_path) {
            Ok(current_checksum) => current_checksum,
            Err(e) => {
                debug!("Failed to get current checksum for {:?}: {}",
                       on_disk_path,
                       e);
                String::new()
            }
        };
        let new_checksum = hash::hash_string(&config)
            .expect("We failed to hash a string in a method that can't return an error; not even \
                     sure what this means");
        if new_checksum != current_checksum {
            let new_filename = format!("{}.write", on_disk_path.to_string_lossy());

            let mut new_file = match File::create(&new_filename) {
                Ok(new_file) => new_file,
                Err(e) => {
                    outputln!(preamble self.service_group_str(),
                        "Service configuration from butterfly failed to open the new file: {}",
                        Red.bold().paint(format!("{}", e)));
                    return false;
                }
            };

            if let Err(e) = new_file.write_all(config.as_bytes()) {
                outputln!(preamble self.service_group_str(),
                    "Service configuration from butterfly failed to write: {}",
                    Red.bold().paint(format!("{}", e)));
                return false;
            }

            if let Err(e) = std::fs::rename(&new_filename, &on_disk_path) {
                outputln!(preamble self.service_group_str(),
                    "Service configuration from butterfly failed to rename: {}",
                    Red.bold().paint(format!("{}", e)));
                return false;
            }

            if let Err(e) = set_owner(&on_disk_path,
                                      &self.supervisor.runtime_config.svc_user,
                                      &self.supervisor.runtime_config.svc_group) {
                outputln!(preamble self.service_group_str(),
                    "Service configuration from butterfly failed to set ownership: {}",
                    Red.bold().paint(format!("{}", e)));
                return false;
            }

            if let Err(e) = set_permissions(&on_disk_path, 0o770) {
                outputln!(preamble self.service_group_str(),
                    "Service configuration from butterfly failed to set permissions: {}",
                    Red.bold().paint(format!("{}", e)));
                return false;
            }

            outputln!(preamble self.service_group_str(),
                "Service configuration updated from butterfly: {}",
                Green.bold().paint(new_checksum));
            true
        } else {
            false
        }
    }

    pub fn health_check(&self) -> Result<health_check::CheckResult> {
        self.package.health_check(&self.supervisor)
    }

    pub fn initialize(&mut self) {
        if !self.initialized {
            match self.package.initialize() {
                Ok(()) => outputln!(preamble self.service_group_str(), "{}", "Initializing"),
                Err(e) => {
                    outputln!(preamble self.service_group_str(), "Initialization failed: {}", e)
                }
            }
            self.initialized = true
        }
    }

    pub fn reconfigure(&mut self, census_list: &CensusList) {
        let sg = format!("{}", self.service_group);
        let mut service_config =
            match ServiceConfig::new(&sg, &self.package, census_list, gconfig().bind()) {
                Ok(sc) => sc,
                Err(e) => {
                    outputln!(preamble self.service_group_str(),
                              "Error generating Service Configuration; not reconfiguring: {}",
                              e);
                    return;
                }
            };
        self.package.create_svc_path();
        match service_config.write(&self.package) {
            Ok(true) => {
                self.needs_restart = true;
                match self.package.reconfigure() {
                    Ok(_) => {}
                    Err(e) => {
                        outputln!(preamble self.service_group_str(),
                            "Reconfiguration hook failed: {}", e);
                    }
                }
            }
            Ok(false) => {}
            Err(e) => {
                outputln!(preamble self.service_group_str(),
                    "Failed to write service configuration: {}", e);
            }
        }

        self.package.hooks().load_hooks();
        // Probably worth moving the run hook under compile all, eventually
        self.package.copy_run(&service_config);
        self.package.hooks().compile_all(&service_config);
    }
}

impl fmt::Display for Service {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.package)
    }
}
