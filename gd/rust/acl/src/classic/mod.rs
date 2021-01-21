//! Classic ACL manager

use crate::core;
use bt_common::Bluetooth;
use bt_hci::{Address, CommandSender, EventRegistry};
use bt_packets::hci::EventChild::{
    AuthenticationComplete, ConnectionComplete, DisconnectionComplete,
};
use bt_packets::hci::{
    AcceptConnectionRequestBuilder, AcceptConnectionRequestRole, ClockOffsetValid,
    CreateConnectionBuilder, CreateConnectionCancelBuilder, CreateConnectionRoleSwitch,
    DisconnectBuilder, DisconnectReason, ErrorCode, EventChild, EventCode, EventPacket,
    PageScanRepetitionMode, RejectConnectionReason, RejectConnectionRequestBuilder, Role,
};
use bytes::Bytes;
use gddi::{module, provides, Stoppable};
use log::warn;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::runtime::Runtime;
use tokio::select;
use tokio::sync::mpsc::{channel, Receiver, Sender};
use tokio::sync::{oneshot, Mutex};

module! {
    classic_acl_module,
    providers {
        AclManager => provide_acl_manager,
    },
}

/// Classic ACL manager
#[derive(Clone, Stoppable)]
pub struct AclManager {
    req_tx: Sender<Request>,
    /// High level events from AclManager
    pub evt_rx: Arc<Mutex<Receiver<Event>>>,
}

/// Events generated by AclManager
#[derive(Debug)]
pub enum Event {
    /// Connection was successful - provides the newly created connection
    ConnectSuccess(Connection),
    /// Locally initialted connection was not successful - indicates address & reason
    ConnectFail {
        /// Address of the failed connection
        addr: Address,
        /// Reason of the failed connection
        reason: ErrorCode,
    },
}

/// A classic ACL connection
#[derive(Debug)]
pub struct Connection {
    addr: Address,
    rx: Receiver<Bytes>,
    tx: Sender<Bytes>,
    shared: Arc<Mutex<ConnectionShared>>,
    requests: Sender<ConnectionRequest>,
    evt_rx: Receiver<ConnectionEvent>,
}

/// Events generated by Connection
#[derive(Debug)]
pub enum ConnectionEvent {
    /// Connection was disconnected with the specified code.
    Disconnected(ErrorCode),
    /// Connection authentication was completed
    AuthenticationComplete,
}

impl Connection {
    /// Disconnect the connection with the specified reason.
    pub async fn disconnect(&mut self, reason: DisconnectReason) {
        let (tx, rx) = oneshot::channel();
        self.requests.send(ConnectionRequest::Disconnect { reason, fut: tx }).await.unwrap();
        rx.await.unwrap()
    }
}

#[derive(Debug)]
enum ConnectionRequest {
    Disconnect { reason: DisconnectReason, fut: oneshot::Sender<()> },
}

struct ConnectionInternal {
    addr: Address,
    #[allow(dead_code)]
    shared: Arc<Mutex<ConnectionShared>>,
    hci_evt_tx: Sender<EventPacket>,
}

#[derive(Debug)]
struct ConnectionShared {
    role: Role,
}

impl AclManager {
    /// Connect to the specified address, or queue it if a connection is already pending
    pub async fn connect(&mut self, addr: Address) {
        self.req_tx.send(Request::Connect { addr }).await.unwrap();
    }

    /// Cancel the connection to the specified address, if it is pending
    pub async fn cancel_connect(&mut self, addr: Address) {
        let (tx, rx) = oneshot::channel();
        self.req_tx.send(Request::CancelConnect { addr, fut: tx }).await.unwrap();
        rx.await.unwrap();
    }
}

#[derive(Debug)]
enum Request {
    Connect { addr: Address },
    CancelConnect { addr: Address, fut: oneshot::Sender<()> },
}

#[derive(Eq, PartialEq)]
enum PendingConnect {
    Outgoing(Address),
    Incoming(Address),
    None,
}

impl PendingConnect {
    fn take(&mut self) -> Self {
        std::mem::replace(self, PendingConnect::None)
    }
}

