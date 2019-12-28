//! collect the different streams from the services
//! Stdout and stderr get redirected to the normal stdout/err but are prefixed with a unique string to identify their output
//! streams from the notification sockets get parsed and applied to the respective service

use crate::platform::reset_event_fd;
use crate::platform::EventFd;
use crate::services::Service;
use crate::units::*;
use std::{collections::HashMap, io::Write, os::unix::io::AsRawFd};

fn collect_from_srvc<F>(unit_table: ArcMutUnitTable, f: F) -> HashMap<i32, UnitId>
where
    F: Fn(&mut HashMap<i32, UnitId>, &Service, UnitId),
{
    unit_table
        .read()
        .unwrap()
        .iter()
        .fold(HashMap::new(), |mut map, (id, srvc_unit)| {
            let srvc_unit_locked = srvc_unit.lock().unwrap();
            if let UnitSpecialized::Service(srvc) = &srvc_unit_locked.specialized {
                f(&mut map, &srvc, id.clone());
            }
            map
        })
}

pub fn handle_all_streams(eventfd: EventFd, unit_table: ArcMutUnitTable) {
    loop {
        // need to collect all again. There might be a newly started service
        let fd_to_srvc_id = collect_from_srvc(unit_table.clone(), |map, srvc, id| {
            if let Some(socket) = &srvc.notifications {
                map.insert(socket.lock().unwrap().as_raw_fd(), id);
            }
        });

        let mut fdset = nix::sys::select::FdSet::new();
        for fd in fd_to_srvc_id.keys() {
            fdset.insert(*fd);
        }
        fdset.insert(eventfd.read_end());

        let result = nix::sys::select::select(None, Some(&mut fdset), None, None, None);
        match result {
            Ok(_) => {
                if fdset.contains(eventfd.read_end()) {
                    trace!("Interrupted notification select because the eventfd fired");
                    reset_event_fd(eventfd);
                    trace!("Reset eventfd value");
                }
                let mut buf = [0u8; 512];
                let unit_table_locked = &*unit_table.read().unwrap();
                for (fd, id) in &fd_to_srvc_id {
                    if fdset.contains(*fd) {
                        if let Some(srvc_unit) = unit_table_locked.get(id) {
                            let srvc_unit_locked = &mut *srvc_unit.lock().unwrap();
                            if let UnitSpecialized::Service(srvc) =
                                &mut srvc_unit_locked.specialized
                            {
                                if let Some(socket) = &srvc.notifications {
                                    let bytes = socket.lock().unwrap().recv(&mut buf[..]).unwrap();
                                    let note_str =
                                        String::from_utf8(buf[..bytes].to_vec()).unwrap();
                                    srvc.notifications_buffer.push_str(&note_str);
                                    crate::notification_handler::handle_notifications_from_buffer(
                                        srvc,
                                        &srvc_unit_locked.conf.name(),
                                    );
                                }
                            }
                        }
                    }
                }
            }
            Err(e) => {
                warn!("Error while selecting: {}", e);
            }
        }
    }
}

pub fn handle_all_std_out(eventfd: EventFd, unit_table: ArcMutUnitTable) {
    loop {
        // need to collect all again. There might be a newly started service
        let fd_to_srvc_id = collect_from_srvc(unit_table.clone(), |map, srvc, id| {
            if let Some(fd) = &srvc.stdout_dup {
                map.insert(fd.0, id);
            }
        });

        let mut fdset = nix::sys::select::FdSet::new();
        for fd in fd_to_srvc_id.keys() {
            fdset.insert(*fd);
        }
        fdset.insert(eventfd.read_end());

        let result = nix::sys::select::select(None, Some(&mut fdset), None, None, None);
        match result {
            Ok(_) => {
                if fdset.contains(eventfd.read_end()) {
                    trace!("Interrupted stdout select because the eventfd fired");
                    reset_event_fd(eventfd);
                    trace!("Reset eventfd value");
                }
                let mut buf = [0u8; 512];
                let unit_table_locked = &*unit_table.read().unwrap();
                for (fd, id) in &fd_to_srvc_id {
                    if fdset.contains(*fd) {
                        if let Some(srvc_unit) = unit_table_locked.get(id) {
                            let srvc_unit_locked = srvc_unit.lock().unwrap();
                            let name = srvc_unit_locked.conf.name();

                            // build the service-unique prefix
                            let mut prefix = String::new();
                            prefix.push('[');
                            prefix.push_str(&name);
                            prefix.push(']');
                            prefix.push(' ');
                            buf[..prefix.len()].copy_from_slice(&prefix.as_bytes());

                            let bytes = nix::unistd::read(*fd, &mut buf[..]).unwrap();
                            let lines = buf[..bytes].split(|x| *x == b'\n');
                            let mut outbuf: Vec<u8> = Vec::new();

                            for line in lines {
                                if line.is_empty() {
                                    continue;
                                }
                                outbuf.clear();
                                outbuf.extend(prefix.as_bytes());
                                outbuf.extend(line);
                                outbuf.push(b'\n');
                                std::io::stdout().write_all(&outbuf).unwrap();
                            }
                        }
                    }
                }
            }
            Err(e) => {
                warn!("Error while selecting: {}", e);
            }
        }
    }
}

