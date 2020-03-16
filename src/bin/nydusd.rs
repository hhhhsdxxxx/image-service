// Copyright 2020 Ant Financial. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
//
// Copyright 2019 Intel Corporation. All Rights Reserved.
//
// SPDX-License-Identifier: (Apache-2.0 AND BSD-3-Clause)

#[macro_use(crate_version, crate_authors)]
extern crate clap;
#[macro_use]
extern crate log;
extern crate config;
extern crate stderrlog;

use std::fs::File;
use std::io::Result;
use std::os::unix::io::{AsRawFd, RawFd};
use std::path::Path;
use std::sync::mpsc::{channel, Receiver};
use std::sync::{Arc, RwLock};
use std::thread;
use std::{convert, error, fmt, io, process};

use libc::EFD_NONBLOCK;

use clap::{App, Arg};
use vm_memory::GuestMemoryMmap;
use vmm_sys_util::eventfd::EventFd;

use fuse::filesystem::FileSystem;
use fuse::server::Server;
use fuse::Error as VhostUserFsError;
use nydus_api::http::start_http_thread;
use nydus_api::http_endpoint::{ApiError, ApiRequest, ApiResponsePayload, DaemonInfo, MountInfo};
use rafs::fs::{Rafs, RafsConfig};
use rafs::storage::oss_backend;
use vfs::vfs::Vfs;
use vhost_rs::descriptor_utils::{Reader, Writer};
use vhost_rs::vhost_user::message::*;
use vhost_rs::vring::{VhostUserBackend, VhostUserDaemon, Vring};

const VIRTIO_F_VERSION_1: u32 = 32;

const QUEUE_SIZE: usize = 1024;
const NUM_QUEUES: usize = 2;

// The guest queued an available buffer for the high priority queue.
const HIPRIO_QUEUE_EVENT: u16 = 0;
// The guest queued an available buffer for the request queue.
const REQ_QUEUE_EVENT: u16 = 1;
// The device has been dropped.
const KILL_EVENT: u16 = 2;

type VhostUserBackendResult<T> = std::result::Result<T, std::io::Error>;

#[derive(Debug)]
enum Error {
    /// Failed to handle event other than input event.
    HandleEventNotEpollIn,
    /// Failed to handle unknown event.
    HandleEventUnknownEvent,
    /// No memory configured.
    NoMemoryConfigured,
    /// Processing queue failed.
    ProcessQueue(VhostUserFsError),
    /// Cannot create epoll context.
    Epoll(io::Error),
    /// Cannot clone event fd.
    EventFdClone(io::Error),
    /// Cannot spawn a new thread
    ThreadSpawn(io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "vhost_user_fs_error: {:?}", self)
    }
}

impl error::Error for Error {}

impl convert::From<Error> for io::Error {
    fn from(e: Error) -> Self {
        io::Error::new(io::ErrorKind::Other, e)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum EpollDispatch {
    Exit,
    Reset,
    Stdin,
    Api,
}

pub struct EpollContext {
    raw_fd: RawFd,
    dispatch_table: Vec<Option<EpollDispatch>>,
}

impl EpollContext {
    pub fn new() -> Result<EpollContext> {
        let raw_fd = epoll::create(true)?;

        // Initial capacity needs to be large enough to hold:
        // * 1 exit event
        // * 1 reset event
        // * 1 stdin event
        // * 1 API event
        let mut dispatch_table = Vec::with_capacity(5);
        dispatch_table.push(None);

        Ok(EpollContext {
            raw_fd,
            dispatch_table,
        })
    }

    fn add_event<T>(&mut self, fd: &T, token: EpollDispatch) -> Result<()>
    where
        T: AsRawFd,
    {
        let dispatch_index = self.dispatch_table.len() as u64;
        epoll::ctl(
            self.raw_fd,
            epoll::ControlOptions::EPOLL_CTL_ADD,
            fd.as_raw_fd(),
            epoll::Event::new(epoll::Events::EPOLLIN, dispatch_index),
        )?;
        self.dispatch_table.push(Some(token));

        Ok(())
    }
}

impl AsRawFd for EpollContext {
    fn as_raw_fd(&self) -> RawFd {
        self.raw_fd
    }
}

struct VhostUserFsBackend<F: FileSystem + Send + Sync + 'static> {
    mem: Option<GuestMemoryMmap>,
    kill_evt: EventFd,
    vfs: Arc<Vfs<F>>,
    server: Arc<Server<Vfs<F>>>,
}

struct ApiServer {
    id: String,
    version: String,
    epoll: EpollContext,
    api_evt: EventFd,
}

impl ApiServer {
    fn new(id: String, version: String, api_evt: EventFd) -> Result<Self> {
        let mut epoll = EpollContext::new().map_err(Error::Epoll)?;
        epoll
            .add_event(&api_evt, EpollDispatch::Api)
            .map_err(Error::Epoll)?;

        Ok(ApiServer {
            id: id,
            version: version,
            epoll: epoll,
            api_evt: api_evt,
        })
    }