#[provides]
async fn provide_acl_manager(
    mut hci: CommandSender,
    mut events: EventRegistry,
    mut dispatch: core::AclDispatch,
    rt: Arc<Runtime>,
) -> AclManager {
    let (req_tx, mut req_rx) = channel::<Request>(10);
    let (conn_evt_tx, conn_evt_rx) = channel::<Event>(10);
    let local_rt = rt.clone();

    local_rt.spawn(async move {
        let connections: Arc<Mutex<HashMap<u16, ConnectionInternal>>> = Arc::new(Mutex::new(HashMap::new()));
        let mut connect_queue: Vec<Address> = Vec::new();
        let mut pending = PendingConnect::None;

        let (evt_tx, mut evt_rx) = channel(3);
        events.register(EventCode::ConnectionComplete, evt_tx.clone()).await;
        events.register(EventCode::ConnectionRequest, evt_tx.clone()).await;
        events.register(EventCode::AuthenticationComplete, evt_tx).await;

        loop {
            select! {
                Some(req) = req_rx.recv() => {
                    match req {
                        Request::Connect { addr } => {
                            if connections.lock().await.values().any(|c| c.addr == addr) {
                                warn!("already connected: {}", addr);
                                return;
                            }
                            if let PendingConnect::None = pending {
                                pending = PendingConnect::Outgoing(addr);
                                hci.send(build_create_connection(addr)).await;
                            } else {
                                connect_queue.insert(0, addr);
                            }
                        },
                        Request::CancelConnect { addr, fut } => {
                            connect_queue.retain(|p| *p != addr);
                            if pending == PendingConnect::Outgoing(addr) {
                                hci.send(CreateConnectionCancelBuilder { bd_addr: addr }).await;
                            }
                            fut.send(()).unwrap();
                        }
                    }
                }
                Some(evt) = evt_rx.recv() => {
                    match evt.specialize() {
                        ConnectionComplete(evt) => {
                            let addr = evt.get_bd_addr();
                            let status = evt.get_status();
                            let handle = evt.get_connection_handle();
                            let role = match pending.take() {
                                PendingConnect::Outgoing(a) if a == addr => Role::Central,
                                PendingConnect::Incoming(a) if a == addr => Role::Peripheral,
                                _ => panic!("No prior connection request for {}", addr),
                            };

                            match status {
                                ErrorCode::Success => {
                                    let mut core_conn = dispatch.register(handle, Bluetooth::Classic).await;
                                    let shared = Arc::new(Mutex::new(ConnectionShared { role }));
                                    let (evt_tx, evt_rx) = channel(10);
                                    let (req_tx, req_rx) = channel(10);
                                    let connection = Connection {
                                        addr,
                                        shared: shared.clone(),
                                        rx: core_conn.rx.take().unwrap(),
                                        tx: core_conn.tx.take().unwrap(),
                                        requests: req_tx,
                                        evt_rx,
                                    };
                                    let connection_internal = ConnectionInternal {
                                        addr,
                                        shared,
                                        hci_evt_tx: core_conn.evt_tx.clone(),
                                    };

                                    assert!(connections.lock().await.insert(handle, connection_internal).is_none());
                                    rt.spawn(run_connection(handle, evt_tx, req_rx, core_conn, connections.clone(), hci.clone()));
                                    conn_evt_tx.send(Event::ConnectSuccess(connection)).await.unwrap();
                                },
                                _ => conn_evt_tx.send(Event::ConnectFail { addr, reason: status }).await.unwrap(),
                            }
                        },
                        EventChild::ConnectionRequest(evt) => {
                            let addr = evt.get_bd_addr();
                            pending = PendingConnect::Incoming(addr);
                            if connections.lock().await.values().any(|c| c.addr == addr) {
                                hci.send(RejectConnectionRequestBuilder {
                                    bd_addr: addr,
                                    reason: RejectConnectionReason::UnacceptableBdAddr
                                }).await;
                            } else {
                                hci.send(AcceptConnectionRequestBuilder {
                                    bd_addr: addr,
                                    role: AcceptConnectionRequestRole::BecomeCentral
                                }).await;
                            }
                        },
                        AuthenticationComplete(e) => dispatch_to(e.get_connection_handle(), &connections, evt).await,
                        _ => unimplemented!(),
                    }
                }
            }
        }
    });

    AclManager { req_tx, evt_rx: Arc::new(Mutex::new(conn_evt_rx)) }
}

fn build_create_connection(bd_addr: Address) -> CreateConnectionBuilder {
    CreateConnectionBuilder {
        bd_addr,
        packet_type: 0x4408 /* DM 1,3,5 */ | 0x8810, /*DH 1,3,5 */
        page_scan_repetition_mode: PageScanRepetitionMode::R1,
        clock_offset: 0,
        clock_offset_valid: ClockOffsetValid::Invalid,
        allow_role_switch: CreateConnectionRoleSwitch::AllowRoleSwitch,
    }
}

async fn dispatch_to(
    handle: u16,
    connections: &Arc<Mutex<HashMap<u16, ConnectionInternal>>>,
    event: EventPacket,
) {
    if let Some(c) = connections.lock().await.get_mut(&handle) {
        c.hci_evt_tx.send(event).await.unwrap();
    }
}

async fn run_connection(
    handle: u16,
    evt_tx: Sender<ConnectionEvent>,
    mut req_rx: Receiver<ConnectionRequest>,
    mut core: core::Connection,
    connections: Arc<Mutex<HashMap<u16, ConnectionInternal>>>,
    mut hci: CommandSender,
) {
    loop {
        select! {
            Some(evt) = core.evt_rx.recv() => {
                match evt.specialize() {
                    DisconnectionComplete(evt) => {
                        connections.lock().await.remove(&handle);
                        evt_tx.send(ConnectionEvent::Disconnected(evt.get_reason())).await.unwrap();
                        return; // At this point, there is nothing more to run on the connection.
                    },
                    AuthenticationComplete(_) => evt_tx.send(ConnectionEvent::AuthenticationComplete).await.unwrap(),
                    _ => unimplemented!(),
                }
            },
            Some(req) = req_rx.recv() => {
                match req {
                    ConnectionRequest::Disconnect{reason, fut} => {
                        hci.send(DisconnectBuilder { connection_handle: handle, reason }).await;
                        fut.send(()).unwrap();
                    }
                }
            },
        }
    }
}
