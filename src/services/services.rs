use super::start_service::*;
use crate::platform::EventFd;
use crate::units::*;
use std::os::unix::io::RawFd;
use std::os::unix::net::UnixDatagram;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::Mutex;

#[derive(Debug)]
pub struct ServiceRuntimeInfo {
    pub restarted: u64,
    pub up_since: Option<std::time::Instant>,
}

#[derive(Debug)]
pub struct Service {
    pub pid: Option<nix::unistd::Pid>,
    pub service_config: ServiceConfig,

    pub socket_names: Vec<String>,

    pub status_msgs: Vec<String>,

    pub process_group: Option<nix::unistd::Pid>,

    pub runtime_info: ServiceRuntimeInfo,
    pub signaled_ready: bool,

    pub notifications: Option<Arc<Mutex<UnixDatagram>>>,
    pub notifications_path: Option<std::path::PathBuf>,

    pub stdout_dup: Option<(RawFd, RawFd)>,
    pub stderr_dup: Option<(RawFd, RawFd)>,
    pub notifications_buffer: String,
    pub stdout_buffer: Vec<u8>,
    pub stderr_buffer: Vec<u8>,
}

pub enum StartResult {
    Started,
    WaitingForSocket,
}

impl Service {
    pub fn start(
        &mut self,
        id: UnitId,
        name: &str,
        fd_store: ArcMutFDStore,
        pid_table: ArcMutPidTable,
        notification_socket_path: std::path::PathBuf,
        eventfds: &[EventFd],
        allow_ignore: bool,
    ) -> Result<StartResult, String> {
        if self.pid.is_some() {
            return Err(format!(
                "Service {} has already a pid {:?}",
                name,
                self.pid.unwrap()
            ));
        }
        if self.process_group.is_some() {
            return Err(format!(
                "Service {} has already a pgid {:?}",
                name,
                self.process_group.unwrap()
            ));
        }
        if !allow_ignore || self.socket_names.is_empty() {
            trace!("Start service {}", name);
            super::prepare_service::prepare_service(self, name, &notification_socket_path)?;

            self.run_prestart(id, name, pid_table.clone())?;
            {
                let mut pid_table_locked = pid_table.lock().unwrap();
                // This mainly just forks the process. The waiting (if necessary) is done below
                // Doing it under the lock of the pid_table prevents races between processes exiting very
                // fast and inserting the new pid into the pid table
                start_service(self, name.clone(), &*fd_store.read().unwrap())?;
                if let Some(new_pid) = self.pid {
                    pid_table_locked.insert(
                        new_pid,
                        PidEntry::Service(id, self.service_config.srcv_type),
                    );
                    crate::platform::notify_event_fds(&eventfds);
                }
            }
            if let Some(sock) = &self.notifications {
                let sock = sock.clone();
                super::fork_parent::wait_for_service(
                    self,
                    name,
                    &*sock.lock().unwrap(),
                    pid_table.clone(),
                )?;
            }
            self.run_poststart(id, name, pid_table.clone())
                .map_err(|e| {
                    format!("Some poststart command failed for service {}: {}", name, e)
                })?;
            Ok(StartResult::Started)
        } else {
            trace!(
                "Ignore service {} start, waiting for socket activation instead",
                name,
            );
            crate::platform::notify_event_fds(&eventfds);
            Ok(StartResult::WaitingForSocket)
        }
    }

    fn stop(&mut self, id: UnitId, name: &str, pid_table: ArcMutPidTable) -> Result<(), String> {
        let stop_res = self.run_stop_cmd(id, name, pid_table.clone());
        if let Some(proc_group) = self.process_group {
            match nix::sys::signal::kill(proc_group, nix::sys::signal::Signal::SIGKILL) {
                Ok(_) => trace!("Success killing process group for service {}", name,),
                Err(e) => error!("Error killing process group for service {}: {}", name, e,),
            }
        } else {
            trace!("Tried to kill service that didn't have a process-group. This might have resulted in orphan processes.");
        }
        self.pid = None;
        self.process_group = None;
        let poststop_res = self.run_poststop(id, name, pid_table.clone());

        if poststop_res.is_err() && stop_res.is_err() {
            Err(format!(
                "Errors while running both stop commands {} and poststop commands {}",
                stop_res.err().unwrap(),
                poststop_res.err().unwrap()
            ))
        } else if stop_res.is_err() {
            Err(format!(
                "Errors while running stop commands {}",
                stop_res.err().unwrap()
            ))
        } else if poststop_res.is_err() {
            Err(format!(
                "Errors while running poststop commands {}",
                poststop_res.err().unwrap()
            ))
        } else {
            Ok(())
        }
    }

    pub fn kill(
        &mut self,
        id: UnitId,
        name: &str,
        pid_table: ArcMutPidTable,
    ) -> Result<(), String> {
        self.stop(id, name, pid_table)
    }