    // control loop to handle api requests
    fn control_loop<FF>(&self, api_receiver: Receiver<ApiRequest>, mut mounter: FF) -> Result<()>
    where
        FF: FnMut(MountInfo) -> std::result::Result<ApiResponsePayload, ApiError>,
    {
        const EPOLL_EVENTS_LEN: usize = 100;

        let mut events = vec![epoll::Event::new(epoll::Events::empty(), 0); EPOLL_EVENTS_LEN];
        let epoll_fd = self.epoll.as_raw_fd();

        trace!("api control loop start");
        loop {
            let num_events = match epoll::wait(epoll_fd, -1, &mut events[..]) {
                Ok(res) => res,
                Err(e) => {
                    if e.kind() == io::ErrorKind::Interrupted {
                        // It's well defined from the epoll_wait() syscall
                        // documentation that the epoll loop can be interrupted
                        // before any of the requested events occurred or the
                        // timeout expired. In both those cases, epoll_wait()
                        // returns an error of type EINTR, but this should not
                        // be considered as a regular error. Instead it is more
                        // appropriate to retry, by calling into epoll_wait().
                        continue;
                    }
                    return Err(e);
                }
            };

            trace!("receive api control {} events", num_events);

            for event in events.iter().take(num_events) {
                let dispatch_idx = event.data as usize;

                if let Some(dispatch_type) = self.epoll.dispatch_table[dispatch_idx] {
                    match dispatch_type {
                        EpollDispatch::Api => {
                            // Consume the event.
                            self.api_evt.read()?;

                            // Read from the API receiver channel
                            let api_request = api_receiver.recv().map_err(|e| {
                                error!("receive API channel failed {}", e);
                                io::Error::from(io::ErrorKind::BrokenPipe)
                            })?;

                            match api_request {
                                ApiRequest::DaemonInfo(sender) => {
                                    let response = DaemonInfo {
                                        id: self.id.to_string(),
                                        version: self.version.to_string(),
                                        state: "Running".to_string(),
                                    };

                                    sender
                                        .send(Ok(response).map(ApiResponsePayload::DaemonInfo))
                                        .map_err(|e| {
                                            error!("send API response failed {}", e);
                                            io::Error::from(io::ErrorKind::BrokenPipe)
                                        })?;
                                }
                                ApiRequest::Mount(info, sender) => {
                                    sender.send(mounter(info)).map_err(|e| {
                                        error!("send API response failed {}", e);
                                        io::Error::from(io::ErrorKind::BrokenPipe)
                                    })?;
                                }
                            }
                        }
                        t => {
                            error!("unexpected event type {:?}", t);
                        }
                    }
                }
            }
        }
    }
}

// Start the api server and kick of a local thread to handle
// api requests.
fn start_api_server<FF>(
    id: String,
    version: String,
    http_path: String,
    mounter: FF,
) -> Result<thread::JoinHandle<Result<()>>>
where
    FF: Send + Sync + 'static + Fn(MountInfo) -> std::result::Result<ApiResponsePayload, ApiError>,
{
    let api_evt = EventFd::new(EFD_NONBLOCK).map_err(Error::Epoll)?;
    let http_api_event = api_evt.try_clone().map_err(Error::EventFdClone)?;
    let (api_sender, api_receiver) = channel();

    let thread = thread::Builder::new()
        .name("api_handler".to_string())
        .spawn(move || {
            let s = ApiServer::new(id, version, api_evt)?;
            s.control_loop(api_receiver, mounter)
        })
        .map_err(Error::ThreadSpawn)?;

    // The VMM thread is started, we can start serving HTTP requests
    start_http_thread(&http_path, http_api_event, api_sender)?;

    Ok(thread)
}

impl<F: FileSystem + Send + Sync + 'static> VhostUserFsBackend<F> {
    fn new(vfs: Vfs<F>) -> Result<Self> {
        let fs = Arc::new(vfs);
        Ok(VhostUserFsBackend {
            mem: None,
            kill_evt: EventFd::new(EFD_NONBLOCK).map_err(Error::Epoll)?,
            server: Arc::new(Server::new(Arc::clone(&fs))),
            vfs: Arc::clone(&fs),
        })
    }

