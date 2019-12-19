//! Activate units (recursively and parallel along the dependency tree)

use super::units::*;
use std::collections::HashMap;
use std::os::unix::io::RawFd;
use std::sync::Arc;
use std::sync::Mutex;
use threadpool::ThreadPool;

fn activate_units_recursive(
    ids_to_start: Vec<InternalId>,
    started_ids: Arc<Mutex<Vec<InternalId>>>,
    unit_table: ArcMutUnitTable,
    pids: ArcMutPidTable,
    tpool: ThreadPool,
    notification_socket_path: std::path::PathBuf,
    eventfds: Arc<Vec<RawFd>>,
) {
    for id in ids_to_start {
        let started_ids_copy = started_ids.clone();
        let unit_table_copy = unit_table.clone();
        let pids_copy = pids.clone();
        let tpool_copy = tpool.clone();
        let note_sock_copy = notification_socket_path.clone();
        let eventfds_copy = eventfds.clone();

        tpool.execute(move || {
            let started_ids_copy2 = started_ids_copy.clone();
            let unit_table_copy2 = unit_table_copy.clone();
            let pids_copy2 = pids_copy.clone();
            let tpool_copy2 = tpool_copy.clone();
            let note_sock_copy2 = note_sock_copy.clone();
            let eventfds_copy2 = eventfds_copy.clone();

            match activate_unit(
                id,
                Some(started_ids_copy),
                unit_table_copy,
                pids_copy,
                note_sock_copy,
                eventfds_copy,
                false,
            ) {
                Ok(StartResult::Started(next_services_ids)) => {
                    {
                        let mut started_ids_locked = started_ids_copy2.lock().unwrap();
                        started_ids_locked.push(id);
                    }

                    let next_services_job = move || {
                        activate_units_recursive(
                            next_services_ids,
                            started_ids_copy2,
                            unit_table_copy2,
                            pids_copy2,
                            tpool_copy2,
                            note_sock_copy2,
                            eventfds_copy2,
                        );
                    };
                    tpool_copy.execute(next_services_job);
                }
                Ok(StartResult::Ignored) => {
                    // Thats ok
                }
                Err(e) => {
                    panic!("Error while activating unit {}", e);
                }
            }
        });
    }
}

pub enum StartResult {
    Started(Vec<InternalId>),
    Ignored,
}

pub fn activate_unit(
    id_to_start: InternalId,
    started_ids: Option<Arc<Mutex<Vec<InternalId>>>>,
    unit_table: ArcMutUnitTable,
    pids: ArcMutPidTable,
    notification_socket_path: std::path::PathBuf,
    eventfds: Arc<Vec<RawFd>>,
    by_socket_activation: bool,
) -> std::result::Result<StartResult, std::string::String> {
    trace!("Activate id: {}", id_to_start);

    // first lock
    // 1) the unit itself
    // 2) the needed sockets if it is a service unit
    // this all needs to happen under the unit_table lock because there is a deadlock
    // hazard when taking the unit_table lock after already holding a unit lock
    let mut socket_units = HashMap::new();
    let mut socket_units_locked = HashMap::new();
    let mut socket_units_refs = HashMap::new();
    let unit = {
        let units_locked = unit_table.read().unwrap();
        let unit = match units_locked.get(&id_to_start) {
            Some(unit) => Arc::clone(unit),
            None => {
                panic!("Tried to run a unit that has been removed from the map");
            }
        };
        {
            let unit_locked = unit.lock().unwrap();

            if let Some(started_ids) = started_ids {
                let started_ids_locked = started_ids.lock().unwrap();

                // if not all dependencies are yet started ignore this call. THis unit will be activated again when
                // the next dependency gets ready
                let all_deps_ready = unit_locked
                    .install
                    .after
                    .iter()
                    .fold(true, |acc, elem| acc && started_ids_locked.contains(elem));
                if !all_deps_ready {
                    trace!(
                        "Unit: {} ignores activation. Not all dependencies have been started",
                        unit_locked.conf.name()
                    );
                    return Ok(StartResult::Ignored);
                }
            }

            let name = unit_locked.conf.name();
            trace!("Lock required units for unit {}", name);
            socket_units.extend(unit_locked.filter_units_needed_for_activation(&units_locked));

            for (id, unit) in &socket_units {
                trace!("Lock unit: {}", id);
                let unit_locked = unit.lock().unwrap();
                trace!("Locked unit: {}", id);
                socket_units_locked.insert(*id, unit_locked);
            }
            for (id, unit_locked) in &socket_units_locked {
                let unit_ref: &Unit = &(*unit_locked);
                socket_units_refs.insert(*id, unit_ref);
            }
            trace!("Done locking required units for unit {}", name);
        }
        unit
    };

    let unit_locked = &mut *unit.lock().unwrap();
    let next_services_ids = unit_locked.install.before.clone();

    unit_locked
        .activate(
            &socket_units_refs,
            pids.clone(),
            notification_socket_path.clone(),
            &eventfds,
            by_socket_activation,
        )
        .map(|_| StartResult::Started(next_services_ids))
        .map_err(|e| {
            format!(
                "Error while starting unit {}: {}",
                unit_locked.conf.name(),
                e
            )
        })
}

pub fn activate_units(
    unit_table: ArcMutUnitTable,
    notification_socket_path: std::path::PathBuf,
    eventfds: Vec<RawFd>,
    pid_table: ArcMutPidTable,
) {
    let mut root_units = Vec::new();

    for (id, unit) in &*unit_table.read().unwrap() {
        let unit_locked = unit.lock().unwrap();
        if unit_locked.install.after.is_empty() {
            root_units.push(*id);
            trace!("Root unit: {}", unit_locked.conf.name());
        }
    }

    let tpool = ThreadPool::new(6);
    let eventfds_arc = Arc::new(eventfds);
    let started_ids = Arc::new(Mutex::new(Vec::new()));
    activate_units_recursive(
        root_units,
        started_ids,
        Arc::clone(&unit_table),
        Arc::clone(&pid_table),
        tpool.clone(),
        notification_socket_path,
        eventfds_arc,
    );

    tpool.join();
}
