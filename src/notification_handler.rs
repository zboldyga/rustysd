use crate::sockets::Socket;
use crate::units::*;
use std::collections::HashMap;
use std::io::Read;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};

fn handle_stream_mut(stream: &mut UnixStream, id: InternalId, service_table: Arc<Mutex<HashMap<InternalId, Unit>>>) {
    loop {
        let mut buf = [0u8; 512];
        let bytes = stream.read(&mut buf[..]).unwrap();
        
        if bytes == 0 {
            let service_table: &HashMap<_, _> = &service_table.lock().unwrap();
            let srvc_unit = service_table.get(&id).unwrap();
            trace!(
                " [Notification-Listener] Service: {} closed a notification connection",
                srvc_unit.conf.name(),
            );
            break;
        }
        {
            let service_table: &HashMap<_, _> = &service_table.lock().unwrap();
            let srvc_unit = service_table.get(&id).unwrap();
            trace!(
                " [Notification-Listener] Service: {} sent notification: {}",
                srvc_unit.conf.name(),
                String::from_utf8(Vec::from(&buf[..bytes])).unwrap(),
            );

            // TODO process notification content
        }
    }
}

pub fn handle_stream(mut stream: UnixStream, id: InternalId, service_table: Arc<Mutex<HashMap<InternalId, Unit>>>) {
    std::thread::spawn(move || {
        handle_stream_mut(&mut stream, id, service_table);
    });
}

pub fn handle_notifications(
    _socket_table: Arc<Mutex<HashMap<String, Socket>>>,
    service_table: Arc<Mutex<HashMap<InternalId, Unit>>>,
    _pid_table: Arc<Mutex<HashMap<u32, InternalId>>>,
) {
    std::thread::spawn(move || {
        // setup the list to listen to
        let mut select_vec = Vec::new();
        {
            let service_table_locked: &HashMap<_, _> = &service_table.lock().unwrap();
            for (_name, srvc_unit) in service_table_locked {
                if let UnitSpecialized::Service(srvc) = &srvc_unit.specialized {
                    if let Some(sock) = &srvc.notify_access_socket {
                        select_vec.push((srvc_unit.conf.name(), srvc_unit.id, sock.clone()));
                    }
                }
            }
        }

        loop {
            // take refs from the Arc's
            let select_vec: Vec<_> = select_vec
                .iter()
                .map(|(n, id, x)| ((n.clone(), id), x.as_ref()))
                .collect();
            let streams = crate::unix_listener_select::select(&select_vec, None).unwrap();
            for ((name, id), (stream, _addr)) in streams {
                trace!(
                    " [Notification-Listener] Service: {} has connected on the notification socket",
                    name
                );

                // TODO check notification-access setting for pid an such
                {
                    handle_stream(stream, *id, service_table.clone());
                }
            }
        }
    });
}