    fn process_queue(&mut self, vring: &mut Vring) -> Result<()> {
        let mem = self.mem.as_ref().ok_or(Error::NoMemoryConfigured)?;

        let mut used_desc_heads = [(0, 0); QUEUE_SIZE];
        let mut used_count = 0;
        while let Some(avail_desc) = vring.mut_queue().iter(&mem).next() {
            let head_index = avail_desc.index;
            let reader = Reader::new(&mem, avail_desc.clone()).unwrap();
            let writer = Writer::new(&mem, avail_desc.clone()).unwrap();

            let total = self
                .server
                .handle_message(reader, writer)
                .map_err(Error::ProcessQueue)?;

            used_desc_heads[used_count] = (head_index, total);
            used_count += 1;
        }

        if used_count > 0 {
            for &(desc_index, _) in &used_desc_heads[..used_count] {
                vring.mut_queue().add_used(&mem, desc_index, 0);
            }
            vring.signal_used_queue().unwrap();
        }

        Ok(())
    }
}

impl<F: FileSystem + Send + Sync + 'static> VhostUserBackend for VhostUserFsBackend<F> {
    fn num_queues(&self) -> usize {
        NUM_QUEUES
    }

    fn max_queue_size(&self) -> usize {
        QUEUE_SIZE
    }

    fn features(&self) -> u64 {
        1 << VIRTIO_F_VERSION_1 | VhostUserVirtioFeatures::PROTOCOL_FEATURES.bits()
    }

    fn protocol_features(&self) -> VhostUserProtocolFeatures {
        // liubo: we haven't supported slave req in rafs.
        VhostUserProtocolFeatures::MQ
    }

    fn update_memory(&mut self, mem: GuestMemoryMmap) -> VhostUserBackendResult<()> {
        self.mem = Some(mem);
        Ok(())
    }

    fn handle_event(
        &mut self,
        index: u16,
        evset: epoll::Events,
        vrings: &[Arc<RwLock<Vring>>],
    ) -> VhostUserBackendResult<bool> {
        if evset != epoll::Events::EPOLLIN {
            return Err(Error::HandleEventNotEpollIn.into());
        }

        match index {
            HIPRIO_QUEUE_EVENT => {
                let mut vring = vrings[HIPRIO_QUEUE_EVENT as usize].write().unwrap();
                // high priority requests are also just plain fuse requests, just in a
                // different queue
                self.process_queue(&mut vring)?;
            }
            x if x >= REQ_QUEUE_EVENT && x < vrings.len() as u16 => {
                let mut vring = vrings[x as usize].write().unwrap();
                self.process_queue(&mut vring)?;
            }
            _ => return Err(Error::HandleEventUnknownEvent.into()),
        }

        Ok(false)
    }

    fn exit_event(&self) -> Option<(EventFd, Option<u16>)> {
        Some((self.kill_evt.try_clone().unwrap(), Some(KILL_EVENT)))
    }
}