    pub fn get_start_timeout(&self) -> Option<std::time::Duration> {
        if let Some(timeout) = &self.service_config.starttimeout {
            match timeout {
                Timeout::Duration(dur) => Some(*dur),
                Timeout::Infinity => None,
            }
        } else {
            if let Some(timeout) = &self.service_config.generaltimeout {
                match timeout {
                    Timeout::Duration(dur) => Some(*dur),
                    Timeout::Infinity => None,
                }
            } else {
                // TODO add default timeout if neither starttimeout nor generaltimeout was set
                None
            }
        }
    }

    fn get_stop_timeout(&self) -> Option<std::time::Duration> {
        if let Some(timeout) = &self.service_config.stoptimeout {
            match timeout {
                Timeout::Duration(dur) => Some(*dur),
                Timeout::Infinity => None,
            }
        } else {
            if let Some(timeout) = &self.service_config.generaltimeout {
                match timeout {
                    Timeout::Duration(dur) => Some(*dur),
                    Timeout::Infinity => None,
                }
            } else {
                // TODO add default timeout if neither starttimeout nor generaltimeout was set
                None
            }
        }
    }

    fn run_cmd(
        &mut self,
        cmd_str: &str,
        id: UnitId,
        name: &str,
        timeout: Option<std::time::Duration>,
        pid_table: ArcMutPidTable,
    ) -> Result<(), String> {
        let split = cmd_str.split(' ').collect::<Vec<_>>();
        let mut cmd = Command::new(split[0]);
        for part in &split[1..] {
            cmd.arg(part);
        }
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        cmd.stdin(Stdio::null());
        trace!("Run {} for service: {}", cmd_str, name);
        let spawn_result = {
            let mut pid_table_locked = pid_table.lock().unwrap();
            let res = cmd.spawn();
            if let Ok(child) = &res {
                pid_table_locked.insert(
                    nix::unistd::Pid::from_raw(child.id() as i32),
                    PidEntry::Helper(id, name.to_string()),
                );
            }
            res
        };
        match spawn_result {
            Ok(mut child) => {
                trace!("Wait for {} for service: {}", cmd_str, name);
                let wait_result: Result<(), String> =
                    match wait_for_helper_child(&mut child, pid_table.clone(), timeout) {
                        WaitResult::InTime(Err(e)) => {
                            return Err(format!("error while waiting on {}: {}", cmd_str, e));
                        }
                        WaitResult::InTime(Ok(exitstatus)) => {
                            trace!("success running {} for service: {}", cmd_str, name);
                            if exitstatus.success() {
                                Ok(())
                            } else {
                                Err(format!("{} returned status: {:?}", cmd_str, exitstatus))
                            }
                        }
                        WaitResult::TimedOut => {
                            trace!("Timeout running {} for service: {}", cmd_str, name);
                            let _ = child.kill();
                            Err(format!(
                                "Timeout ({:?}) reached while running {} for service: {}",
                                timeout, cmd_str, name
                            ))
                        }
                    };
                {
                    use std::io::Read;
                    if let Some(stream) = &mut child.stderr {
                        let mut buf = Vec::new();
                        let _bytes = stream.read_to_end(&mut buf).unwrap();
                        self.stderr_buffer.extend(buf);
                    }
                    if let Some(stream) = &mut child.stdout {
                        let mut buf = Vec::new();
                        let _bytes = stream.read_to_end(&mut buf).unwrap();
                        self.stdout_buffer.extend(buf);
                    }
                }

                pid_table
                    .lock()
                    .unwrap()
                    .remove(&nix::unistd::Pid::from_raw(child.id() as i32));
                wait_result
            }
            Err(e) => Err(format!(
                "Error while spawning child process {}, {}",
                cmd_str, e
            )),
        }
    }

    fn run_all_cmds(
        &mut self,
        cmds: &Vec<String>,
        id: UnitId,
        name: &str,
        timeout: Option<std::time::Duration>,
        pid_table: ArcMutPidTable,
    ) -> Result<(), String> {
        for cmd in cmds {
            self.run_cmd(cmd, id, name, timeout, pid_table.clone())?;
        }
        Ok(())
    }

