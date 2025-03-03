// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    crate::{
        filesystem::{mkfs, FxFilesystem, OpenFxFilesystem, OpenOptions},
        fsck,
        log::*,
        object_store::volume::root_volume,
        platform::{
            fuchsia::{errors::map_to_status, volumes_directory::VolumesDirectory},
            RemoteCrypt,
        },
    },
    anyhow::{Context, Error},
    fidl::endpoints::{ClientEnd, DiscoverableProtocolMarker, RequestStream},
    fidl_fuchsia_fs::{AdminMarker, AdminRequest, AdminRequestStream},
    fidl_fuchsia_fs_startup::{
        CheckOptions, StartOptions, StartupMarker, StartupRequest, StartupRequestStream,
    },
    fidl_fuchsia_fxfs::{CryptProxy, VolumesMarker, VolumesRequest, VolumesRequestStream},
    fidl_fuchsia_hardware_block::BlockMarker,
    fidl_fuchsia_io as fio,
    fidl_fuchsia_process_lifecycle::{LifecycleRequest, LifecycleRequestStream},
    fuchsia_async as fasync, fuchsia_zircon as zx,
    futures::lock::Mutex,
    futures::TryStreamExt,
    inspect_runtime::service::{TreeServerSendPreference, TreeServerSettings},
    remote_block_device::RemoteBlockClient,
    std::ops::Deref,
    std::sync::Arc,
    storage_device::{block_device::BlockDevice, DeviceHolder},
    vfs::{
        directory::{entry::DirectoryEntry, helper::DirectlyMutable},
        execution_scope::ExecutionScope,
        path::Path,
    },
};

pub fn map_to_raw_status(e: Error) -> zx::sys::zx_status_t {
    map_to_status(e).into_raw()
}

/// Runs Fxfs as a component.
pub struct Component {
    state: futures::lock::Mutex<State>,

    // The execution scope of the pseudo filesystem.
    scope: ExecutionScope,

    // The root of the pseudo filesystem for the component.
    outgoing_dir: Arc<vfs::directory::immutable::Simple>,
}

enum State {
    ComponentStarted,
    Running(OpenFxFilesystem, Arc<VolumesDirectory>),
}

impl State {
    async fn stop(&mut self, outgoing_dir: &vfs::directory::immutable::Simple) {
        if let State::Running(fs, volumes) = std::mem::replace(self, State::ComponentStarted) {
            info!("Stopping Fxfs runtime; remaining connections will be forcibly closed");
            let _ = outgoing_dir
                .remove_entry_impl("volumes".into(), /* must_be_directory: */ false);
            volumes.terminate().await;
            let _ = fs.close().await;
        }
    }
}