pub fn handle_all_std_err(eventfd: EventFd, unit_table: ArcMutUnitTable) {
    loop {
        // need to collect all again. There might be a newly started service
        let fd_to_srvc_id: HashMap<_, _> =
            unit_table
                .read()
                .unwrap()
                .iter()
                .fold(HashMap::new(), |mut map, (id, srvc_unit)| {
                    let srvc_unit_locked = srvc_unit.lock().unwrap();
                    if let UnitSpecialized::Service(srvc) = &srvc_unit_locked.specialized {
                        if let Some(fd) = &srvc.stderr_dup {
                            map.insert(fd.0, *id);
                        }
                    }
                    map
                });

        let mut fdset = nix::sys::select::FdSet::new();
        for fd in fd_to_srvc_id.keys() {
            fdset.insert(*fd);
        }
        fdset.insert(eventfd.read_end());

        let result = nix::sys::select::select(None, Some(&mut fdset), None, None, None);
        match result {
            Ok(_) => {
                if fdset.contains(eventfd.read_end()) {
                    trace!("Interrupted stderr select because the eventfd fired");
                    reset_event_fd(eventfd);
                    trace!("Reset eventfd value");
                }
                let mut buf = [0u8; 512];
                let unit_table_locked = &*unit_table.read().unwrap();
                for (fd, id) in &fd_to_srvc_id {
                    if fdset.contains(*fd) {
                        if let Some(srvc_unit) = unit_table_locked.get(id) {
                            let srvc_unit_locked = srvc_unit.lock().unwrap();
                            let name = srvc_unit_locked.conf.name();

                            // build the service-unique prefix
                            let mut prefix = String::new();
                            prefix.push('[');
                            prefix.push_str(&name);
                            prefix.push(']');
                            prefix.push_str("[STDERR]");
                            prefix.push(' ');
                            buf[..prefix.len()].copy_from_slice(&prefix.as_bytes());

                            let bytes = nix::unistd::read(*fd, &mut buf[..]).unwrap();
                            let lines = buf[..bytes].split(|x| *x == b'\n');
                            let mut outbuf: Vec<u8> = Vec::new();

                            for line in lines {
                                if line.is_empty() {
                                    continue;
                                }
                                outbuf.clear();
                                outbuf.extend(prefix.as_bytes());
                                outbuf.extend(line);
                                outbuf.push(b'\n');
                                std::io::stderr().write_all(&outbuf).unwrap();
                            }
                        }
                    }
                }
            }
            Err(e) => {
                warn!("Error while selecting: {}", e);
            }
        }
    }
}

pub fn handle_notification_message(msg: &str, srvc: &mut Service, name: &str) {
    // TODO process notification content
    let split: Vec<_> = msg.split('=').collect();
    match split[0] {
        "STATUS" => {
            srvc.status_msgs.push(split[1].to_owned());
            trace!(
                "New status message pushed from service {}: {}",
                name,
                srvc.status_msgs.last().unwrap()
            );
        }
        "READY" => {
            srvc.signaled_ready = true;
        }
        _ => {
            warn!("Unknown notification name{}", split[0]);
        }
    }
}

pub fn handle_notifications_from_buffer(srvc: &mut Service, name: &str) {
    while srvc.notifications_buffer.contains('\n') {
        let (line, rest) = srvc
            .notifications_buffer
            .split_at(srvc.notifications_buffer.find('\n').unwrap());
        let line = line.to_owned();
        srvc.notifications_buffer = rest[1..].to_owned();

        handle_notification_message(&line, srvc, name);
    }
}