    fn run_stop_cmd(
        &mut self,
        id: UnitId,
        name: &str,
        pid_table: ArcMutPidTable,
    ) -> Result<(), String> {
        if self.service_config.stop.is_empty() {
            return Ok(());
        }
        let timeout = self.get_stop_timeout();
        let cmds = self.service_config.stop.clone();
        self.run_all_cmds(&cmds, id, name, timeout, pid_table.clone())
    }
    fn run_prestart(
        &mut self,
        id: UnitId,
        name: &str,
        pid_table: ArcMutPidTable,
    ) -> Result<(), String> {
        if self.service_config.startpre.is_empty() {
            return Ok(());
        }
        let timeout = self.get_start_timeout();
        let cmds = self.service_config.startpre.clone();
        let res = self
            .run_all_cmds(&cmds, id, name, timeout, pid_table.clone())
            .map_err(|e| format!("Some prestart command failed for service {}: {}", name, e));
        if let Err(e) = res {
            Err(self.run_poststop_because_err(id, name, pid_table, e))
        } else {
            Ok(())
        }
    }
    fn run_poststart(
        &mut self,
        id: UnitId,
        name: &str,
        pid_table: ArcMutPidTable,
    ) -> Result<(), String> {
        if self.service_config.startpost.is_empty() {
            return Ok(());
        }
        let timeout = self.get_start_timeout();
        let cmds = self.service_config.startpost.clone();
        let res = self
            .run_all_cmds(&cmds, id, name, timeout, pid_table.clone())
            .map_err(|e| format!("Some prestart command failed for service {}: {}", name, e));
        if let Err(e) = res {
            Err(self.run_poststop_because_err(id, name, pid_table, e))
        } else {
            Ok(())
        }
    }

    fn run_poststop_because_err(
        &mut self,
        id: UnitId,
        name: &str,
        pid_table: ArcMutPidTable,
        previous_err: String,
    ) -> String {
        let poststop_res = self.run_poststop(id, name, pid_table.clone());

        if poststop_res.is_err() {
            format!(
                "Errors while running both helper commands {} and poststop commands {}",
                previous_err,
                poststop_res.err().unwrap()
            )
        } else if poststop_res.is_err() {
            format!(
                "Errors while running poststop commands {}",
                poststop_res.err().unwrap()
            )
        } else {
            format!("Errors while running helper commands {}", previous_err)
        }
    }

    fn run_poststop(
        &mut self,
        id: UnitId,
        name: &str,
        pid_table: ArcMutPidTable,
    ) -> Result<(), String> {
        if self.service_config.startpost.is_empty() {
            return Ok(());
        }
        let timeout = self.get_start_timeout();
        let cmds = self.service_config.stoppost.clone();
        self.run_all_cmds(&cmds, id, name, timeout, pid_table.clone())
    }
}

enum WaitResult {
    TimedOut,
    InTime(std::io::Result<crate::signal_handler::ChildTermination>),
}

/// Wait for the termination of a subprocess, with an optional timeout.
/// An error does not mean that the waiting actually failed.
/// This might also happen because it was collected by the signal_handler.
/// This could be fixed by using the waitid() with WNOWAIT in the signal handler but
/// that has not been ported to rust
fn wait_for_helper_child(
    child: &mut std::process::Child,
    pid_table: ArcMutPidTable,
    time_out: Option<std::time::Duration>,
) -> WaitResult {
    let pid = nix::unistd::Pid::from_raw(child.id() as i32);
    let mut counter = 1u64;
    let start_time = std::time::Instant::now();
    loop {
        if let Some(time_out) = time_out {
            if start_time.elapsed() >= time_out {
                return WaitResult::TimedOut;
            }
        }
        {
            let mut pid_table_locked = pid_table.lock().unwrap();
            match pid_table_locked.get(&pid) {
                Some(entry) => {
                    match entry {
                        PidEntry::OneshotExited(_) => {
                            // Should never happen
                            unreachable!(
                            "Was waiting on helper process but pid got saved as PidEntry::OneshotExited"
                        );
                        }
                        PidEntry::Service(_, _) => {
                            // Should never happen
                            unreachable!(
                            "Was waiting on helper process but pid got saved as PidEntry::Service"
                        );
                        }
                        PidEntry::Helper(_, _) => {
                            // Need to wait longer
                        }
                        PidEntry::HelperExited(_) => {
                            let entry_owned = pid_table_locked.remove(&pid).unwrap();
                            if let PidEntry::HelperExited(termination_owned) = entry_owned {
                                return WaitResult::InTime(Ok(termination_owned));
                            }
                        }
                    }
                }
                None => {
                    // Should not happen. Either there is an Helper entry oder a Exited entry
                    unreachable!("No entry for child found")
                }
            }
        }
        // exponential backoff to get low latencies for fast processes
        // but not hog the cpu for too long
        // start at 0.05 ms
        // capped to 10 ms to not introduce too big latencies
        // TODO review those numbers
        let sleep_dur = std::time::Duration::from_micros(counter * 50);
        let sleep_cap = std::time::Duration::from_millis(10);
        let sleep_dur = sleep_dur.min(sleep_cap);
        if sleep_dur < sleep_cap {
            counter = counter * 2;
        }
        std::thread::sleep(sleep_dur);
    }
}