fn main() -> Result<()> {
    let cmd_arguments = App::new("vhost-user-fs backend")
        .version(crate_version!())
        .author(crate_authors!())
        .about("Launch a vhost-user-fs backend.")
        .arg(
            Arg::with_name("metadata")
                .long("metadata")
                .help("rafs metadata file")
                .takes_value(true)
                .min_values(1),
        )
        .arg(
            Arg::with_name("sock")
                .long("sock")
                .help("vhost-user socket path")
                .takes_value(true)
                .min_values(1),
        )
        .arg(
            Arg::with_name("config")
                .long("config")
                .help("config file")
                .takes_value(true)
                .min_values(1),
        )
        .arg(
            Arg::with_name("apisock")
                .long("apisock")
                .help("admin api socket path")
                .takes_value(true)
                .min_values(1),
        )
        .get_matches();

    // Retrieve arguments
    let config_file = cmd_arguments
        .value_of("config")
        .expect("config file must be provided");
    let sock = cmd_arguments
        .value_of("sock")
        .expect("Failed to retrieve vhost-user socket path");
    let metadata = cmd_arguments.value_of("metadata").unwrap_or_default();
    let apisock = cmd_arguments.value_of("apisock").unwrap_or_default();

    stderrlog::new()
        .quiet(false)
        .verbosity(log::LevelFilter::Trace as usize)
        .timestamp(stderrlog::Timestamp::Second)
        .init()
        .unwrap();

    let mut settings = config::Config::new();
    settings
        .merge(config::File::from(Path::new(config_file)))
        .expect("failed to open config file");
    let rafs_conf: RafsConfig = settings.try_into().expect("Invalid config");

    let vfs: Vfs<Rafs<oss_backend::OSS>> = Vfs::new();
    let fs_backend = Arc::new(RwLock::new(VhostUserFsBackend::new(vfs).unwrap()));

    if metadata != "" {
        let mut rafs = Rafs::new(rafs_conf.clone(), oss_backend::new());
        let mut file = File::open(metadata)?;
        rafs.import(&mut file)?;
        info!("rafs mounted");
        let fs = Arc::clone(&fs_backend.write().unwrap().vfs);
        fs.mount(rafs, "/").unwrap();
        info!("vfs mounted");
    }

    if apisock != "" {
        let backend = Arc::clone(&fs_backend);
        start_api_server(
            "nydusd".to_string(),
            env!("CARGO_PKG_VERSION").to_string(),
            apisock.to_string(),
            move |info| {
                let mut rafs = Rafs::new(rafs_conf.clone(), oss_backend::new());
                let mut file = File::open(&info.source).map_err(ApiError::MountFailure)?;
                rafs.import(&mut file).map_err(ApiError::MountFailure)?;
                info!("rafs mounted");
                let vfs = Arc::clone(&backend.write().unwrap().vfs);

                match vfs.mount(rafs, &info.mountpoint) {
                    Ok(()) => Ok(ApiResponsePayload::Mount),
                    Err(e) => {
                        error!("mount {:?} failed {}", info, e);
                        Err(ApiError::MountFailure(io::Error::from(
                            io::ErrorKind::InvalidData,
                        )))
                    }
                }
            },
        )?;
        info!("api server running at {}", apisock);
    }

    let mut daemon = VhostUserDaemon::new(
        String::from("vhost-user-fs-backend"),
        String::from(sock),
        fs_backend.clone(),
    )
    .unwrap();

    info!("starting fuse daemon");
    if let Err(e) = daemon.start() {
        error!("Failed to start daemon: {:?}", e);
        process::exit(1);
    }

    if let Err(e) = daemon.wait() {
        error!("Waiting for daemon failed: {:?}", e);
    }

    let kill_evt = &fs_backend.read().unwrap().kill_evt;
    if let Err(e) = kill_evt.write(1) {
        error!("Error shutting down worker thread: {:?}", e)
    }

    info!("nydusd quits");
    Ok(())
}