impl Component {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State::ComponentStarted),
            scope: ExecutionScope::new(),
            outgoing_dir: vfs::directory::immutable::simple(),
        })
    }

    /// Runs Fxfs as a component.
    pub async fn run(
        self: Arc<Self>,
        outgoing_dir: zx::Channel,
        lifecycle_channel: Option<zx::Channel>,
    ) -> Result<(), Error> {
        self.outgoing_dir
            .add_entry(
                "diagnostics",
                inspect_runtime::create_diagnostics_dir_with_options(
                    fuchsia_inspect::component::inspector().clone(),
                    TreeServerSettings {
                        send_vmo_preference: TreeServerSendPreference::frozen_or(
                            TreeServerSendPreference::DeepCopy,
                        ),
                    },
                ),
            )
            .expect("unable to create diagnostics dir");

        let svc_dir = vfs::directory::immutable::simple();
        self.outgoing_dir.add_entry("svc", svc_dir.clone()).expect("Unable to create svc dir");
        let weak = Arc::downgrade(&self);
        svc_dir.add_entry(
            StartupMarker::PROTOCOL_NAME,
            vfs::service::host(move |requests| {
                let weak = weak.clone();
                async move {
                    if let Some(me) = weak.upgrade() {
                        let _ = me.handle_startup_requests(requests).await;
                    }
                }
            }),
        )?;
        let weak = Arc::downgrade(&self);
        svc_dir.add_entry(
            VolumesMarker::PROTOCOL_NAME,
            vfs::service::host(move |requests| {
                let weak = weak.clone();
                async move {
                    if let Some(me) = weak.upgrade() {
                        me.handle_volumes_requests(requests).await;
                    }
                }
            }),
        )?;

        let weak = Arc::downgrade(&self);
        svc_dir.add_entry(
            AdminMarker::PROTOCOL_NAME,
            vfs::service::host(move |requests| {
                let weak = weak.clone();
                async move {
                    if let Some(me) = weak.upgrade() {
                        let _ = me.handle_admin_requests(requests).await;
                    }
                }
            }),
        )?;

        self.outgoing_dir.clone().open(
            self.scope.clone(),
            fio::OpenFlags::RIGHT_READABLE | fio::OpenFlags::RIGHT_WRITABLE,
            0,
            Path::dot(),
            outgoing_dir.into(),
        );

        if let Some(channel) = lifecycle_channel {
            let me = self.clone();
            self.scope.spawn(async move {
                if let Err(e) = me.handle_lifecycle_requests(channel).await {
                    warn!(error = e.as_value(), "handle_lifecycle_requests");
                }
            });
        }

        self.scope.wait().await;

        Ok(())
    }

    async fn handle_startup_requests(&self, mut stream: StartupRequestStream) -> Result<(), Error> {
        while let Some(request) = stream.try_next().await? {
            match request {
                StartupRequest::Start { responder, device, options } => {
                    responder.send(&mut self.handle_start(device, options).await.map_err(|e| {
                        error!(error = e.as_value(), "handle_start failed");
                        map_to_raw_status(e)
                    }))?
                }
                StartupRequest::Format { responder, device, .. } => {
                    responder.send(&mut self.handle_format(device).await.map_err(|e| {
                        error!(error = e.as_value(), "handle_format failed");
                        map_to_raw_status(e)
                    }))?
                }
                StartupRequest::Check { responder, device, options } => {
                    responder.send(&mut self.handle_check(device, options).await.map_err(|e| {
                        error!(error = e.as_value(), "handle_check failed");
                        map_to_raw_status(e)
                    }))?
                }
            }
        }
        Ok(())
    }

    async fn handle_start(
        &self,
        device: ClientEnd<BlockMarker>,
        options: StartOptions,
    ) -> Result<(), Error> {
        info!("Received start request");
        let mut state = self.state.lock().await;
        // TODO(fxbug.dev/93066): This is not very graceful.  It would be better for the client to
        // explicitly shut down all volumes first, and make this fail if there are remaining active
        // connections.  Fix the bug in fs_test which requires this.
        state.stop(&self.outgoing_dir).await;
        let client = RemoteBlockClient::new(device.into_channel()).await?;
        let fs = FxFilesystem::open_with_options(
            DeviceHolder::new(BlockDevice::new(Box::new(client), options.read_only).await?),
            OpenOptions::read_only(options.read_only),
        )
        .await?;
        let volumes = VolumesDirectory::new(root_volume(&fs).await?).await?;

        self.outgoing_dir.add_entry_impl(
            "volumes".to_string(),
            volumes.directory_node().clone(),
            /* overwrite: */ true,
        )?;

        *state = State::Running(fs, volumes);
        info!("Mounted");
        Ok(())
    }

    async fn handle_format(&self, device: ClientEnd<BlockMarker>) -> Result<(), Error> {
        let device = DeviceHolder::new(
            BlockDevice::new(
                Box::new(RemoteBlockClient::new(device.into_channel()).await?),
                /* read_only: */ false,
            )
            .await?,
        );
        mkfs(device).await?;
        info!("Formatted filesystem");
        Ok(())
    }

    async fn handle_check(
        &self,
        device: ClientEnd<BlockMarker>,
        options: CheckOptions,
    ) -> Result<(), Error> {
        let state = self.state.lock().await;
        let (fs_container, fs) = match *state {
            State::ComponentStarted => {
                let client = RemoteBlockClient::new(device.into_channel()).await?;
                let fs_container = FxFilesystem::open_with_options(
                    DeviceHolder::new(
                        BlockDevice::new(Box::new(client), /* read_only: */ true).await?,
                    ),
                    OpenOptions::read_only(true),
                )
                .await?;
                let fs = fs_container.clone();
                (Some(fs_container), fs)
            }
            State::Running(ref fs, ..) => (None, fs.deref().clone()),
        };
        let fsck_options = fsck::default_options();
        let crypt = Arc::new(RemoteCrypt::new(CryptProxy::new(fasync::Channel::from_channel(
            options.crypt.ok_or(zx::Status::INVALID_ARGS)?.into_channel(),
        )?)));
        let res = fsck::fsck_with_options(&fs, Some(crypt), fsck_options).await;
        if let Some(fs_container) = fs_container {
            let _ = fs_container.close().await;
        }
        res
    }

    async fn handle_admin_requests(&self, mut stream: AdminRequestStream) -> Result<(), Error> {
        while let Some(request) = stream.try_next().await.context("Reading request")? {
            if self.handle_admin(request).await? {
                break;
            }
        }
        Ok(())
    }

    // Returns true if we should close the connection.
    async fn handle_admin(&self, req: AdminRequest) -> Result<bool, Error> {
        match req {
            AdminRequest::Shutdown { responder } => {
                info!("Received shutdown request");
                self.shutdown().await;
                responder
                    .send()
                    .unwrap_or_else(|e| warn!("Failed to send shutdown response: {}", e));
                return Ok(true);
            }
        }
    }

    async fn shutdown(&self) {
        self.state.lock().await.stop(&self.outgoing_dir).await;
        info!("Filesystem terminated");
    }

    async fn handle_volumes_requests(&self, mut stream: VolumesRequestStream) {
        let volumes = if let State::Running(_, volumes) = &*self.state.lock().await {
            volumes.clone()
        } else {
            let _ = stream.into_inner().0.shutdown_with_epitaph(zx::Status::BAD_STATE);
            return;
        };
        while let Ok(Some(request)) = stream.try_next().await {
            match request {
                VolumesRequest::Create { name, crypt, outgoing_directory, responder } => {
                    info!(
                        name = name.as_str(),
                        "Create {}volume",
                        if crypt.is_some() { "encrypted " } else { "" }
                    );
                    responder
                        .send(
                            &mut volumes
                                .create_and_serve_volume(
                                    &name,
                                    crypt,
                                    outgoing_directory.into_channel().into(),
                                )
                                .await
                                .map_err(map_to_raw_status),
                        )
                        .unwrap_or_else(|e| {
                            warn!(error = e.as_value(), "Failed to send volume creation response")
                        });
                }
                VolumesRequest::Remove { name, responder } => {
                    info!(name = name.as_str(), "Remove volume");
                    responder
                        .send(&mut volumes.remove_volume(&name).await.map_err(map_to_raw_status))
                        .unwrap_or_else(|e| {
                            warn!(error = e.as_value(), "Failed to send volume removal response")
                        });
                }
            }
        }
    }

    async fn handle_lifecycle_requests(&self, lifecycle_channel: zx::Channel) -> Result<(), Error> {
        let mut stream =
            LifecycleRequestStream::from_channel(fasync::Channel::from_channel(lifecycle_channel)?);
        match stream.try_next().await.context("Reading request")? {
            Some(LifecycleRequest::Stop { .. }) => {
                info!("Received Lifecycle::Stop request");
                self.shutdown().await;
            }
            None => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use {
        super::Component,
        crate::{filesystem::FxFilesystem, object_store::volume::root_volume},
        fidl::{
            encoding::Decodable,
            endpoints::{Proxy, ServerEnd},
        },
        fidl_fuchsia_fs::AdminMarker,
        fidl_fuchsia_fs_startup::{StartOptions, StartupMarker},
        fidl_fuchsia_fxfs::VolumesMarker,
        fidl_fuchsia_io as fio,
        fidl_fuchsia_process_lifecycle::{LifecycleMarker, LifecycleProxy},
        fuchsia_async as fasync,
        fuchsia_component::client::connect_to_protocol_at_dir_svc,
        fuchsia_fs::directory::readdir,
        fuchsia_zircon as zx,
        futures::future::{BoxFuture, FusedFuture},
        futures::{future::FutureExt, pin_mut, select},
        ramdevice_client::{wait_for_device, RamdiskClientBuilder},
        remote_block_device::RemoteBlockClient,
        std::{collections::HashSet, pin::Pin},
        storage_device::block_device::BlockDevice,
        storage_device::DeviceHolder,
    };

    async fn run_test(
        callback: impl Fn(&fio::DirectoryProxy, LifecycleProxy) -> BoxFuture<'static, ()>,
    ) -> Pin<Box<impl FusedFuture>> {
        const WAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
        wait_for_device("/dev/sys/platform/00:00:2d/ramctl", WAIT_TIMEOUT)
            .expect("ramctl did not appear");

        let ramdisk =
            RamdiskClientBuilder::new(512, 16384).build().expect("Failed to build ramdisk");

        {
            let fs = FxFilesystem::new_empty(DeviceHolder::new(
                BlockDevice::new(
                    Box::new(
                        RemoteBlockClient::new(ramdisk.open().expect("Unable to open ramdisk"))
                            .await
                            .expect("Unable to create block client"),
                    ),
                    false,
                )
                .await
                .unwrap(),
            ))
            .await
            .expect("FxFilesystem::new_empty failed");
            {
                let root_volume = root_volume(&fs).await.expect("Open root_volume failed");
                root_volume.new_volume("default", None).await.expect("Create volume failed");
            }
            fs.close().await.expect("close failed");
        }

        let (client_end, server_end) =
            fidl::endpoints::create_proxy::<fio::DirectoryMarker>().unwrap();

        let (lifecycle_client, lifecycle_server) =
            fidl::endpoints::create_proxy::<LifecycleMarker>().unwrap();

        let mut component_task = Box::pin(
            async {
                Component::new()
                    .run(server_end.into_channel(), Some(lifecycle_server.into_channel()))
                    .await
                    .expect("Failed to run component");
            }
            .fuse(),
        );

        let startup_proxy = connect_to_protocol_at_dir_svc::<StartupMarker>(&client_end)
            .expect("Unable to connect to Startup protocol");
        let task = async {
            startup_proxy
                .start(
                    ramdisk.open().expect("Unable to open ramdisk").into(),
                    &mut StartOptions::new_empty(),
                )
                .await
                .expect("Start failed (FIDL)")
                .expect("Start failed");
            callback(&client_end, lifecycle_client).await;
        }
        .fuse();

        pin_mut!(task);

        loop {
            select! {
                () = component_task => {},
                () = task => break,
            }
        }

        component_task
    }

    #[fasync::run(2, test)]
    async fn test_shutdown() {
        let component_task = run_test(|client, _| {
            let admin_proxy = connect_to_protocol_at_dir_svc::<AdminMarker>(client)
                .expect("Unable to connect to Admin protocol");
            async move {
                admin_proxy.shutdown().await.expect("shutdown failed");
            }
            .boxed()
        })
        .await;
        assert!(!component_task.is_terminated());
    }

    #[fasync::run(2, test)]
    async fn test_lifecycle_stop() {
        let component_task = run_test(|_, lifecycle_client| {
            lifecycle_client.stop().expect("Stop failed");
            async move {
                fasync::OnSignals::new(
                    &lifecycle_client.into_channel().expect("into_channel failed"),
                    zx::Signals::CHANNEL_PEER_CLOSED,
                )
                .await
                .expect("OnSignals failed");
            }
            .boxed()
        })
        .await;
        component_task.await;
    }

    #[fasync::run(2, test)]
    async fn test_create_and_remove() {
        run_test(|client, _| {
            let volumes_proxy = connect_to_protocol_at_dir_svc::<VolumesMarker>(client)
                .expect("Unable to connect to Volumes protocol");

            let fs_admin_proxy = connect_to_protocol_at_dir_svc::<AdminMarker>(client)
                .expect("Unable to connect to Admin protocol");

            async move {
                let (dir_proxy, server_end) =
                    fidl::endpoints::create_proxy::<fio::DirectoryMarker>()
                        .expect("create_proxy failed");
                volumes_proxy
                    .create("test", None, server_end)
                    .await
                    .expect("fidl failed")
                    .expect("create failed");

                // This should fail whilst the volume is mounted.
                volumes_proxy
                    .remove("test")
                    .await
                    .expect("fidl failed")
                    .expect_err("remove succeeded");

                let volume_admin_proxy = connect_to_protocol_at_dir_svc::<AdminMarker>(&dir_proxy)
                    .expect("Unable to connect to Admin protocol");
                volume_admin_proxy.shutdown().await.expect("shutdown failed");

                // Creating another volume with the same name should fail.
                let (_dir_proxy, server_end) =
                    fidl::endpoints::create_proxy::<fio::DirectoryMarker>()
                        .expect("create_proxy failed");
                volumes_proxy
                    .create("test", None, server_end)
                    .await
                    .expect("fidl failed")
                    .expect_err("create succeeded");

                volumes_proxy.remove("test").await.expect("fidl failed").expect("remove failed");

                // Removing a non-existent volume should fail.
                volumes_proxy
                    .remove("test")
                    .await
                    .expect("fidl failed")
                    .expect_err("remove failed");

                // Create the same volume again and it should now succeed.
                let (_dir_proxy, server_end) =
                    fidl::endpoints::create_proxy::<fio::DirectoryMarker>()
                        .expect("create_proxy failed");
                volumes_proxy
                    .create("test", None, server_end)
                    .await
                    .expect("fidl failed")
                    .expect("create failed");

                fs_admin_proxy.shutdown().await.expect("shutdown failed");
            }
            .boxed()
        })
        .await;
    }

    #[fasync::run(2, test)]
    async fn test_volumes_enumeration() {
        run_test(|client, _| {
            let volumes_proxy = connect_to_protocol_at_dir_svc::<VolumesMarker>(client)
                .expect("Unable to connect to Volumes protocol");

            let (volumes_dir_proxy, server_end) =
                fidl::endpoints::create_proxy::<fio::DirectoryMarker>()
                    .expect("create_proxy failed");
            client
                .open(
                    fio::OpenFlags::RIGHT_READABLE | fio::OpenFlags::RIGHT_WRITABLE,
                    0,
                    "volumes",
                    ServerEnd::new(server_end.into_channel()),
                )
                .expect("open failed");

            let fs_admin_proxy = connect_to_protocol_at_dir_svc::<AdminMarker>(client)
                .expect("Unable to connect to Admin protocol");

            async move {
                let (_dir_proxy, server_end) =
                    fidl::endpoints::create_proxy::<fio::DirectoryMarker>()
                        .expect("create_proxy failed");
                volumes_proxy
                    .create("test", None, server_end)
                    .await
                    .expect("fidl failed")
                    .expect("create failed");

                assert_eq!(
                    readdir(&volumes_dir_proxy)
                        .await
                        .expect("readdir failed")
                        .iter()
                        .map(|d| d.name.as_str())
                        .collect::<HashSet<_>>(),
                    HashSet::from(["default", "test"])
                );

                fs_admin_proxy.shutdown().await.expect("shutdown failed");
            }
            .boxed()
        })
        .await;
    }
}
