// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    crate::{
        client::{bss_selection, network_selection, types},
        config_management::{self, PastConnectionData, SavedNetworksManagerApi},
        telemetry::{DisconnectInfo, TelemetryEvent, TelemetrySender},
        util::{
            listener::{
                ClientListenerMessageSender, ClientNetworkState, ClientStateUpdate,
                Message::NotifyListeners,
            },
            state_machine::{self, ExitReason, IntoStateExt},
        },
    },
    anyhow::format_err,
    fidl::endpoints::create_proxy,
    fidl_fuchsia_wlan_common as fidl_common, fidl_fuchsia_wlan_ieee80211 as fidl_ieee80211,
    fidl_fuchsia_wlan_internal as fidl_internal, fidl_fuchsia_wlan_policy as fidl_policy,
    fidl_fuchsia_wlan_sme as fidl_sme,
    fuchsia_async::{self as fasync, DurationExt},
    fuchsia_zircon as zx,
    futures::{
        channel::{mpsc, oneshot},
        future::{self, FutureExt},
        select,
        stream::{self, FuturesUnordered, StreamExt, TryStreamExt},
    },
    log::{debug, error, info},
    std::{convert::TryFrom, sync::Arc},
    void::ResultVoidErrExt,
    wlan_common::{bss::BssDescription, energy::DecibelMilliWatt, stats::SignalStrengthAverage},
};

const MAX_CONNECTION_ATTEMPTS: u8 = 4; // arbitrarily chosen until we have some data
type State = state_machine::State<ExitReason>;
type ReqStream = stream::Fuse<mpsc::Receiver<ManualRequest>>;

pub trait ClientApi {
    fn connect(
        &mut self,
        request: types::ConnectRequest,
        responder: oneshot::Sender<()>,
    ) -> Result<(), anyhow::Error>;
    fn disconnect(
        &mut self,
        reason: types::DisconnectReason,
        responder: oneshot::Sender<()>,
    ) -> Result<(), anyhow::Error>;

    /// Queries the liveness of the channel used to control the client state machine.  If the
    /// channel is not alive, this indicates that the client state machine has exited.
    fn is_alive(&self) -> bool;
}

pub struct Client {
    req_sender: mpsc::Sender<ManualRequest>,
}

impl Client {
    pub fn new(req_sender: mpsc::Sender<ManualRequest>) -> Self {
        Self { req_sender }
    }
}

impl ClientApi for Client {
    fn connect(
        &mut self,
        request: types::ConnectRequest,
        responder: oneshot::Sender<()>,
    ) -> Result<(), anyhow::Error> {
        self.req_sender
            .try_send(ManualRequest::Connect((request, responder)))
            .map_err(|e| format_err!("failed to send connect request: {:?}", e))
    }

    fn disconnect(
        &mut self,
        reason: types::DisconnectReason,
        responder: oneshot::Sender<()>,
    ) -> Result<(), anyhow::Error> {
        self.req_sender
            .try_send(ManualRequest::Disconnect((reason, responder)))
            .map_err(|e| format_err!("failed to send disconnect request: {:?}", e))
    }

    fn is_alive(&self) -> bool {
        !self.req_sender.is_closed()
    }
}

pub enum ManualRequest {
    Connect((types::ConnectRequest, oneshot::Sender<()>)),
    Disconnect((types::DisconnectReason, oneshot::Sender<()>)),
}

fn send_listener_state_update(
    sender: &ClientListenerMessageSender,
    network_update: Option<ClientNetworkState>,
) {
    let mut networks = vec![];
    if let Some(network) = network_update {
        networks.push(network)
    }

    let updates =
        ClientStateUpdate { state: fidl_policy::WlanClientState::ConnectionsEnabled, networks };
    match sender.clone().unbounded_send(NotifyListeners(updates)) {
        Ok(_) => (),
        Err(e) => error!("failed to send state update: {:?}", e),
    };
}

pub async fn serve(
    iface_id: u16,
    has_wpa3_support: bool,
    proxy: fidl_sme::ClientSmeProxy,
    sme_event_stream: fidl_sme::ClientSmeEventStream,
    req_stream: mpsc::Receiver<ManualRequest>,
    update_sender: ClientListenerMessageSender,
    saved_networks_manager: Arc<dyn SavedNetworksManagerApi>,
    connect_request: Option<(types::ConnectRequest, oneshot::Sender<()>)>,
    network_selector: Arc<network_selection::NetworkSelector>,
    telemetry_sender: TelemetrySender,
    stats_sender: ConnectionStatsSender,
) {
    let next_network = match connect_request {
        Some((req, sender)) => Some(ConnectingOptions {
            connect_responder: Some(sender),
            connect_request: req,
            attempt_counter: 0,
        }),
        None => None,
    };
    let disconnect_options = DisconnectingOptions {
        disconnect_responder: None,
        previous_network: None,
        next_network,
        reason: types::DisconnectReason::Startup,
    };
    let common_options = CommonStateOptions {
        proxy: proxy,
        req_stream: req_stream.fuse(),
        update_sender: update_sender,
        saved_networks_manager: saved_networks_manager,
        network_selector,
        telemetry_sender,
        iface_id,
        has_wpa3_support,
        stats_sender,
    };
    let state_machine =
        disconnecting_state(common_options, disconnect_options).into_state_machine();
    let removal_watcher = sme_event_stream.map_ok(|_| ()).try_collect::<()>();
    select! {
        state_machine = state_machine.fuse() => {
            match state_machine.void_unwrap_err() {
                ExitReason(Err(e)) => error!("Client state machine for iface #{} terminated with an error: {:?}",
                    iface_id, e),
                ExitReason(Ok(_)) => info!("Client state machine for iface #{} exited gracefully",
                    iface_id,),
            }
        }
        removal_watcher = removal_watcher.fuse() => if let Err(e) = removal_watcher {
            info!("Error reading from Client SME channel of iface #{}: {:?}",
                iface_id, e);
        },
    }
}

/// Common parameters passed to all states
struct CommonStateOptions {
    proxy: fidl_sme::ClientSmeProxy,
    req_stream: ReqStream,
    update_sender: ClientListenerMessageSender,
    saved_networks_manager: Arc<dyn SavedNetworksManagerApi>,
    network_selector: Arc<network_selection::NetworkSelector>,
    telemetry_sender: TelemetrySender,
    iface_id: u16,
    has_wpa3_support: bool,
    /// Used to send periodic connection stats used to determine whether or not to roam.
    stats_sender: mpsc::UnboundedSender<PeriodicConnectionStats>,
}

/// Data that is periodically gathered for determining whether to roam
pub struct PeriodicConnectionStats {
    /// ID and BSSID of the current connection, to exclude it when comparing available networks.
    pub id: types::NetworkIdentifier,
    /// Iface ID that the connection is on.
    pub iface_id: u16,
    pub quality_data: bss_selection::BssQualityData,
}

pub type ConnectionStatsSender = mpsc::UnboundedSender<PeriodicConnectionStats>;
pub type ConnectionStatsReceiver = mpsc::UnboundedReceiver<PeriodicConnectionStats>;

fn handle_none_request() -> Result<State, ExitReason> {
    return Err(ExitReason(Err(format_err!("The stream of requests ended unexpectedly"))));
}

// These functions were introduced to resolve the following error:
// ```
// error[E0391]: cycle detected when evaluating trait selection obligation
// `impl core::future::future::Future: std::marker::Send`
// ```
// which occurs when two functions that return an `impl Trait` call each other
// in a cycle. (e.g. this case `connecting_state` calling `disconnecting_state`,
// which calls `connecting_state`)
fn to_disconnecting_state(
    common_options: CommonStateOptions,
    disconnecting_options: DisconnectingOptions,
) -> State {
    disconnecting_state(common_options, disconnecting_options).into_state()
}
fn to_connecting_state(
    common_options: CommonStateOptions,
    connecting_options: ConnectingOptions,
) -> State {
    connecting_state(common_options, connecting_options).into_state()
}

struct DisconnectingOptions {
    disconnect_responder: Option<oneshot::Sender<()>>,
    /// Information about the previously connected network, if there was one. Used to send out
    /// listener updates.
    previous_network: Option<(types::NetworkIdentifier, types::DisconnectStatus)>,
    /// Configuration for the next network to connect to, after the disconnect is complete. If not
    /// present, the state machine will proceed to IDLE.
    next_network: Option<ConnectingOptions>,
    reason: types::DisconnectReason,
}
/// The DISCONNECTING state requests an SME disconnect, then transitions to either:
/// - the CONNECTING state if options.next_network is present
/// - exit otherwise
async fn disconnecting_state(
    common_options: CommonStateOptions,
    options: DisconnectingOptions,
) -> Result<State, ExitReason> {
    // Log a message with the disconnect reason
    match options.reason {
        types::DisconnectReason::FailedToConnect
        | types::DisconnectReason::Startup
        | types::DisconnectReason::DisconnectDetectedFromSme => {
            // These are either just noise or have separate logging, so keep the level at debug.
            debug!("Disconnected due to {:?}", options.reason);
        }
        reason => {
            info!("Disconnected due to {:?}", reason);
        }
    }

    // TODO(fxbug.dev/53505): either make this fire-and-forget in the SME, or spawn a thread for this,
    // so we don't block on it
    common_options
        .proxy
        .disconnect(types::convert_to_sme_disconnect_reason(options.reason))
        .await
        .map_err(|e| {
            ExitReason(Err(format_err!("Failed to send command to wlanstack: {:?}", e)))
        })?;

    // Notify listeners of disconnection
    let networks = match options.previous_network {
        Some((network_identifier, status)) => Some(ClientNetworkState {
            id: network_identifier,
            state: types::ConnectionState::Disconnected,
            status: Some(status),
        }),
        None => None,
    };
    send_listener_state_update(&common_options.update_sender, networks);

    // Notify the caller that disconnect was sent to the SME once the final disconnected update has
    // been sent.  This ensures that there will not be a race when the IfaceManager sends out a
    // ConnectionsDisabled update.
    match options.disconnect_responder {
        Some(responder) => responder.send(()).unwrap_or_else(|_| ()),
        None => (),
    }

    // Transition to next state
    match options.next_network {
        Some(next_network) => Ok(to_connecting_state(common_options, next_network)),
        None => Err(ExitReason(Ok(()))),
    }
}

fn connect_txn_event_name(event: &fidl_sme::ConnectTransactionEvent) -> &'static str {
    match event {
        fidl_sme::ConnectTransactionEvent::OnConnectResult { .. } => "OnConnectResult",
        fidl_sme::ConnectTransactionEvent::OnDisconnect { .. } => "OnDisconnect",
        fidl_sme::ConnectTransactionEvent::OnSignalReport { .. } => "OnSignalReport",
        fidl_sme::ConnectTransactionEvent::OnChannelSwitched { .. } => "OnChannelSwitched",
    }
}

async fn wait_until_connected(
    txn: fidl_sme::ConnectTransactionProxy,
) -> Result<(fidl_sme::ConnectResult, fidl_sme::ConnectTransactionEventStream), anyhow::Error> {
    let mut stream = txn.take_event_stream();
    if let Some(event) = stream.try_next().await? {
        match event {
            fidl_sme::ConnectTransactionEvent::OnConnectResult { result } => {
                return Ok((result, stream))
            }
            other => {
                return Err(format_err!(
                    "Expected ConnectTransactionEvent::OnConnectResult, got {}",
                    connect_txn_event_name(&other)
                ))
            }
        }
    }
    Err(format_err!("Server closed the ConnectTransaction channel before sending a response"))
}

struct ConnectingOptions {
    connect_responder: Option<oneshot::Sender<()>>,
    connect_request: types::ConnectRequest,
    /// Count of previous consecutive failed connection attempts to this same network.
    attempt_counter: u8,
}

struct ConnectResult {
    sme_result: fidl_sme::ConnectResult,
    multiple_bss_candidates: bool,
    connect_txn_stream: fidl_sme::ConnectTransactionEventStream,
    bss_description: Box<BssDescription>,
}

#[derive(Clone, Debug)]
struct ScanResult {
    bss_description: fidl_internal::BssDescription,
    has_multiple_bss_candidates: bool,
    security_type_detailed: types::SecurityTypeDetailed,
}

impl From<types::ScannedCandidate> for ScanResult {
    fn from(candidate: types::ScannedCandidate) -> Self {
        let types::ScannedCandidate {
            bss_description,
            has_multiple_bss_candidates,
            security_type_detailed,
            ..
        } = candidate;
        ScanResult { bss_description, has_multiple_bss_candidates, security_type_detailed }
    }
}

enum SmeOperation {
    /// Include information about the time the connection attempt started
    ConnectResult(Result<ConnectResult, anyhow::Error>, fasync::Time),
    ScanResult(Option<ScanResult>),
}

async fn handle_connecting_error_and_retry(
    common_options: CommonStateOptions,
    options: ConnectingOptions,
) -> Result<State, ExitReason> {
    // Check if the limit for connection attempts to this network has been
    // exceeded.
    let new_attempt_count = options.attempt_counter + 1;
    if new_attempt_count >= MAX_CONNECTION_ATTEMPTS {
        info!("Exceeded maximum connection attempts, will not retry");
        send_listener_state_update(
            &common_options.update_sender,
            Some(ClientNetworkState {
                id: options.connect_request.target.network,
                state: types::ConnectionState::Failed,
                status: Some(types::DisconnectStatus::ConnectionFailed),
            }),
        );
        return Err(ExitReason(Ok(())));
    } else {
        // Limit not exceeded, retry after backing off.
        let backoff_time = 400_i64 * i64::from(new_attempt_count);
        info!("Will attempt to reconnect after {}ms backoff", backoff_time);
        fasync::Timer::new(zx::Duration::from_millis(backoff_time).after_now()).await;

        let next_connecting_options = ConnectingOptions {
            connect_responder: None,
            connect_request: types::ConnectRequest {
                reason: types::ConnectReason::RetryAfterFailedConnectAttempt,
                ..options.connect_request
            },
            attempt_counter: new_attempt_count,
        };
        let disconnecting_options = DisconnectingOptions {
            disconnect_responder: None,
            previous_network: None,
            next_network: Some(next_connecting_options),
            reason: types::DisconnectReason::FailedToConnect,
        };
        return Ok(to_disconnecting_state(common_options, disconnecting_options));
    }
}

/// The CONNECTING state checks for the required station information in the connection request. If not
/// present, the state first requests an SME scan. For a failed scan, retry connection by passing a
/// next_network to the DISCONNECTING state, as long as there haven't been too many attempts.
/// Next, it requests an SME connect. It handles the SME connect response:
/// - for a successful connection, transition to CONNECTED state
/// - for a failed connection, retry connection by passing a next_network to the
///       DISCONNECTING state, as long as there haven't been too many connection attempts
/// During this time, incoming ManualRequests are also monitored for:
/// - duplicate connect requests are deduped
/// - different connect requests are serviced by passing a next_network to the DISCONNECTING state
/// - disconnect requests cause a transition to DISCONNECTING state
async fn connecting_state<'a>(
    mut common_options: CommonStateOptions,
    mut options: ConnectingOptions,
) -> Result<State, ExitReason> {
    debug!("Entering connecting state");

    if options.attempt_counter > 0 {
        info!(
            "Retrying connection, {} attempts remaining",
            MAX_CONNECTION_ATTEMPTS - options.attempt_counter
        );
    }

    // Send a "Connecting" update to listeners, unless this is a retry
    if options.attempt_counter == 0 {
        send_listener_state_update(
            &common_options.update_sender,
            Some(ClientNetworkState {
                id: options.connect_request.target.network.clone(),
                state: types::ConnectionState::Connecting,
                status: None,
            }),
        );
    };

    // Let the responder know we've successfully started this connection attempt
    match options.connect_responder.take() {
        Some(responder) => responder.send(()).unwrap_or_else(|_| ()),
        None => {}
    }

    // If detailed station information was not provided, perform a scan to discover it
    let network_selector = common_options.network_selector.clone();
    let scan_future = match options.connect_request.target.scanned {
        Some(ref scanned) => {
            future::ready(SmeOperation::ScanResult(Some(ScanResult::from(scanned.clone())))).boxed()
        }
        None => {
            info!("Connection requested, scanning to find a BSS for the network");
            network_selector
                .find_connection_candidate_for_network(
                    common_options.proxy.clone(),
                    options.connect_request.target.network.clone(),
                )
                .map(|candidate| {
                    SmeOperation::ScanResult(candidate.and_then(
                        |types::ConnectionCandidate { scanned, .. }| scanned.map(ScanResult::from),
                    ))
                })
                .boxed()
        }
    };
    let mut internal_futures = FuturesUnordered::new();
    internal_futures.push(scan_future);

    loop {
        select! {
            // Monitor the SME operations
            completed_future = internal_futures.select_next_some() => match completed_future {
                SmeOperation::ScanResult(scan) => {
                    let ScanResult {
                        bss_description,
                        has_multiple_bss_candidates: multiple_bss_candidates,
                        security_type_detailed,
                    } = match scan {
                        Some(scan) => scan,
                        None => {
                            info!("Failed to find a BSS to which to connect.");
                            return handle_connecting_error_and_retry(common_options, options).await;
                        }
                    };
                    let parsed_bss_description = BssDescription::try_from(bss_description.clone())
                        .map_err(|error| {
                            // This only occurs if an invalid `BssDescription` is received from
                            // SME, which should never happen.
                            ExitReason(Err(format_err!(
                                "Failed to convert BSS description from FIDL: {:?}",
                                error,
                            )))
                        })?;
                    // TODO(fxbug.dev/102606): Move this call to network selection and write the
                    //                         result into a field of `ScannedCandidate`. This code
                    //                         should read that field instead of calling this
                    //                         function directly.
                    let authentication = config_management::select_authentication_method(
                        security_type_detailed,
                        options.connect_request.target.credential.clone(),
                        common_options.has_wpa3_support,
                    )
                    .ok_or_else(|| {
                        // This only occurs if invalid or unsupported security criteria are
                        // received from the network selector, which should never happen.
                        ExitReason(Err(format_err!(
                            "Failed to negotiate authentication for {:?} network.",
                            security_type_detailed,
                        )))
                    })?;
                    // Send a connect request to the SME
                    let (connect_txn, remote) = create_proxy()
                        .map_err(|e| ExitReason(Err(format_err!("Failed to create proxy: {:?}", e))))?;
                    let mut sme_connect_request = fidl_sme::ConnectRequest {
                        ssid: options.connect_request.target.network.ssid.to_vec(),
                        bss_description,
                        multiple_bss_candidates,
                        authentication,
                        deprecated_scan_type: fidl_fuchsia_wlan_common::ScanType::Active,
                    };
                    common_options.proxy.connect(&mut sme_connect_request, Some(remote)).map_err(|e| {
                        ExitReason(Err(format_err!("Failed to send command to wlanstack: {:?}", e)))
                    })?;
                    let start_time = fasync::Time::now();
                    let pending_connect_request = wait_until_connected(connect_txn.clone())
                        .map(move |res| {
                            let result = res.map(|(sme_result, stream)| ConnectResult {
                                sme_result,
                                multiple_bss_candidates,
                                connect_txn_stream: stream,
                                bss_description: Box::new(parsed_bss_description),
                            });
                            SmeOperation::ConnectResult(result, start_time)
                        })
                        .boxed();
                    internal_futures.push(pending_connect_request);
                },
                SmeOperation::ConnectResult(connect_result, start_time) => {
                    let connect_result = connect_result.map_err({
                        |e| ExitReason(Err(format_err!("failed to send connect to sme: {:?}", e)))
                    })?;
                    let sme_result = connect_result.sme_result;
                    // Notify the saved networks manager of the scan mode with which the network
                    // was observed if a scan was performed.
                    let scan_type = options.connect_request.target.scanned
                        .as_ref()
                        .and_then(|scanned| match scanned.observation {
                            types::ScanObservation::Passive => Some(fidl_common::ScanType::Passive),
                            types::ScanObservation::Active => Some(fidl_common::ScanType::Active),
                            types::ScanObservation::Unknown => None,
                        });
                    common_options.saved_networks_manager.record_connect_result(
                        options.connect_request.target.network.clone().into(),
                        &options.connect_request.target.credential,
                        connect_result.bss_description.bssid,
                        sme_result,
                        scan_type
                    ).await;

                    common_options.telemetry_sender.send(TelemetryEvent::ConnectResult {
                        latest_ap_state: (*connect_result.bss_description).clone(),
                        result: sme_result,
                        policy_connect_reason: Some(options.connect_request.reason),
                        multiple_bss_candidates: connect_result.multiple_bss_candidates,
                        iface_id: common_options.iface_id,
                    });
                    match (sme_result.code, sme_result.is_credential_rejected) {
                        (fidl_ieee80211::StatusCode::Success, _) => {
                            info!("Successfully connected to network");
                            send_listener_state_update(
                                &common_options.update_sender,
                                Some(ClientNetworkState {
                                    id: options.connect_request.target.network.clone(),
                                    state: types::ConnectionState::Connected,
                                    status: None
                                }),
                            );
                            let connected_options = ConnectedOptions {
                                currently_fulfilled_request: options.connect_request,
                                connect_txn_stream: connect_result.connect_txn_stream,
                                latest_ap_state: connect_result.bss_description,
                                multiple_bss_candidates: connect_result.multiple_bss_candidates,
                                connection_attempt_time: start_time,
                                time_to_connect: fasync::Time::now() - start_time,
                            };
                            return Ok(
                                connected_state(common_options, connected_options).into_state()
                            );
                        },
                        (code, true) => {
                            info!("Failed to connect: {:?}. Will not retry because of credential error.", code);
                            send_listener_state_update(
                                &common_options.update_sender,
                                Some(ClientNetworkState {
                                    id: options.connect_request.target.network,
                                    state: types::ConnectionState::Failed,
                                    status: Some(types::DisconnectStatus::CredentialsFailed),
                                }),
                            );
                            return Err(ExitReason(Err(format_err!("bad credentials"))));
                        },
                        (code, _) => {
                            info!("Failed to connect: {:?}", code);
                            return handle_connecting_error_and_retry(common_options, options).await;
                        }
                    };
                },
            },
            // Monitor incoming ManualRequests
            new_req = common_options.req_stream.next() => match new_req {
                Some(ManualRequest::Disconnect((reason, responder))) => {
                    info!("Cancelling pending connect due to disconnect request");
                    send_listener_state_update(
                        &common_options.update_sender,
                        Some(ClientNetworkState {
                            id: options.connect_request.target.network,
                            state: types::ConnectionState::Disconnected,
                            status: Some(types::DisconnectStatus::ConnectionStopped)
                        }),
                    );
                    let options = DisconnectingOptions {
                        disconnect_responder: Some(responder),
                        previous_network: None,
                        next_network: None,
                        reason,
                    };
                    return Ok(to_disconnecting_state(common_options, options));
                }
                Some(ManualRequest::Connect((new_connect_request, new_responder))) => {
                    // Check if it's the same network as we're currently connected to.
                    // If yes, dedupe the request.
                    if new_connect_request.target.network == options.connect_request.target.network {
                        info!("Received duplicate connection request, deduping");
                        new_responder.send(()).unwrap_or_else(|_| ());
                    } else {
                        info!("Cancelling pending connect due to new connection request");
                        send_listener_state_update(
                            &common_options.update_sender,
                            Some(ClientNetworkState {
                                id: options.connect_request.target.network,
                                state: types::ConnectionState::Disconnected,
                                status: Some(types::DisconnectStatus::ConnectionStopped)
                            }),
                        );
                        let next_connecting_options = ConnectingOptions {
                            connect_responder: Some(new_responder),
                            connect_request: new_connect_request.clone(),
                            attempt_counter: 0,
                        };
                        let disconnecting_options = DisconnectingOptions {
                            disconnect_responder: None,
                            previous_network: None,
                            next_network: Some(next_connecting_options),
                            reason: match new_connect_request.reason {
                                types::ConnectReason::ProactiveNetworkSwitch => types::DisconnectReason::ProactiveNetworkSwitch,
                                types::ConnectReason::FidlConnectRequest => types::DisconnectReason::FidlConnectRequest,
                                _ => {
                                    error!("Unexpected connection reason: {:?}", new_connect_request.reason);
                                    types::DisconnectReason::Unknown
                                }
                            },
                        };
                        return Ok(to_disconnecting_state(common_options, disconnecting_options));
                    }
                }
                None => return handle_none_request(),
            },
        }
    }
}

struct ConnectedOptions {
    // Keep track of the BSSID we are connected in order to record connection information for
    // future network selection.
    latest_ap_state: Box<BssDescription>,
    multiple_bss_candidates: bool,
    currently_fulfilled_request: types::ConnectRequest,
    connect_txn_stream: fidl_sme::ConnectTransactionEventStream,
    /// Time at which connect was first attempted, historical data for network scoring.
    pub connection_attempt_time: fasync::Time,
    /// Duration from connection attempt to success, historical data for network scoring.
    pub time_to_connect: zx::Duration,
}

/// The CONNECTED state monitors the SME status. It handles the SME status response:
/// - if still connected to the correct network, no action
/// - if disconnected, retry connection by passing a next_network to the
///       DISCONNECTING state
/// During this time, incoming ManualRequests are also monitored for:
/// - duplicate connect requests are deduped
/// - different connect requests are serviced by passing a next_network to the DISCONNECTING state
/// - disconnect requests cause a transition to DISCONNECTING state
async fn connected_state(
    mut common_options: CommonStateOptions,
    mut options: ConnectedOptions,
) -> Result<State, ExitReason> {
    debug!("Entering connected state");
    let mut connect_start_time = fasync::Time::now();

    // Initialize connection data
    let past_connections = common_options
        .saved_networks_manager
        .get_past_connections(
            &options.currently_fulfilled_request.target.network,
            &options.currently_fulfilled_request.target.credential,
            &options.latest_ap_state.bssid,
        )
        .await;
    let mut bss_quality_data = bss_selection::BssQualityData::new(
        bss_selection::SignalData::new(
            options.latest_ap_state.rssi_dbm,
            options.latest_ap_state.snr_db,
            bss_selection::EWMA_SMOOTHING_FACTOR,
        ),
        options.latest_ap_state.channel,
        past_connections,
    );

    // Keep track of the connection's average signal strength for future scoring.
    let mut avg_rssi = SignalStrengthAverage::new();

    loop {
        select! {
            event = options.connect_txn_stream.next() => match event {
                Some(Ok(event)) => {
                    let is_sme_idle = match event {
                        fidl_sme::ConnectTransactionEvent::OnDisconnect { info: fidl_info } => {
                            // Log a disconnect in Cobalt
                            let now = fasync::Time::now();
                            let info = DisconnectInfo {
                                connected_duration: now - connect_start_time,
                                is_sme_reconnecting: fidl_info.is_sme_reconnecting,
                                disconnect_source: fidl_info.disconnect_source,
                                latest_ap_state: (*options.latest_ap_state).clone(),
                            };
                            common_options.telemetry_sender.send(TelemetryEvent::Disconnected { track_subsequent_downtime: true, info });

                            // Record data about the connection and disconnect for future network
                            // selection.
                            record_disconnect(
                                &common_options,
                                &options,
                                connect_start_time,
                                types::DisconnectReason::DisconnectDetectedFromSme,
                                bss_quality_data.signal_data.clone(),
                            ).await;

                            !fidl_info.is_sme_reconnecting
                            }
                        fidl_sme::ConnectTransactionEvent::OnConnectResult { result } => {
                            let connected = result.code == fidl_ieee80211::StatusCode::Success;
                            if connected {
                                // This OnConnectResult should be for reconnecting to the same AP,
                                // so keep the same SignalData but reset the connect start time
                                // to track as a new connection.
                                connect_start_time = fasync::Time::now();
                            }
                            common_options.telemetry_sender.send(TelemetryEvent::ConnectResult {
                                iface_id: common_options.iface_id,
                                result,
                                policy_connect_reason: None,
                                // It's not necessarily true that there are still multiple BSS
                                // candidates in the network at this point in time, but we use the
                                // heuristic that if previously there were multiple BSS's, then
                                // it likely remains the same.
                                multiple_bss_candidates: options.multiple_bss_candidates,
                                latest_ap_state: (*options.latest_ap_state).clone(),
                            });
                            !connected
                        }
                        fidl_sme::ConnectTransactionEvent::OnSignalReport { ind } => {
                            // Update connection data
                            options.latest_ap_state.rssi_dbm = ind.rssi_dbm;
                            options.latest_ap_state.snr_db = ind.snr_db;
                            bss_quality_data.signal_data.update_with_new_measurement(ind.rssi_dbm, ind.snr_db);
                            avg_rssi.add(DecibelMilliWatt(ind.rssi_dbm));
                            let current_connection = &options.currently_fulfilled_request.target;
                            handle_connection_stats(
                                &mut common_options.telemetry_sender,
                                &mut common_options.stats_sender,
                                common_options.iface_id,
                                current_connection.network.clone(),
                                ind,
                                bss_quality_data.clone()
                            ).await;

                            // Evaluate current BSS, and determine if roaming future should be
                            // triggered.
                            let (_bss_score, roam_reasons) = bss_selection::evaluate_current_bss(bss_quality_data.clone());
                            if !roam_reasons.is_empty() {
                                common_options.telemetry_sender.send(TelemetryEvent::RoamingScan);
                                // TODO(haydennix): Trigger roaming future, which must be idempotent
                                // since repeated calls are likely.
                            }
                            false
                        }
                        fidl_sme::ConnectTransactionEvent::OnChannelSwitched { info } => {
                            options.latest_ap_state.channel.primary = info.new_channel;
                            common_options.telemetry_sender.send(TelemetryEvent::OnChannelSwitched { info });
                            false
                        }
                    };

                    if is_sme_idle {
                        let next_connecting_options = ConnectingOptions {
                            connect_responder: None,
                            connect_request: types::ConnectRequest {
                                reason: types::ConnectReason::RetryAfterDisconnectDetected,
                                target: types::ConnectionCandidate {
                                    // strip out the bss info to force a new scan
                                    scanned: None,
                                    ..options.currently_fulfilled_request.target.clone()
                                }
                            },
                            attempt_counter: 0,
                        };

                        let options = DisconnectingOptions {
                            disconnect_responder: None,
                            previous_network: Some((options.currently_fulfilled_request.target.network.clone(), types::DisconnectStatus::ConnectionFailed)),
                            next_network: Some(next_connecting_options),
                            reason: types::DisconnectReason::DisconnectDetectedFromSme,
                        };
                        common_options.telemetry_sender.send(TelemetryEvent::StartEstablishConnection { reset_start_time: false });
                        info!("Detected disconnection from network, will attempt reconnection");
                        return Ok(disconnecting_state(common_options, options).into_state());
                    }
                }
                _ => {
                    info!("SME dropped ConnectTransaction channel. Exiting state machine");
                    return Err(ExitReason(Err(format_err!("Failed to receive ConnectTransactionEvent for SME status"))));
                }
            },
            req = common_options.req_stream.next() => {
                let now = fasync::Time::now();
                match req {
                    Some(ManualRequest::Disconnect((reason, responder))) => {
                        debug!("Disconnect requested");
                        record_disconnect(
                            &common_options,
                            &options,
                            connect_start_time,
                            reason,
                            bss_quality_data.signal_data
                        ).await;
                        let latest_ap_state = options.latest_ap_state;
                        let options = DisconnectingOptions {
                            disconnect_responder: Some(responder),
                            previous_network: Some((options.currently_fulfilled_request.target.network, types::DisconnectStatus::ConnectionStopped)),
                            next_network: None,
                            reason,
                        };
                        let info = DisconnectInfo {
                            connected_duration: now - connect_start_time,
                            is_sme_reconnecting: false,
                            disconnect_source: fidl_sme::DisconnectSource::User(types::convert_to_sme_disconnect_reason(options.reason)),
                            latest_ap_state: *latest_ap_state,
                        };
                        common_options.telemetry_sender.send(TelemetryEvent::Disconnected { track_subsequent_downtime: false, info });
                        return Ok(disconnecting_state(common_options, options).into_state());
                    }
                    Some(ManualRequest::Connect((new_connect_request, new_responder))) => {
                        // Check if it's the same network as we're currently connected to. If yes, reply immediately
                        if new_connect_request.target.network == options.currently_fulfilled_request.target.network {
                            info!("Received connection request for current network, deduping");
                            new_responder.send(()).unwrap_or_else(|_| ());
                        } else {
                            let disconnect_reason = convert_manual_connect_to_disconnect_reason(&new_connect_request.reason).unwrap_or_else(|()| {
                                error!("Unexpected connection reason: {:?}", new_connect_request.reason);
                                types::DisconnectReason::Unknown
                            });
                            record_disconnect(
                                &common_options,
                                &options,
                                connect_start_time,
                                disconnect_reason,
                                bss_quality_data.signal_data
                            ).await;


                            let next_connecting_options = ConnectingOptions {
                                connect_responder: Some(new_responder),
                                connect_request: new_connect_request.clone(),
                                attempt_counter: 0,
                            };
                            let latest_ap_state = options.latest_ap_state;
                            let options = DisconnectingOptions {
                                disconnect_responder: None,
                                previous_network: Some((options.currently_fulfilled_request.target.network, types::DisconnectStatus::ConnectionStopped)),
                                next_network: Some(next_connecting_options),
                                reason: disconnect_reason,
                            };
                            info!("Connection to new network requested, disconnecting from current network");
                            let info = DisconnectInfo {
                                connected_duration: now - connect_start_time,
                                is_sme_reconnecting: false,
                                disconnect_source: fidl_sme::DisconnectSource::User(types::convert_to_sme_disconnect_reason(options.reason)),
                                latest_ap_state: *latest_ap_state,
                            };
                            common_options.telemetry_sender.send(TelemetryEvent::Disconnected { track_subsequent_downtime: false, info });
                            return Ok(disconnecting_state(common_options, options).into_state())
                        }
                    }
                    None => return handle_none_request(),
                };
            }
        }
    }
}

/// Update IfaceManager with the updated connection quality data.
async fn handle_connection_stats(
    telemetry_sender: &mut TelemetrySender,
    stats_sender: &mut ConnectionStatsSender,
    iface_id: u16,
    id: types::NetworkIdentifier,
    ind: fidl_internal::SignalReportIndication,
    bss_quality_data: bss_selection::BssQualityData,
) {
    let connection_stats =
        PeriodicConnectionStats { id, iface_id, quality_data: bss_quality_data.clone() };
    stats_sender.unbounded_send(connection_stats).unwrap_or_else(|e| {
        error!("Failed to send periodic connection stats from the connected state: {}", e);
    });
    // Send RSSI and RSSI velocity metrics
    telemetry_sender.send(TelemetryEvent::OnSignalReport {
        ind,
        rssi_velocity: bss_quality_data.signal_data.rssi_velocity,
    });
}

async fn record_disconnect(
    common_options: &CommonStateOptions,
    options: &ConnectedOptions,
    connect_start_time: fasync::Time,
    reason: types::DisconnectReason,
    signal_data: bss_selection::SignalData,
) {
    let curr_time = fasync::Time::now();
    let uptime = curr_time - connect_start_time;
    let data = PastConnectionData::new(
        options.latest_ap_state.bssid,
        options.connection_attempt_time,
        options.time_to_connect,
        curr_time,
        uptime,
        reason,
        signal_data,
        // TODO: record average phy rate over connection once available
        0,
    );
    common_options
        .saved_networks_manager
        .record_disconnect(
            &options.currently_fulfilled_request.target.network.clone().into(),
            &options.currently_fulfilled_request.target.credential,
            data,
        )
        .await;
}

/// Get the disconnect reason corresponding to the connect reason. Return an error if the connect
/// reason does not correspond to a manual connect.
pub fn convert_manual_connect_to_disconnect_reason(
    reason: &types::ConnectReason,
) -> Result<types::DisconnectReason, ()> {
    match reason {
        types::ConnectReason::FidlConnectRequest => Ok(types::DisconnectReason::FidlConnectRequest),
        types::ConnectReason::ProactiveNetworkSwitch => {
            Ok(types::DisconnectReason::ProactiveNetworkSwitch)
        }
        types::ConnectReason::RetryAfterDisconnectDetected
        | types::ConnectReason::RetryAfterFailedConnectAttempt
        | types::ConnectReason::RegulatoryChangeReconnect
        | types::ConnectReason::IdleInterfaceAutoconnect
        | types::ConnectReason::NewSavedNetworkAutoconnect => Err(()),
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            config_management::{
                network_config::{
                    self, AddAndGetRecent, Credential, FailureReason, WPA_PSK_BYTE_LEN,
                },
                PastConnectionList, SavedNetworksManager,
            },
            telemetry::{TelemetryEvent, TelemetrySender},
            util::{
                listener,
                testing::{
                    create_inspect_persistence_channel, create_wlan_hasher,
                    generate_disconnect_info, poll_sme_req, random_connection_data,
                    validate_sme_scan_request_and_send_results, ConnectResultRecord,
                    ConnectionRecord, FakeSavedNetworksManager,
                },
            },
        },
        fidl::endpoints::create_proxy_and_stream,
        fidl::prelude::*,
        fidl_fuchsia_stash as fidl_stash, fidl_fuchsia_wlan_policy as fidl_policy,
        fuchsia_inspect::{self as inspect},
        fuchsia_zircon::prelude::*,
        futures::{task::Poll, Future},
        pin_utils::pin_mut,
        std::convert::TryFrom,
        test_case::test_case,
        test_util::{assert_gt, assert_lt},
        wlan_common::{
            assert_variant, bss::Protection, random_bss_description, random_fidl_bss_description,
        },
        wlan_metrics_registry::PolicyDisconnectionMigratedMetricDimensionReason,
    };

    struct TestValues {
        common_options: CommonStateOptions,
        sme_req_stream: fidl_sme::ClientSmeRequestStream,
        saved_networks_manager: Arc<FakeSavedNetworksManager>,
        client_req_sender: mpsc::Sender<ManualRequest>,
        update_receiver: mpsc::UnboundedReceiver<listener::ClientListenerMessage>,
        telemetry_receiver: mpsc::Receiver<TelemetryEvent>,
        stats_receiver: mpsc::UnboundedReceiver<PeriodicConnectionStats>,
    }

    fn test_setup() -> TestValues {
        let (client_req_sender, client_req_stream) = mpsc::channel(1);
        let (update_sender, update_receiver) = mpsc::unbounded();
        let (sme_proxy, sme_server) =
            create_proxy::<fidl_sme::ClientSmeMarker>().expect("failed to create an sme channel");
        let sme_req_stream = sme_server.into_stream().expect("could not create SME request stream");
        let saved_networks = FakeSavedNetworksManager::new();
        let saved_networks_manager = Arc::new(saved_networks);
        let (persistence_req_sender, _persistence_stream) = create_inspect_persistence_channel();
        let (telemetry_sender, telemetry_receiver) = mpsc::channel::<TelemetryEvent>(100);
        let telemetry_sender = TelemetrySender::new(telemetry_sender);
        let network_selector = Arc::new(network_selection::NetworkSelector::new(
            saved_networks_manager.clone(),
            create_wlan_hasher(),
            inspect::Inspector::new().root().create_child("network_selector"),
            persistence_req_sender,
            telemetry_sender.clone(),
        ));
        let (stats_sender, stats_receiver) = mpsc::unbounded();

        TestValues {
            common_options: CommonStateOptions {
                proxy: sme_proxy,
                req_stream: client_req_stream.fuse(),
                update_sender: update_sender,
                saved_networks_manager: saved_networks_manager.clone(),
                network_selector,
                telemetry_sender,
                iface_id: 1,
                has_wpa3_support: false,
                stats_sender,
            },
            sme_req_stream,
            saved_networks_manager,
            client_req_sender,
            update_receiver,
            telemetry_receiver,
            stats_receiver,
        }
    }

    async fn run_state_machine(
        fut: impl Future<Output = Result<State, ExitReason>> + Send + 'static,
    ) {
        let state_machine = fut.into_state_machine();
        select! {
            _state_machine = state_machine.fuse() => return,
        }
    }

    fn wep_key() -> Credential {
        Credential::Password("abcdef0000".as_bytes().to_vec())
    }

    fn wpa_password() -> Credential {
        Credential::Password("password".as_bytes().to_vec())
    }

    fn wpa_psk() -> Credential {
        Credential::Psk(vec![0u8; WPA_PSK_BYTE_LEN])
    }

    /// Move stash requests forward so that a save request can progress.
    fn process_stash_write(
        exec: &mut fasync::TestExecutor,
        stash_server: &mut fidl_stash::StoreAccessorRequestStream,
    ) {
        assert_variant!(
            exec.run_until_stalled(&mut stash_server.try_next()),
            Poll::Ready(Ok(Some(fidl_stash::StoreAccessorRequest::SetValue { .. })))
        );
        assert_variant!(
            exec.run_until_stalled(&mut stash_server.try_next()),
            Poll::Ready(Ok(Some(fidl_stash::StoreAccessorRequest::Flush{responder}))) => {
                responder.send(&mut Ok(())).expect("failed to send stash response");
            }
        );
    }

    #[fuchsia::test]
    fn connecting_state_successfully_connects() {
        let mut exec = fasync::TestExecutor::new().expect("failed to create an executor");
        let mut test_values = test_setup();
        // Do SavedNetworksManager set up manually to get functionality and stash server
        let (saved_networks, mut stash_server) =
            exec.run_singlethreaded(SavedNetworksManager::new_and_stash_server());
        let saved_networks_manager = Arc::new(saved_networks);
        test_values.common_options.saved_networks_manager = saved_networks_manager.clone();
        let next_network_ssid = types::Ssid::try_from("bar").unwrap();
        let bss_description = random_fidl_bss_description!(Wpa2, ssid: next_network_ssid.clone());
        let connect_request = types::ConnectRequest {
            target: types::ConnectionCandidate {
                network: types::NetworkIdentifier {
                    ssid: next_network_ssid.clone(),
                    security_type: types::SecurityType::Wep,
                },
                credential: Credential::Password("five0".as_bytes().to_vec()),
                scanned: Some(types::ScannedCandidate {
                    bss_description: bss_description.clone(),
                    observation: types::ScanObservation::Passive,
                    has_multiple_bss_candidates: true,
                    security_type_detailed: types::SecurityTypeDetailed::Wep,
                }),
            },
            reason: types::ConnectReason::FidlConnectRequest,
        };

        // Store the network in the saved_networks_manager, so we can record connection success
        let save_fut = saved_networks_manager.store(
            connect_request.target.network.clone().into(),
            connect_request.target.credential.clone(),
        );
        pin_mut!(save_fut);
        assert_variant!(exec.run_until_stalled(&mut save_fut), Poll::Pending);
        process_stash_write(&mut exec, &mut stash_server);
        assert_variant!(exec.run_until_stalled(&mut save_fut), Poll::Ready(Ok(None)));

        // Check that the saved networks manager has the expected initial data
        let saved_networks = exec.run_singlethreaded(
            saved_networks_manager.lookup(&connect_request.target.network.clone().into()),
        );
        assert_eq!(false, saved_networks[0].has_ever_connected);
        assert!(saved_networks[0].hidden_probability > 0.0);

        let (connect_sender, mut connect_receiver) = oneshot::channel();
        let connecting_options = ConnectingOptions {
            connect_responder: Some(connect_sender),
            connect_request: connect_request.clone(),
            attempt_counter: 0,
        };
        let initial_state = connecting_state(test_values.common_options, connecting_options);
        let fut = run_state_machine(initial_state);
        pin_mut!(fut);
        let sme_fut = test_values.sme_req_stream.into_future();
        pin_mut!(sme_fut);

        // Run the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Ensure a connect request is sent to the SME
        let connect_txn_handle = assert_variant!(
            poll_sme_req(&mut exec, &mut sme_fut),
            Poll::Ready(fidl_sme::ClientSmeRequest::Connect{ req, txn, control_handle: _ }) => {
                assert_eq!(req.ssid, next_network_ssid.to_vec());
                assert_eq!(connect_request.target.credential, req.authentication.credentials);
                assert_eq!(req.bss_description, bss_description);
                assert_eq!(req.deprecated_scan_type, fidl_fuchsia_wlan_common::ScanType::Active);
                assert_eq!(req.multiple_bss_candidates, true);
                // Send connection response.
                let (_stream, ctrl) = txn.expect("connect txn unused")
                    .into_stream_and_control_handle().expect("error accessing control handle");
                ctrl
            }
        );
        connect_txn_handle
            .send_on_connect_result(&mut fake_successful_connect_result())
            .expect("failed to send connection completion");

        // Check for a connecting update
        let client_state_update = ClientStateUpdate {
            state: fidl_policy::WlanClientState::ConnectionsEnabled,
            networks: vec![ClientNetworkState {
                id: types::NetworkIdentifier {
                    ssid: next_network_ssid.clone(),
                    security_type: types::SecurityType::Wep,
                },
                state: fidl_policy::ConnectionState::Connecting,
                status: None,
            }],
        };
        assert_variant!(
            test_values.update_receiver.try_next(),
            Ok(Some(listener::Message::NotifyListeners(updates))) => {
            assert_eq!(updates, client_state_update);
        });

        // Progress the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);
        process_stash_write(&mut exec, &mut stash_server);
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Check the responder was acknowledged
        assert_variant!(exec.run_until_stalled(&mut connect_receiver), Poll::Ready(Ok(())));

        // Check for a connect update
        let client_state_update = ClientStateUpdate {
            state: fidl_policy::WlanClientState::ConnectionsEnabled,
            networks: vec![ClientNetworkState {
                id: types::NetworkIdentifier {
                    ssid: next_network_ssid.clone(),
                    security_type: types::SecurityType::Wep,
                },
                state: fidl_policy::ConnectionState::Connected,
                status: None,
            }],
        };
        assert_variant!(
            test_values.update_receiver.try_next(),
            Ok(Some(listener::Message::NotifyListeners(updates))) => {
            assert_eq!(updates, client_state_update);
        });

        // Check that the saved networks manager has the connection recorded
        let saved_networks = exec.run_singlethreaded(
            saved_networks_manager.lookup(&connect_request.target.network.clone().into()),
        );
        assert_eq!(true, saved_networks[0].has_ever_connected);
        assert_eq!(
            network_config::PROB_HIDDEN_IF_CONNECT_PASSIVE,
            saved_networks[0].hidden_probability
        );

        // Progress the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Ensure no further updates were sent to listeners
        assert_variant!(
            exec.run_until_stalled(&mut test_values.update_receiver.into_future()),
            Poll::Pending
        );
    }

    #[fuchsia::test]
    fn connecting_state_successfully_scans_and_connects() {
        let mut exec =
            fasync::TestExecutor::new_with_fake_time().expect("failed to create an executor");
        exec.set_fake_time(fasync::Time::from_nanos(123));
        let mut test_values = test_setup();
        let next_network_ssid = types::Ssid::try_from("bar").unwrap();
        let id = types::NetworkIdentifier {
            ssid: next_network_ssid.clone(),
            security_type: types::SecurityType::Wep,
        };
        let credential = Credential::Password("12345".as_bytes().to_vec());
        let connection_attempt_time = fasync::Time::now();
        let connect_request = types::ConnectRequest {
            target: types::ConnectionCandidate {
                network: id.clone(),
                credential: credential.clone(),
                scanned: None,
            },
            reason: types::ConnectReason::FidlConnectRequest,
        };

        // Set how the SavedNetworksManager should respond to lookup_compatible for the scan.
        let expected_config =
            network_config::NetworkConfig::new(id.clone().into(), credential.clone(), false)
                .expect("failed to create network config");
        test_values.saved_networks_manager.set_lookup_compatible_response(vec![expected_config]);

        let (connect_sender, mut connect_receiver) = oneshot::channel();
        let connecting_options = ConnectingOptions {
            connect_responder: Some(connect_sender),
            connect_request: connect_request.clone(),
            attempt_counter: 0,
        };
        let initial_state = connecting_state(test_values.common_options, connecting_options);
        let fut = run_state_machine(initial_state);
        pin_mut!(fut);

        // Run the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Ensure a scan request is sent to the SME and send back a result
        let expected_scan_request = fidl_sme::ScanRequest::Active(fidl_sme::ActiveScanRequest {
            ssids: vec![next_network_ssid.to_vec()],
            channels: vec![],
        });
        let bss_description = random_fidl_bss_description!(Wep, ssid: next_network_ssid.clone());
        let scan_results = vec![fidl_sme::ScanResult {
            compatible: true,
            timestamp_nanos: zx::Time::get_monotonic().into_nanos(),
            bss_description: bss_description.clone(),
        }];
        validate_sme_scan_request_and_send_results(
            &mut exec,
            &mut test_values.sme_req_stream,
            &expected_scan_request,
            scan_results,
        );

        // Run the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Ensure a connect request is sent to the SME
        let sme_fut = test_values.sme_req_stream.into_future();
        pin_mut!(sme_fut);
        let time_to_connect = 30.seconds();
        let connect_txn_handle = assert_variant!(
            poll_sme_req(&mut exec, &mut sme_fut),
            Poll::Ready(fidl_sme::ClientSmeRequest::Connect{ req, txn, control_handle: _ }) => {
                assert_eq!(req.ssid, next_network_ssid.to_vec());
                assert_eq!(connect_request.target.credential, req.authentication.credentials);
                assert_eq!(req.bss_description, bss_description.clone().into());
                assert_eq!(req.deprecated_scan_type, fidl_fuchsia_wlan_common::ScanType::Active);
                assert_eq!(req.multiple_bss_candidates, false);
                // Send connection response.
                exec.set_fake_time(fasync::Time::after(time_to_connect));
                let (_stream, ctrl) = txn.expect("connect txn unused")
                    .into_stream_and_control_handle().expect("error accessing control handle");
                ctrl
            }
        );
        connect_txn_handle
            .send_on_connect_result(&mut fake_successful_connect_result())
            .expect("failed to send connection completion");

        // Check for a connecting update
        let client_state_update = ClientStateUpdate {
            state: fidl_policy::WlanClientState::ConnectionsEnabled,
            networks: vec![ClientNetworkState {
                id: types::NetworkIdentifier {
                    ssid: next_network_ssid.clone(),
                    security_type: types::SecurityType::Wep,
                },
                state: fidl_policy::ConnectionState::Connecting,
                status: None,
            }],
        };
        assert_variant!(
            test_values.update_receiver.try_next(),
            Ok(Some(listener::Message::NotifyListeners(updates))) => {
            assert_eq!(updates, client_state_update);
        });

        // Check that NetworkSelectionDecision telemetry event is sent
        assert_variant!(test_values.telemetry_receiver.try_next(), Ok(Some(event)) => {
            assert_variant!(event, TelemetryEvent::NetworkSelectionDecision { .. });
        });

        // Progress the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Check the responder was acknowledged
        assert_variant!(exec.run_until_stalled(&mut connect_receiver), Poll::Ready(Ok(())));

        // Check for a connect update
        let client_state_update = ClientStateUpdate {
            state: fidl_policy::WlanClientState::ConnectionsEnabled,
            networks: vec![ClientNetworkState {
                id: types::NetworkIdentifier {
                    ssid: next_network_ssid.clone(),
                    security_type: types::SecurityType::Wep,
                },
                state: fidl_policy::ConnectionState::Connected,
                status: None,
            }],
        };
        assert_variant!(
            test_values.update_receiver.try_next(),
            Ok(Some(listener::Message::NotifyListeners(updates))) => {
            assert_eq!(updates, client_state_update);
        });

        // Check that the saved networks manager has the connection result recorded
        assert_variant!(test_values.saved_networks_manager.get_recorded_connect_reslts().as_slice(), [data] => {
            let expected_connect_result = ConnectResultRecord {
                 id: id.clone(),
                 credential: credential.clone(),
                 bssid: types::Bssid(bss_description.bssid),
                connect_result: fake_successful_connect_result(),
                 discovered_in_scan: None,
            };
            assert_eq!(data, &expected_connect_result);
        });

        // Check that connected telemetry event is sent
        assert_variant!(
            test_values.telemetry_receiver.try_next(),
            Ok(Some(TelemetryEvent::ConnectResult { iface_id: 1, policy_connect_reason, result, multiple_bss_candidates, latest_ap_state })) => {
                assert_eq!(bss_description, latest_ap_state.into());
                assert!(!multiple_bss_candidates);
                assert_eq!(policy_connect_reason, Some(types::ConnectReason::FidlConnectRequest));
                assert_eq!(result, fake_successful_connect_result());
            }
        );

        // Progress the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Ensure no further updates were sent to listeners
        assert_variant!(
            exec.run_until_stalled(&mut test_values.update_receiver.into_future()),
            Poll::Pending
        );

        // Send a disconnect and check that the connection data is correctly recorded
        let is_sme_reconnecting = false;
        let mut fidl_disconnect_info = generate_disconnect_info(is_sme_reconnecting);
        connect_txn_handle
            .send_on_disconnect(&mut fidl_disconnect_info)
            .expect("failed to send disconnection event");
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        let expected_recorded_connection = ConnectionRecord {
            id: id.clone(),
            credential: credential.clone(),
            data: PastConnectionData {
                bssid: types::Bssid(bss_description.bssid),
                connection_attempt_time,
                time_to_connect,
                disconnect_time: fasync::Time::now(),
                connection_uptime: zx::Duration::from_minutes(0),
                disconnect_reason: types::DisconnectReason::DisconnectDetectedFromSme,
                signal_data_at_disconnect: bss_selection::SignalData::new(
                    bss_description.rssi_dbm,
                    bss_description.snr_db,
                    bss_selection::EWMA_SMOOTHING_FACTOR,
                ),
                // TODO: record average phy rate over connection once available
                average_tx_rate: 0,
            },
        };
        assert_variant!(test_values.saved_networks_manager.get_recorded_past_connections().as_slice(), [data] => {
            assert_eq!(data, &expected_recorded_connection);
        });
    }

    #[test_case(types::SecurityType::Wpa3)]
    #[test_case(types::SecurityType::Wpa2)]
    #[fuchsia::test(add_test_attr = false)]
    fn connecting_state_successfully_connects_wpa2wpa3(type_: types::SecurityType) {
        let mut exec = fasync::TestExecutor::new().expect("failed to create an executor");
        // Do test set up manually to get stash server
        let (_client_req_sender, client_req_stream) = mpsc::channel(1);
        let (update_sender, _update_receiver) = mpsc::unbounded();
        let (sme_proxy, sme_server) =
            create_proxy::<fidl_sme::ClientSmeMarker>().expect("failed to create an sme channel");
        let mut sme_req_stream =
            sme_server.into_stream().expect("could not create SME request stream");
        let (saved_networks, mut stash_server) =
            exec.run_singlethreaded(SavedNetworksManager::new_and_stash_server());
        let saved_networks_manager = Arc::new(saved_networks);
        let (persistence_req_sender, _persistence_stream) = create_inspect_persistence_channel();
        let (telemetry_sender, _telemetry_receiver) = mpsc::channel::<TelemetryEvent>(100);
        let telemetry_sender = TelemetrySender::new(telemetry_sender);
        let (stats_sender, _stats_receiver) = mpsc::unbounded();
        let network_selector = Arc::new(network_selection::NetworkSelector::new(
            saved_networks_manager.clone(),
            create_wlan_hasher(),
            inspect::Inspector::new().root().create_child("network_selector"),
            persistence_req_sender,
            telemetry_sender.clone(),
        ));
        let next_network_ssid = types::Ssid::try_from("bar").unwrap();
        let connect_request = types::ConnectRequest {
            target: types::ConnectionCandidate {
                network: types::NetworkIdentifier {
                    ssid: next_network_ssid.clone(),
                    security_type: type_.into(),
                },
                credential: Credential::Password("Anything".as_bytes().to_vec()),
                scanned: None,
            },
            reason: types::ConnectReason::FidlConnectRequest,
        };

        // Store the network in the saved_networks_manager, so we can record connection success
        let save_fut = saved_networks_manager.store(
            connect_request.target.network.clone().into(),
            connect_request.target.credential.clone(),
        );
        pin_mut!(save_fut);
        assert_variant!(exec.run_until_stalled(&mut save_fut), Poll::Pending);
        process_stash_write(&mut exec, &mut stash_server);
        assert_variant!(exec.run_until_stalled(&mut save_fut), Poll::Ready(Ok(None)));

        // Check that the saved networks manager has the expected initial data
        let saved_networks = exec.run_singlethreaded(
            saved_networks_manager.lookup(&connect_request.target.network.clone().into()),
        );
        assert_eq!(false, saved_networks[0].has_ever_connected);
        assert!(saved_networks[0].hidden_probability > 0.0);

        let (connect_sender, _connect_receiver) = oneshot::channel();
        let connecting_options = ConnectingOptions {
            connect_responder: Some(connect_sender),
            connect_request: connect_request.clone(),
            attempt_counter: 0,
        };
        let common_options = CommonStateOptions {
            proxy: sme_proxy,
            req_stream: client_req_stream.fuse(),
            update_sender: update_sender,
            saved_networks_manager: saved_networks_manager.clone(),
            network_selector,
            telemetry_sender,
            iface_id: 1,
            has_wpa3_support: false,
            stats_sender,
        };
        let initial_state = connecting_state(common_options, connecting_options);
        let fut = run_state_machine(initial_state);
        pin_mut!(fut);

        // Run the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Ensure a scan request is sent to the SME and send back a result
        let expected_scan_request = fidl_sme::ScanRequest::Active(fidl_sme::ActiveScanRequest {
            ssids: vec![next_network_ssid.to_vec()],
            channels: vec![],
        });
        let bss_description =
            random_fidl_bss_description!(Wpa2Wpa3, ssid: next_network_ssid.clone());
        let scan_results = vec![fidl_sme::ScanResult {
            compatible: true,
            timestamp_nanos: zx::Time::get_monotonic().into_nanos(),
            bss_description: bss_description.clone(),
        }];
        validate_sme_scan_request_and_send_results(
            &mut exec,
            &mut sme_req_stream,
            &expected_scan_request,
            scan_results,
        );

        // Run the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Ensure the WPA2/WPA3 network was selected for connection and a connect request is sent to
        // the SME.
        let sme_fut = sme_req_stream.into_future();
        pin_mut!(sme_fut);
        assert_variant!(
            poll_sme_req(&mut exec, &mut sme_fut),
            Poll::Ready(fidl_sme::ClientSmeRequest::Connect{ req, .. }) => {
                assert_eq!(req.ssid, next_network_ssid.to_vec());
                assert_eq!(connect_request.target.credential, req.authentication.credentials);
                assert_eq!(req.bss_description, bss_description);
                assert_eq!(req.deprecated_scan_type, fidl_fuchsia_wlan_common::ScanType::Active);
            }
        );
    }

    /// Test parameters for authentication test cases.
    ///
    /// See the `connecting_state_select_authentication` test function.
    #[derive(Clone, Debug)]
    struct AuthenticationTestCase {
        credential: Credential,
        requested: types::SecurityType,
        scanned: types::SecurityTypeDetailed,
        has_wpa3_support: bool,
    }

    impl AuthenticationTestCase {
        fn open() -> Self {
            AuthenticationTestCase {
                credential: Credential::None,
                requested: types::SecurityType::None,
                scanned: types::SecurityTypeDetailed::Open,
                has_wpa3_support: false,
            }
        }

        fn wep_requested_wpa1_scanned() -> Self {
            AuthenticationTestCase {
                credential: wep_key(),
                requested: types::SecurityType::Wep,
                scanned: types::SecurityTypeDetailed::Wpa1,
                has_wpa3_support: false,
            }
        }

        fn wpa1_requested_wpa2_scanned() -> Self {
            AuthenticationTestCase {
                credential: wpa_psk(),
                requested: types::SecurityType::Wpa,
                scanned: types::SecurityTypeDetailed::Wpa2Personal,
                has_wpa3_support: false,
            }
        }

        fn wpa2_requested_open_scanned() -> Self {
            AuthenticationTestCase {
                credential: wpa_password(),
                requested: types::SecurityType::Wpa2,
                scanned: types::SecurityTypeDetailed::Open,
                has_wpa3_support: false,
            }
        }

        fn wpa2_requested_wpa3_scanned(
            has_wpa3_credential_support: bool,
            has_wpa3_hardware_support: bool,
        ) -> Self {
            AuthenticationTestCase {
                credential: if has_wpa3_credential_support { wpa_password() } else { wpa_psk() },
                requested: types::SecurityType::Wpa2,
                scanned: types::SecurityTypeDetailed::Wpa3Personal,
                has_wpa3_support: has_wpa3_hardware_support,
            }
        }

        fn wpa3_requested_wpa2_wpa3_scanned(has_wpa3_hardware_support: bool) -> Self {
            AuthenticationTestCase {
                credential: wpa_password(),
                requested: types::SecurityType::Wpa3,
                scanned: types::SecurityTypeDetailed::Wpa2Wpa3Personal,
                has_wpa3_support: has_wpa3_hardware_support,
            }
        }

        fn wpa2_wpa3_scanned(
            requested: types::SecurityType,
            has_wpa3_credential_support: bool,
            has_wpa3_hardware_support: bool,
        ) -> Self {
            AuthenticationTestCase {
                credential: if has_wpa3_credential_support { wpa_password() } else { wpa_psk() },
                requested,
                scanned: types::SecurityTypeDetailed::Wpa2Wpa3Personal,
                has_wpa3_support: has_wpa3_hardware_support,
            }
        }
    }

    // TODO(fxbug.dev/102196): Refactor this test into an a more end-to-end test against the higher
    //                         level client API (rather than testing directly against the state
    //                         machine).
    /// Tests for success and failure based on authentication parameters.
    ///
    /// This test exercises authentication (security protocol) selection in the state machine. The
    /// parameters in `AuthenticationTestCase` determine the security protocol and/or credentials
    /// of the network to which a connection is requested, the corresponding saved network, and the
    /// network discovered during the scan.
    // Expect successful connection for the following cases.
    #[test_case(AuthenticationTestCase::open())]
    #[test_case(AuthenticationTestCase::wpa1_requested_wpa2_scanned())]
    #[test_case(AuthenticationTestCase::wpa2_requested_wpa3_scanned(true, true))]
    #[test_case(AuthenticationTestCase::wpa3_requested_wpa2_wpa3_scanned(false))]
    #[test_case(AuthenticationTestCase::wpa3_requested_wpa2_wpa3_scanned(true))]
    #[test_case(AuthenticationTestCase::wpa2_wpa3_scanned(
        types::SecurityType::Wpa2,
        false,
        false
    ))]
    #[test_case(AuthenticationTestCase::wpa2_wpa3_scanned(types::SecurityType::Wpa2, true, false))]
    #[test_case(AuthenticationTestCase::wpa2_wpa3_scanned(types::SecurityType::Wpa2, false, true))]
    #[test_case(AuthenticationTestCase::wpa2_wpa3_scanned(types::SecurityType::Wpa2, true, true))]
    #[test_case(AuthenticationTestCase::wpa2_wpa3_scanned(types::SecurityType::Wpa3, true, false))]
    #[test_case(AuthenticationTestCase::wpa2_wpa3_scanned(types::SecurityType::Wpa3, true, true))]
    // Expect unsuccessful connection (panic) for the following cases.
    #[test_case(AuthenticationTestCase::wep_requested_wpa1_scanned() => panics)]
    #[test_case(AuthenticationTestCase::wpa2_requested_open_scanned() => panics)]
    #[test_case(AuthenticationTestCase::wpa2_requested_wpa3_scanned(false, false) => panics)]
    #[test_case(AuthenticationTestCase::wpa2_requested_wpa3_scanned(true, false) => panics)]
    #[test_case(AuthenticationTestCase::wpa2_requested_wpa3_scanned(false, true) => panics)]
    #[fuchsia::test(add_test_attr = false)]
    fn connecting_state_select_authentication(case: AuthenticationTestCase) {
        let mut executor = fasync::TestExecutor::new().expect("failed to create an executor");
        // Configure channels and WLAN components for the test. This test must save networks, so it
        // does not use the common setup functions seen in other tests in this module.
        let (_client_req_tx, client_req_rx) = mpsc::channel(1);
        let (update_tx, _update_rx) = mpsc::unbounded();
        let (sme_proxy, sme_server) =
            create_proxy::<fidl_sme::ClientSmeMarker>().expect("failed to create an sme channel");
        let mut sme_req_stream =
            sme_server.into_stream().expect("could not create SME request stream");
        let (saved_networks, mut stash_server) =
            executor.run_singlethreaded(SavedNetworksManager::new_and_stash_server());
        let saved_networks_manager = Arc::new(saved_networks);
        let (persistence_tx, _persistence_rx) = create_inspect_persistence_channel();
        let (telementry_tx, _telemetry_rx) = mpsc::channel::<TelemetryEvent>(100);
        let telemetry_sender = TelemetrySender::new(telementry_tx);
        let (stats_tx, _stats_rx) = mpsc::unbounded();
        let network_selector = Arc::new(network_selection::NetworkSelector::new(
            saved_networks_manager.clone(),
            create_wlan_hasher(),
            inspect::Inspector::new().root().create_child("network_selector"),
            persistence_tx,
            telemetry_sender.clone(),
        ));

        // Create an SSID and connect request for the requested network of the test case.
        let ssid = types::Ssid::try_from("test").unwrap();
        let id = types::NetworkIdentifier { ssid: ssid.clone(), security_type: case.requested };
        let connect_request = types::ConnectRequest {
            target: types::ConnectionCandidate {
                network: id.clone(),
                credential: case.credential.clone(),
                scanned: None,
            },
            reason: types::ConnectReason::FidlConnectRequest,
        };

        // Store the requested network of the test case.
        let store_future = saved_networks_manager.store(id, case.credential);
        pin_mut!(store_future);
        assert_variant!(executor.run_until_stalled(&mut store_future), Poll::Pending);
        process_stash_write(&mut executor, &mut stash_server);
        assert_variant!(executor.run_until_stalled(&mut store_future), Poll::Ready(Ok(None)));

        // Create a state machine in the connecting state and begin running it.
        let (connect_tx, _connect_rx) = oneshot::channel();
        let connecting_options = ConnectingOptions {
            connect_responder: Some(connect_tx),
            connect_request: connect_request.clone(),
            attempt_counter: 0,
        };
        let common_options = CommonStateOptions {
            proxy: sme_proxy,
            req_stream: client_req_rx.fuse(),
            update_sender: update_tx,
            saved_networks_manager: saved_networks_manager.clone(),
            network_selector,
            telemetry_sender,
            iface_id: 1,
            has_wpa3_support: case.has_wpa3_support,
            stats_sender: stats_tx,
        };
        let initial_state = connecting_state(common_options, connecting_options);
        let run_state_machine_future = run_state_machine(initial_state);
        pin_mut!(run_state_machine_future);
        assert_variant!(executor.run_until_stalled(&mut run_state_machine_future), Poll::Pending);

        // Report a network with the test case SSID and security protocol in the scan result from
        // SME.
        let expected_scan_request = fidl_sme::ScanRequest::Active(fidl_sme::ActiveScanRequest {
            ssids: vec![ssid.to_vec()],
            channels: vec![],
        });
        let bss_description =
            random_fidl_bss_description!(protection => case.scanned, ssid: ssid.clone());
        let scan_results = vec![fidl_sme::ScanResult {
            compatible: match case.scanned {
                types::SecurityTypeDetailed::Wpa3Personal
                | types::SecurityTypeDetailed::Wpa3Enterprise => case.has_wpa3_support,
                _ => true,
            },
            timestamp_nanos: zx::Time::get_monotonic().into_nanos(),
            bss_description: bss_description.clone(),
        }];
        validate_sme_scan_request_and_send_results(
            &mut executor,
            &mut sme_req_stream,
            &expected_scan_request,
            scan_results,
        );
        assert_variant!(executor.run_until_stalled(&mut run_state_machine_future), Poll::Pending);

        // Assert that SME is sent a connect request.
        let sme_req_future = sme_req_stream.into_future();
        pin_mut!(sme_req_future);
        assert_variant!(
            poll_sme_req(&mut executor, &mut sme_req_future),
            Poll::Ready(fidl_sme::ClientSmeRequest::Connect{ req, .. }) => {
                assert_eq!(req.ssid, ssid.to_vec());
                assert_eq!(connect_request.target.credential, req.authentication.credentials);
                assert_eq!(req.bss_description, bss_description);
                assert_eq!(req.deprecated_scan_type, fidl_fuchsia_wlan_common::ScanType::Active);
            }
        );
    }

    // TODO(fxbug.dev/92693): Test with WPA Enterprise security protocols.
    /// Tests for success when forwarding an authentication method in a scanned candidate.
    #[test_case(Credential::None, types::SecurityTypeDetailed::Open, false)]
    #[test_case(wep_key(), types::SecurityTypeDetailed::Wep, false)]
    #[test_case(wpa_psk(), types::SecurityTypeDetailed::Wpa1, false)]
    #[test_case(wpa_psk(), types::SecurityTypeDetailed::Wpa1Wpa2PersonalTkipOnly, false)]
    #[test_case(wpa_psk(), types::SecurityTypeDetailed::Wpa1Wpa2Personal, false)]
    #[test_case(wpa_psk(), types::SecurityTypeDetailed::Wpa2PersonalTkipOnly, false)]
    #[test_case(wpa_psk(), types::SecurityTypeDetailed::Wpa2Personal, false)]
    #[test_case(wpa_password(), types::SecurityTypeDetailed::Wpa2Wpa3Personal, false)]
    #[test_case(wpa_psk(), types::SecurityTypeDetailed::Wpa2Wpa3Personal, false)]
    #[test_case(wpa_password(), types::SecurityTypeDetailed::Wpa2Wpa3Personal, true)]
    #[test_case(wpa_psk(), types::SecurityTypeDetailed::Wpa2Wpa3Personal, true)]
    #[test_case(wpa_password(), types::SecurityTypeDetailed::Wpa3Personal, true)]
    #[fuchsia::test(add_test_attr = false)]
    fn connecting_state_forward_authentication(
        credential: Credential,
        scanned: types::SecurityTypeDetailed,
        has_wpa3_support: bool,
    ) {
        let mut executor = fasync::TestExecutor::new().expect("failed to create an executor");
        // Configure channels and WLAN components for the test. This test must save networks, so it
        // does not use the common setup functions seen in other tests in this module.
        let (_client_req_tx, client_req_rx) = mpsc::channel(1);
        let (update_tx, _update_rx) = mpsc::unbounded();
        let (sme_proxy, sme_server) =
            create_proxy::<fidl_sme::ClientSmeMarker>().expect("failed to create an sme channel");
        let sme_req_stream = sme_server.into_stream().expect("could not create SME request stream");
        let (saved_networks, mut stash_server) =
            executor.run_singlethreaded(SavedNetworksManager::new_and_stash_server());
        let saved_networks_manager = Arc::new(saved_networks);
        let (persistence_tx, _persistence_rx) = create_inspect_persistence_channel();
        let (telementry_tx, _telemetry_rx) = mpsc::channel::<TelemetryEvent>(100);
        let telemetry_sender = TelemetrySender::new(telementry_tx);
        let (stats_tx, _stats_rx) = mpsc::unbounded();
        let network_selector = Arc::new(network_selection::NetworkSelector::new(
            saved_networks_manager.clone(),
            create_wlan_hasher(),
            inspect::Inspector::new().root().create_child("network_selector"),
            persistence_tx,
            telemetry_sender.clone(),
        ));

        // Create an SSID and connect request for the requested network of the test case. Note that
        // the security types of the network identifier and BSS description need not corroborate
        // the scanned candidate; these security protocol specifications should not interact in the
        // state machine when a scanned candidate is provided.
        let ssid = types::Ssid::try_from("test").unwrap();
        let id = types::NetworkIdentifier {
            ssid: ssid.clone(),
            security_type: match credential {
                Credential::None => types::SecurityType::None,
                Credential::Password(_) | Credential::Psk(_) => types::SecurityType::Wpa,
            },
        };
        let bss_description = random_fidl_bss_description!(Open, ssid: ssid.clone());
        let connect_request = types::ConnectRequest {
            target: types::ConnectionCandidate {
                network: id.clone(),
                credential: credential.clone(),
                scanned: Some(types::ScannedCandidate {
                    bss_description: bss_description.clone(),
                    observation: types::ScanObservation::Active,
                    has_multiple_bss_candidates: false,
                    security_type_detailed: scanned,
                }),
            },
            reason: types::ConnectReason::FidlConnectRequest,
        };

        // Store the requested network of the test case.
        let store_future = saved_networks_manager.store(id, credential);
        pin_mut!(store_future);
        assert_variant!(executor.run_until_stalled(&mut store_future), Poll::Pending);
        process_stash_write(&mut executor, &mut stash_server);
        assert_variant!(executor.run_until_stalled(&mut store_future), Poll::Ready(Ok(None)));

        // Create a state machine in the connecting state and begin running it.
        let (connect_tx, _connect_rx) = oneshot::channel();
        let connecting_options = ConnectingOptions {
            connect_responder: Some(connect_tx),
            connect_request: connect_request.clone(),
            attempt_counter: 0,
        };
        let common_options = CommonStateOptions {
            proxy: sme_proxy,
            req_stream: client_req_rx.fuse(),
            update_sender: update_tx,
            saved_networks_manager: saved_networks_manager.clone(),
            network_selector,
            telemetry_sender,
            iface_id: 1,
            has_wpa3_support,
            stats_sender: stats_tx,
        };
        let initial_state = connecting_state(common_options, connecting_options);
        let run_state_machine_future = run_state_machine(initial_state);
        pin_mut!(run_state_machine_future);
        assert_variant!(executor.run_until_stalled(&mut run_state_machine_future), Poll::Pending);

        // Assert that SME is sent a connect request.
        let sme_req_future = sme_req_stream.into_future();
        pin_mut!(sme_req_future);
        assert_variant!(
            poll_sme_req(&mut executor, &mut sme_req_future),
            Poll::Ready(fidl_sme::ClientSmeRequest::Connect{ req, .. }) => {
                assert_eq!(req.ssid, ssid.to_vec());
                assert_eq!(connect_request.target.credential, req.authentication.credentials);
                assert_eq!(req.bss_description, bss_description);
                assert_eq!(req.deprecated_scan_type, fidl_fuchsia_wlan_common::ScanType::Active);
            }
        );
    }

    #[fuchsia::test]
    fn connecting_state_fails_to_connect_and_retries() {
        let mut exec = fasync::TestExecutor::new().expect("failed to create an executor");
        let mut test_values = test_setup();

        let next_network_ssid = types::Ssid::try_from("bar").unwrap();
        let bss_description = random_bss_description!(Wpa2, ssid: next_network_ssid.clone());
        let connect_request = types::ConnectRequest {
            target: types::ConnectionCandidate {
                network: types::NetworkIdentifier {
                    ssid: next_network_ssid.clone(),
                    security_type: types::SecurityType::Wpa2,
                },
                credential: Credential::Password(b"password".to_vec()),
                scanned: Some(types::ScannedCandidate {
                    bss_description: bss_description.clone().into(),
                    observation: types::ScanObservation::Passive,
                    has_multiple_bss_candidates: true,
                    security_type_detailed: types::SecurityTypeDetailed::Wpa2Personal,
                }),
            },
            reason: types::ConnectReason::FidlConnectRequest,
        };
        let (connect_sender, mut connect_receiver) = oneshot::channel();
        let connecting_options = ConnectingOptions {
            connect_responder: Some(connect_sender),
            connect_request: connect_request.clone(),
            attempt_counter: 0,
        };
        let initial_state = connecting_state(test_values.common_options, connecting_options);
        let fut = run_state_machine(initial_state);
        pin_mut!(fut);
        let sme_fut = test_values.sme_req_stream.into_future();
        pin_mut!(sme_fut);

        // Run the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Check the responder was acknowledged
        assert_variant!(exec.run_until_stalled(&mut connect_receiver), Poll::Ready(Ok(())));

        // Ensure a connect request is sent to the SME
        let mut connect_txn_handle = assert_variant!(
            poll_sme_req(&mut exec, &mut sme_fut),
            Poll::Ready(fidl_sme::ClientSmeRequest::Connect{ req, txn, control_handle: _ }) => {
                assert_eq!(req.ssid, next_network_ssid.to_vec());
                 // Send connection response.
                let (_stream, ctrl) = txn.expect("connect txn unused")
                    .into_stream_and_control_handle().expect("error accessing control handle");
                ctrl
            }
        );
        let mut connect_result = fidl_sme::ConnectResult {
            code: fidl_ieee80211::StatusCode::RefusedReasonUnspecified,
            ..fake_successful_connect_result()
        };
        connect_txn_handle
            .send_on_connect_result(&mut connect_result)
            .expect("failed to send connection completion");

        // Check for a connecting update
        let client_state_update = ClientStateUpdate {
            state: fidl_policy::WlanClientState::ConnectionsEnabled,
            networks: vec![ClientNetworkState {
                id: types::NetworkIdentifier {
                    ssid: next_network_ssid.clone(),
                    security_type: types::SecurityType::Wpa2,
                },
                state: fidl_policy::ConnectionState::Connecting,
                status: None,
            }],
        };
        assert_variant!(
            test_values.update_receiver.try_next(),
            Ok(Some(listener::Message::NotifyListeners(updates))) => {
            assert_eq!(updates, client_state_update);
        });

        // Progress the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);
        assert!(exec.wake_next_timer().is_some());
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Check that connect result telemetry event is sent
        assert_variant!(
            test_values.telemetry_receiver.try_next(),
            Ok(Some(TelemetryEvent::ConnectResult { iface_id: 1, policy_connect_reason, result, multiple_bss_candidates, latest_ap_state })) => {
                assert_eq!(bss_description, latest_ap_state);
                assert!(multiple_bss_candidates);
                assert_eq!(policy_connect_reason, Some(types::ConnectReason::FidlConnectRequest));
                assert_eq!(result, connect_result);
            }
        );

        // Ensure a disconnect request is sent to the SME
        assert_variant!(
            poll_sme_req(&mut exec, &mut sme_fut),
            Poll::Ready(fidl_sme::ClientSmeRequest::Disconnect{ responder, reason: fidl_sme::UserDisconnectReason::FailedToConnect }) => {
                responder.send().expect("could not send sme response");
            }
        );

        // Progress the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Ensure a connect request is sent to the SME
        connect_txn_handle = assert_variant!(
            poll_sme_req(&mut exec, &mut sme_fut),
            Poll::Ready(fidl_sme::ClientSmeRequest::Connect{ req, txn, control_handle: _ }) => {
                assert_eq!(req.ssid, next_network_ssid.to_vec());
                assert_eq!(req.bss_description, bss_description.clone().into());
                assert_eq!(req.multiple_bss_candidates, true);
                 // Send connection response.
                let (_stream, ctrl) = txn.expect("connect txn unused")
                    .into_stream_and_control_handle().expect("error accessing control handle");
                ctrl
            }
        );
        let mut connect_result = fake_successful_connect_result();
        connect_txn_handle
            .send_on_connect_result(&mut connect_result)
            .expect("failed to send connection completion");

        // Progress the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // The state machine should have sent a listener update
        assert_variant!(
            test_values.update_receiver.try_next(),
            Ok(Some(listener::Message::NotifyListeners(ClientStateUpdate {
                state: fidl_policy::WlanClientState::ConnectionsEnabled,
                networks
            }))) => {
                assert!(networks.is_empty());
            }
        );

        // Check for a connected update
        let client_state_update = ClientStateUpdate {
            state: fidl_policy::WlanClientState::ConnectionsEnabled,
            networks: vec![ClientNetworkState {
                id: types::NetworkIdentifier {
                    ssid: next_network_ssid.clone(),
                    security_type: types::SecurityType::Wpa2,
                },
                state: fidl_policy::ConnectionState::Connected,
                status: None,
            }],
        };
        assert_variant!(
            test_values.update_receiver.try_next(),
            Ok(Some(listener::Message::NotifyListeners(updates))) => {
            assert_eq!(updates, client_state_update);
        });

        // Progress the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Ensure no further updates were sent to listeners
        assert_variant!(
            exec.run_until_stalled(&mut test_values.update_receiver.into_future()),
            Poll::Pending
        );
    }

    #[fuchsia::test]
    fn connecting_state_fails_to_scan_and_retries() {
        let mut exec = fasync::TestExecutor::new().expect("failed to create an executor");
        // Do test set up manually to get stash server
        let (_client_req_sender, client_req_stream) = mpsc::channel(1);
        let (update_sender, mut update_receiver) = mpsc::unbounded();
        let (sme_proxy, sme_server) =
            create_proxy::<fidl_sme::ClientSmeMarker>().expect("failed to create an sme channel");
        let sme_req_stream = sme_server.into_stream().expect("could not create SME request stream");
        let (saved_networks, mut stash_server) =
            exec.run_singlethreaded(SavedNetworksManager::new_and_stash_server());
        let saved_networks_manager = Arc::new(saved_networks);
        let (persistence_req_sender, _persistence_stream) = create_inspect_persistence_channel();
        let (telemetry_sender, _telemetry_receiver) = mpsc::channel::<TelemetryEvent>(100);
        let telemetry_sender = TelemetrySender::new(telemetry_sender);
        let (stats_sender, _stats_receiver) = mpsc::unbounded();
        let network_selector = Arc::new(network_selection::NetworkSelector::new(
            saved_networks_manager.clone(),
            create_wlan_hasher(),
            inspect::Inspector::new().root().create_child("network_selector"),
            persistence_req_sender,
            telemetry_sender.clone(),
        ));
        let next_network_ssid = types::Ssid::try_from("bar").unwrap();
        let connect_request = types::ConnectRequest {
            target: types::ConnectionCandidate {
                network: types::NetworkIdentifier {
                    ssid: next_network_ssid.clone(),
                    security_type: types::SecurityType::Wpa2,
                },
                credential: Credential::Password("Anything".as_bytes().to_vec()),
                scanned: None,
            },
            reason: types::ConnectReason::FidlConnectRequest,
        };

        // Store the network in the saved_networks_manager, so we can record connection success
        let save_fut = saved_networks_manager.store(
            connect_request.target.network.clone().into(),
            connect_request.target.credential.clone(),
        );
        pin_mut!(save_fut);
        assert_variant!(exec.run_until_stalled(&mut save_fut), Poll::Pending);
        process_stash_write(&mut exec, &mut stash_server);
        assert_variant!(exec.run_until_stalled(&mut save_fut), Poll::Ready(Ok(None)));

        let (connect_sender, mut connect_receiver) = oneshot::channel();
        let connecting_options = ConnectingOptions {
            connect_responder: Some(connect_sender),
            connect_request: connect_request.clone(),
            attempt_counter: 0,
        };
        let common_options = CommonStateOptions {
            proxy: sme_proxy,
            req_stream: client_req_stream.fuse(),
            update_sender: update_sender,
            saved_networks_manager: saved_networks_manager.clone(),
            network_selector,
            telemetry_sender,
            iface_id: 1,
            has_wpa3_support: false,
            stats_sender,
        };
        let initial_state = connecting_state(common_options, connecting_options);
        let fut = run_state_machine(initial_state);
        pin_mut!(fut);

        // Run the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Check the responder was acknowledged
        assert_variant!(exec.run_until_stalled(&mut connect_receiver), Poll::Ready(Ok(())));

        // Ensure a scan request is sent to the SME
        let sme_fut = sme_req_stream.into_future();
        pin_mut!(sme_fut);
        assert_variant!(
            poll_sme_req(&mut exec, &mut sme_fut),
            Poll::Ready(fidl_sme::ClientSmeRequest::Scan{ txn, .. }) => {
                // Send failed scan response.
                let (_stream, ctrl) = txn
                    .into_stream_and_control_handle().expect("error accessing control handle");
                ctrl.send_on_error(&mut fidl_sme::ScanError {
                    code: fidl_sme::ScanErrorCode::InternalError,
                    message: "Failed to scan".to_string()
                })
                    .expect("failed to send scan error");
            }
        );

        // Check for a connecting update
        let client_state_update = ClientStateUpdate {
            state: fidl_policy::WlanClientState::ConnectionsEnabled,
            networks: vec![ClientNetworkState {
                id: types::NetworkIdentifier {
                    ssid: next_network_ssid.clone(),
                    security_type: types::SecurityType::Wpa2,
                },
                state: fidl_policy::ConnectionState::Connecting,
                status: None,
            }],
        };
        assert_variant!(
            update_receiver.try_next(),
            Ok(Some(listener::Message::NotifyListeners(updates))) => {
            assert_eq!(updates, client_state_update);
        });

        // Progress the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);
        assert!(exec.wake_next_timer().is_some());
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Ensure a disconnect request is sent to the SME
        assert_variant!(
            poll_sme_req(&mut exec, &mut sme_fut),
            Poll::Ready(fidl_sme::ClientSmeRequest::Disconnect{ responder, reason: fidl_sme::UserDisconnectReason::FailedToConnect }) => {
                responder.send().expect("could not send sme response");
            }
        );

        // Progress the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Ensure a new scan request is sent to the SME
        let expected_scan_request = fidl_sme::ScanRequest::Active(fidl_sme::ActiveScanRequest {
            ssids: vec![next_network_ssid.to_vec()],
            channels: vec![],
        });
        let bss_description = random_fidl_bss_description!(Wpa2, ssid: next_network_ssid.clone());
        let mut scan_results = vec![fidl_sme::ScanResult {
            compatible: true,
            timestamp_nanos: zx::Time::get_monotonic().into_nanos(),
            bss_description: bss_description.clone(),
        }];
        assert_variant!(
            poll_sme_req(&mut exec, &mut sme_fut),
            Poll::Ready(fidl_sme::ClientSmeRequest::Scan {
                txn, req, control_handle: _
            }) => {
                // Validate the request
                assert_eq!(req, expected_scan_request);
                // Send all the APs
                let (_stream, ctrl) = txn
                    .into_stream_and_control_handle().expect("error accessing control handle");
                ctrl.send_on_result(&mut scan_results.iter_mut())
                    .expect("failed to send scan data");

                // Send the end of data
                ctrl.send_on_finished()
                    .expect("failed to send scan data");
            }
        );

        // Run the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Ensure a connect request is sent to the SME
        assert_variant!(
            poll_sme_req(&mut exec, &mut sme_fut),
            Poll::Ready(fidl_sme::ClientSmeRequest::Connect{ req, txn, control_handle: _ }) => {
                assert_eq!(req.ssid, next_network_ssid.to_vec());
                assert_eq!(connect_request.target.credential, req.authentication.credentials);
                assert_eq!(req.bss_description, bss_description);
                assert_eq!(req.deprecated_scan_type, fidl_fuchsia_wlan_common::ScanType::Active);
                assert_eq!(req.multiple_bss_candidates, false);
                 // Send connection response.
                let (_stream, ctrl) = txn.expect("connect txn unused")
                    .into_stream_and_control_handle().expect("error accessing control handle");
                ctrl
                    .send_on_connect_result(&mut fake_successful_connect_result())
                    .expect("failed to send connection completion");
            }
        );
    }

    #[fuchsia::test]
    fn connecting_state_fails_to_connect_at_max_retries() {
        let mut exec = fasync::TestExecutor::new().expect("failed to create an executor");
        // Don't use test_values() because of issue with KnownEssStore
        let (update_sender, mut update_receiver) = mpsc::unbounded();
        let (sme_proxy, sme_server) =
            create_proxy::<fidl_sme::ClientSmeMarker>().expect("failed to create an sme channel");
        let sme_req_stream = sme_server.into_stream().expect("could not create SME request stream");
        let saved_networks_manager = Arc::new(
            exec.run_singlethreaded(SavedNetworksManager::new_for_test())
                .expect("Failed to create saved networks manager"),
        );
        let (_client_req_sender, client_req_stream) = mpsc::channel(1);
        let (persistence_req_sender, _persistence_stream) = create_inspect_persistence_channel();
        let (telemetry_sender, _telemetry_receiver) = mpsc::channel::<TelemetryEvent>(100);
        let telemetry_sender = TelemetrySender::new(telemetry_sender);
        let (stats_sender, _stats_receiver) = mpsc::unbounded();
        let network_selector = Arc::new(network_selection::NetworkSelector::new(
            saved_networks_manager.clone(),
            create_wlan_hasher(),
            inspect::Inspector::new().root().create_child("network_selector"),
            persistence_req_sender,
            telemetry_sender.clone(),
        ));
        let common_options = CommonStateOptions {
            proxy: sme_proxy,
            req_stream: client_req_stream.fuse(),
            update_sender: update_sender,
            saved_networks_manager: saved_networks_manager.clone(),
            network_selector,
            telemetry_sender,
            iface_id: 1,
            has_wpa3_support: false,
            stats_sender,
        };

        let next_network_ssid = types::Ssid::try_from("bar").unwrap();
        let next_security_type = types::SecurityType::None;
        let next_credential = Credential::None;
        let next_network_identifier = types::NetworkIdentifier {
            ssid: next_network_ssid.clone(),
            security_type: next_security_type,
        };
        let config_net_id =
            network_config::NetworkIdentifier::from(next_network_identifier.clone());
        let bss_description = random_fidl_bss_description!(Open, ssid: next_network_ssid.clone());
        // save network to check that failed connect is recorded
        assert!(exec
            .run_singlethreaded(
                saved_networks_manager.store(config_net_id.clone(), next_credential.clone()),
            )
            .expect("Failed to save network")
            .is_none());
        let before_recording = fasync::Time::now();

        let connect_request = types::ConnectRequest {
            target: types::ConnectionCandidate {
                network: next_network_identifier,
                credential: next_credential,
                scanned: Some(types::ScannedCandidate {
                    bss_description: bss_description.clone(),
                    observation: types::ScanObservation::Passive,
                    has_multiple_bss_candidates: true,
                    security_type_detailed: types::SecurityTypeDetailed::Open,
                }),
            },
            reason: types::ConnectReason::FidlConnectRequest,
        };
        let (connect_sender, mut connect_receiver) = oneshot::channel();
        let connecting_options = ConnectingOptions {
            connect_responder: Some(connect_sender),
            connect_request: connect_request.clone(),
            attempt_counter: MAX_CONNECTION_ATTEMPTS - 1,
        };
        let initial_state = connecting_state(common_options, connecting_options);
        let fut = run_state_machine(initial_state);
        pin_mut!(fut);
        let sme_fut = sme_req_stream.into_future();
        pin_mut!(sme_fut);

        // Run the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Check the responder was acknowledged
        assert_variant!(exec.run_until_stalled(&mut connect_receiver), Poll::Ready(Ok(())));

        // Ensure a connect request is sent to the SME
        assert_variant!(
            poll_sme_req(&mut exec, &mut sme_fut),
            Poll::Ready(fidl_sme::ClientSmeRequest::Connect{ req, txn, control_handle: _ }) => {
                assert_eq!(req.ssid, next_network_ssid.to_vec());
                assert_eq!(connect_request.target.credential, req.authentication.credentials);
                assert_eq!(req.bss_description, bss_description.clone());
                assert_eq!(req.deprecated_scan_type, fidl_fuchsia_wlan_common::ScanType::Active);
                assert_eq!(req.multiple_bss_candidates, true);
                 // Send connection response.
                let (_stream, ctrl) = txn.expect("connect txn unused")
                    .into_stream_and_control_handle().expect("error accessing control handle");
                let mut connect_result = fidl_sme::ConnectResult {
                    code: fidl_ieee80211::StatusCode::RefusedReasonUnspecified,
                    ..fake_successful_connect_result()
                };
                ctrl
                    .send_on_connect_result(&mut connect_result)
                    .expect("failed to send connection completion");
            }
        );

        // After failing to reconnect, the state machine should exit so that the state machine
        // monitor can attempt to reconnect the interface.
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Ready(()));

        // Check for a connect update
        let client_state_update = ClientStateUpdate {
            state: fidl_policy::WlanClientState::ConnectionsEnabled,
            networks: vec![ClientNetworkState {
                id: types::NetworkIdentifier {
                    ssid: next_network_ssid.clone(),
                    security_type: next_security_type,
                },
                state: fidl_policy::ConnectionState::Failed,
                status: Some(fidl_policy::DisconnectStatus::ConnectionFailed),
            }],
        };
        assert_variant!(
            update_receiver.try_next(),
            Ok(Some(listener::Message::NotifyListeners(updates))) => {
            assert_eq!(updates, client_state_update);
        });

        // Check that failure was recorded in SavedNetworksManager
        let mut configs = exec.run_singlethreaded(saved_networks_manager.lookup(&config_net_id));
        let network_config = configs.pop().expect("Failed to get saved network");
        let mut failures =
            network_config.perf_stats.connect_failures.get_recent_for_network(before_recording);
        let connect_failure = failures.pop().expect("Saved network is missing failure reason");
        assert_eq!(connect_failure.reason, FailureReason::GeneralFailure);
    }

    #[fuchsia::test]
    fn connecting_state_fails_to_connect_with_bad_credentials() {
        let mut exec = fasync::TestExecutor::new().expect("failed to create an executor");
        // Don't use test_values() because of issue with KnownEssStore
        let (update_sender, mut update_receiver) = mpsc::unbounded();
        let (sme_proxy, sme_server) =
            create_proxy::<fidl_sme::ClientSmeMarker>().expect("failed to create an sme channel");
        let sme_req_stream = sme_server.into_stream().expect("could not create SME request stream");
        let saved_networks_manager = Arc::new(
            exec.run_singlethreaded(SavedNetworksManager::new_for_test())
                .expect("Failed to create saved networks manager"),
        );
        let (_client_req_sender, client_req_stream) = mpsc::channel(1);
        let (persistence_req_sender, _persistence_stream) = create_inspect_persistence_channel();
        let (telemetry_sender, _telemetry_receiver) = mpsc::channel::<TelemetryEvent>(100);
        let telemetry_sender = TelemetrySender::new(telemetry_sender);
        let (stats_sender, _stats_receiver) = mpsc::unbounded();
        let network_selector = Arc::new(network_selection::NetworkSelector::new(
            saved_networks_manager.clone(),
            create_wlan_hasher(),
            inspect::Inspector::new().root().create_child("network_selector"),
            persistence_req_sender,
            telemetry_sender.clone(),
        ));

        let common_options = CommonStateOptions {
            proxy: sme_proxy,
            req_stream: client_req_stream.fuse(),
            update_sender: update_sender,
            saved_networks_manager: saved_networks_manager.clone(),
            network_selector,
            telemetry_sender,
            iface_id: 1,
            has_wpa3_support: false,
            stats_sender,
        };

        let next_network_ssid = types::Ssid::try_from("bar").unwrap();
        let next_network_identifier = types::NetworkIdentifier {
            ssid: next_network_ssid.clone(),
            security_type: types::SecurityType::Wpa2,
        };
        let config_net_id =
            network_config::NetworkIdentifier::from(next_network_identifier.clone());
        let next_credential = Credential::Password("password".as_bytes().to_vec());
        // save network to check that failed connect is recorded
        let saved_networks_manager = common_options.saved_networks_manager.clone();
        assert!(exec
            .run_singlethreaded(
                saved_networks_manager.store(config_net_id.clone(), next_credential.clone()),
            )
            .expect("Failed to save network")
            .is_none());
        let before_recording = fasync::Time::now();

        let bss_description = random_fidl_bss_description!(Wpa2, ssid: next_network_ssid.clone());
        let connect_request = types::ConnectRequest {
            target: types::ConnectionCandidate {
                network: next_network_identifier,
                credential: next_credential,
                scanned: Some(types::ScannedCandidate {
                    bss_description: bss_description.clone(),
                    observation: types::ScanObservation::Passive,
                    has_multiple_bss_candidates: true,
                    security_type_detailed: types::SecurityTypeDetailed::Wpa2Personal,
                }),
            },
            reason: types::ConnectReason::ProactiveNetworkSwitch,
        };
        let (connect_sender, mut connect_receiver) = oneshot::channel();
        let connecting_options = ConnectingOptions {
            connect_responder: Some(connect_sender),
            connect_request: connect_request.clone(),
            attempt_counter: MAX_CONNECTION_ATTEMPTS - 1,
        };
        let initial_state = connecting_state(common_options, connecting_options);
        let fut = run_state_machine(initial_state);
        pin_mut!(fut);
        let sme_fut = sme_req_stream.into_future();
        pin_mut!(sme_fut);

        // Run the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Check the responder was acknowledged
        assert_variant!(exec.run_until_stalled(&mut connect_receiver), Poll::Ready(Ok(())));

        // Ensure a connect request is sent to the SME
        assert_variant!(
            poll_sme_req(&mut exec, &mut sme_fut),
            Poll::Ready(fidl_sme::ClientSmeRequest::Connect{ req, txn, control_handle: _ }) => {
                assert_eq!(req.ssid, next_network_ssid.to_vec());
                assert_eq!(connect_request.target.credential, req.authentication.credentials);
                assert_eq!(req.bss_description, bss_description.clone());
                assert_eq!(req.deprecated_scan_type, fidl_fuchsia_wlan_common::ScanType::Active);
                assert_eq!(req.multiple_bss_candidates, true);
                 // Send connection response.
                let (_stream, ctrl) = txn.expect("connect txn unused")
                    .into_stream_and_control_handle().expect("error accessing control handle");
                let mut connect_result = fidl_sme::ConnectResult {
                    code: fidl_ieee80211::StatusCode::RefusedReasonUnspecified,
                    is_credential_rejected: true,
                    ..fake_successful_connect_result()
                };
                ctrl
                    .send_on_connect_result(&mut connect_result)
                    .expect("failed to send connection completion");
            }
        );

        // The state machine should exit when bad credentials are detected so that the state
        // machine monitor can try to connect to another network.
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Ready(()));

        // Check for a connect update
        let client_state_update = ClientStateUpdate {
            state: fidl_policy::WlanClientState::ConnectionsEnabled,
            networks: vec![ClientNetworkState {
                id: types::NetworkIdentifier {
                    ssid: next_network_ssid.clone(),
                    security_type: types::SecurityType::Wpa2,
                },
                state: fidl_policy::ConnectionState::Failed,
                status: Some(fidl_policy::DisconnectStatus::CredentialsFailed),
            }],
        };
        assert_variant!(
            update_receiver.try_next(),
            Ok(Some(listener::Message::NotifyListeners(updates))) => {
            assert_eq!(updates, client_state_update);
        });

        // Check that failure was recorded in SavedNetworksManager
        let mut configs = exec.run_singlethreaded(saved_networks_manager.lookup(&config_net_id));
        let network_config = configs.pop().expect("Failed to get saved network");
        let mut failures =
            network_config.perf_stats.connect_failures.get_recent_for_network(before_recording);
        let connect_failure = failures.pop().expect("Saved network is missing failure reason");
        assert_eq!(connect_failure.reason, FailureReason::CredentialRejected);
    }

    #[fuchsia::test]
    fn connecting_state_gets_duplicate_connect_request() {
        let mut exec = fasync::TestExecutor::new().expect("failed to create an executor");
        let mut test_values = test_setup();

        let next_network_ssid = types::Ssid::try_from("bar").unwrap();
        let bss_description = random_fidl_bss_description!(Wpa2, ssid: next_network_ssid.clone());
        let connect_request = types::ConnectRequest {
            target: types::ConnectionCandidate {
                network: types::NetworkIdentifier {
                    ssid: next_network_ssid.clone(),
                    security_type: types::SecurityType::Wpa2,
                },
                credential: Credential::Password(b"password".to_vec()),
                scanned: Some(types::ScannedCandidate {
                    bss_description: bss_description.clone(),
                    observation: types::ScanObservation::Passive,
                    has_multiple_bss_candidates: true,
                    security_type_detailed: types::SecurityTypeDetailed::Wpa2Personal,
                }),
            },
            reason: types::ConnectReason::RegulatoryChangeReconnect,
        };
        let (connect_sender, mut connect_receiver) = oneshot::channel();
        let connecting_options = ConnectingOptions {
            connect_responder: Some(connect_sender),
            connect_request: connect_request.clone(),
            attempt_counter: 0,
        };
        let initial_state = connecting_state(test_values.common_options, connecting_options);
        let fut = run_state_machine(initial_state);
        pin_mut!(fut);
        let sme_fut = test_values.sme_req_stream.into_future();
        pin_mut!(sme_fut);

        // Run the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Check the responder was acknowledged
        assert_variant!(exec.run_until_stalled(&mut connect_receiver), Poll::Ready(Ok(())));

        // Check for a connecting update
        let client_state_update = ClientStateUpdate {
            state: fidl_policy::WlanClientState::ConnectionsEnabled,
            networks: vec![ClientNetworkState {
                id: types::NetworkIdentifier {
                    ssid: next_network_ssid.clone(),
                    security_type: types::SecurityType::Wpa2,
                },
                state: fidl_policy::ConnectionState::Connecting,
                status: None,
            }],
        };
        assert_variant!(
            test_values.update_receiver.try_next(),
            Ok(Some(listener::Message::NotifyListeners(updates))) => {
            assert_eq!(updates, client_state_update);
        });

        // Send a duplicate connect request
        let mut client = Client::new(test_values.client_req_sender);
        let (connect_sender2, mut connect_receiver2) = oneshot::channel();
        let duplicate_request = types::ConnectRequest {
            target: {
                types::ConnectionCandidate {
                    scanned: None, // this incoming request should be deduped regardless of the bss info
                    ..connect_request.clone().target
                }
            },
            // this incoming request should be deduped regardless of the reason
            reason: types::ConnectReason::ProactiveNetworkSwitch,
        };
        client.connect(duplicate_request, connect_sender2).expect("failed to make request");

        // Progress the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Check the responder was acknowledged
        assert_variant!(exec.run_until_stalled(&mut connect_receiver2), Poll::Ready(Ok(())));

        // Ensure a connect request is sent to the SME
        let connect_txn_handle = assert_variant!(
            poll_sme_req(&mut exec, &mut sme_fut),
            Poll::Ready(fidl_sme::ClientSmeRequest::Connect{ req, txn, control_handle: _ }) => {
                assert_eq!(req.ssid, next_network_ssid.to_vec());
                assert_eq!(connect_request.target.credential, req.authentication.credentials);
                assert_eq!(req.deprecated_scan_type, fidl_fuchsia_wlan_common::ScanType::Active);
                assert_eq!(req.bss_description, bss_description);
                assert_eq!(req.multiple_bss_candidates, true);
                 // Send connection response.
                let (_stream, ctrl) = txn.expect("connect txn unused")
                    .into_stream_and_control_handle().expect("error accessing control handle");
                ctrl
            }
        );
        connect_txn_handle
            .send_on_connect_result(&mut fake_successful_connect_result())
            .expect("failed to send connection completion");

        // Progress the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Check for a connect update
        let client_state_update = ClientStateUpdate {
            state: fidl_policy::WlanClientState::ConnectionsEnabled,
            networks: vec![ClientNetworkState {
                id: types::NetworkIdentifier {
                    ssid: next_network_ssid.clone(),
                    security_type: types::SecurityType::Wpa2,
                },
                state: fidl_policy::ConnectionState::Connected,
                status: None,
            }],
        };
        assert_variant!(
            test_values.update_receiver.try_next(),
            Ok(Some(listener::Message::NotifyListeners(updates))) => {
            assert_eq!(updates, client_state_update);
        });

        // Progress the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Ensure no further updates were sent to listeners
        assert_variant!(
            exec.run_until_stalled(&mut test_values.update_receiver.into_future()),
            Poll::Pending
        );
    }

    #[fuchsia::test]
    fn connecting_state_gets_different_connect_request() {
        let mut exec = fasync::TestExecutor::new().expect("failed to create an executor");
        let mut test_values = test_setup();

        let first_network_ssid = types::Ssid::try_from("foo").unwrap();
        let second_network_ssid = types::Ssid::try_from("bar").unwrap();
        let bss_description = random_fidl_bss_description!(Wpa2, ssid: first_network_ssid.clone());
        let connect_request = types::ConnectRequest {
            target: types::ConnectionCandidate {
                network: types::NetworkIdentifier {
                    ssid: first_network_ssid.clone(),
                    security_type: types::SecurityType::Wpa2,
                },
                credential: Credential::Password(b"password".to_vec()),
                scanned: Some(types::ScannedCandidate {
                    bss_description: bss_description.clone(),
                    observation: types::ScanObservation::Passive,
                    has_multiple_bss_candidates: true,
                    security_type_detailed: types::SecurityTypeDetailed::Wpa2Personal,
                }),
            },
            reason: types::ConnectReason::RegulatoryChangeReconnect,
        };
        let (connect_sender, mut connect_receiver) = oneshot::channel();
        let connecting_options = ConnectingOptions {
            connect_responder: Some(connect_sender),
            connect_request: connect_request.clone(),
            attempt_counter: 0,
        };
        let initial_state = connecting_state(test_values.common_options, connecting_options);
        let fut = run_state_machine(initial_state);
        pin_mut!(fut);
        let sme_fut = test_values.sme_req_stream.into_future();
        pin_mut!(sme_fut);

        // Run the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Check the responder was acknowledged
        assert_variant!(exec.run_until_stalled(&mut connect_receiver), Poll::Ready(Ok(())));

        // Check for a connecting update
        let client_state_update = ClientStateUpdate {
            state: fidl_policy::WlanClientState::ConnectionsEnabled,
            networks: vec![ClientNetworkState {
                id: types::NetworkIdentifier {
                    ssid: first_network_ssid.clone(),
                    security_type: types::SecurityType::Wpa2,
                },
                state: fidl_policy::ConnectionState::Connecting,
                status: None,
            }],
        };
        assert_variant!(
            test_values.update_receiver.try_next(),
            Ok(Some(listener::Message::NotifyListeners(updates))) => {
            assert_eq!(updates, client_state_update);
        });

        // Send a different connect request
        let mut client = Client::new(test_values.client_req_sender);
        let (connect_sender2, mut connect_receiver2) = oneshot::channel();
        let bss_desc2 = random_fidl_bss_description!(Wpa2, ssid: second_network_ssid.clone());
        let connect_request2 = types::ConnectRequest {
            target: types::ConnectionCandidate {
                network: types::NetworkIdentifier {
                    ssid: second_network_ssid.clone(),
                    security_type: types::SecurityType::Wpa2,
                },
                credential: Credential::Password(b"password".to_vec()),
                scanned: Some(types::ScannedCandidate {
                    bss_description: bss_desc2.clone(),
                    observation: types::ScanObservation::Passive,
                    has_multiple_bss_candidates: false,
                    security_type_detailed: types::SecurityTypeDetailed::Wpa2Personal,
                }),
            },
            reason: types::ConnectReason::FidlConnectRequest,
        };
        client.connect(connect_request2.clone(), connect_sender2).expect("failed to make request");

        // Progress the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Check for a disconnect update
        let client_state_update = ClientStateUpdate {
            state: fidl_policy::WlanClientState::ConnectionsEnabled,
            networks: vec![ClientNetworkState {
                id: types::NetworkIdentifier {
                    ssid: first_network_ssid.clone(),
                    security_type: types::SecurityType::Wpa2,
                },
                state: fidl_policy::ConnectionState::Disconnected,
                status: Some(fidl_policy::DisconnectStatus::ConnectionStopped),
            }],
        };
        assert_variant!(
            test_values.update_receiver.try_next(),
            Ok(Some(listener::Message::NotifyListeners(updates))) => {
            assert_eq!(updates, client_state_update);
        });

        // There should be 3 requests to the SME stacked up
        // First SME request: connect to the first network
        assert_variant!(
            poll_sme_req(&mut exec, &mut sme_fut),
            Poll::Ready(fidl_sme::ClientSmeRequest::Connect{ req, txn: _, control_handle: _ }) => {
                assert_eq!(req.ssid, first_network_ssid.to_vec());
                assert_eq!(req.bss_description, bss_description.clone());
                // Don't bother sending response, listener is gone
            }
        );
        // Second SME request: disconnect
        assert_variant!(
            poll_sme_req(&mut exec, &mut sme_fut),
            Poll::Ready(fidl_sme::ClientSmeRequest::Disconnect{ responder, reason: fidl_sme::UserDisconnectReason::FidlConnectRequest }) => {
                responder.send().expect("could not send sme response");
            }
        );
        // Progress the state machine
        // TODO(fxbug.dev/53505): remove this once the disconnect request is fire-and-forget
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // The state machine should have sent a listener update
        assert_variant!(
            test_values.update_receiver.try_next(),
            Ok(Some(listener::Message::NotifyListeners(ClientStateUpdate {
                state: fidl_policy::WlanClientState::ConnectionsEnabled,
                networks
            }))) => {
                assert!(networks.is_empty());
            }
        );

        // Third SME request: connect to the second network
        let connect_txn_handle = assert_variant!(
            poll_sme_req(&mut exec, &mut sme_fut),
            Poll::Ready(fidl_sme::ClientSmeRequest::Connect{ req, txn, control_handle: _ }) => {
                assert_eq!(req.ssid, second_network_ssid.to_vec());
                assert_eq!(req.bss_description, bss_desc2.clone());
                assert_eq!(req.multiple_bss_candidates, false);
                 // Send connection response.
                let (_stream, ctrl) = txn.expect("connect txn unused")
                    .into_stream_and_control_handle().expect("error accessing control handle");
                ctrl
            }
        );
        connect_txn_handle
            .send_on_connect_result(&mut fake_successful_connect_result())
            .expect("failed to send connection completion");

        // Progress the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Check the responder was acknowledged
        assert_variant!(exec.run_until_stalled(&mut connect_receiver2), Poll::Ready(Ok(())));

        // Check for a connecting update
        let client_state_update = ClientStateUpdate {
            state: fidl_policy::WlanClientState::ConnectionsEnabled,
            networks: vec![ClientNetworkState {
                id: types::NetworkIdentifier {
                    ssid: second_network_ssid.clone(),
                    security_type: types::SecurityType::Wpa2,
                },
                state: fidl_policy::ConnectionState::Connecting,
                status: None,
            }],
        };
        assert_variant!(
            test_values.update_receiver.try_next(),
            Ok(Some(listener::Message::NotifyListeners(updates))) => {
            assert_eq!(updates, client_state_update);
        });
        // Check for a connected update
        let client_state_update = ClientStateUpdate {
            state: fidl_policy::WlanClientState::ConnectionsEnabled,
            networks: vec![ClientNetworkState {
                id: types::NetworkIdentifier {
                    ssid: second_network_ssid.clone(),
                    security_type: types::SecurityType::Wpa2,
                },
                state: fidl_policy::ConnectionState::Connected,
                status: None,
            }],
        };
        assert_variant!(
            test_values.update_receiver.try_next(),
            Ok(Some(listener::Message::NotifyListeners(updates))) => {
            assert_eq!(updates, client_state_update);
        });

        // Progress the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Ensure no further updates were sent to listeners
        assert_variant!(
            exec.run_until_stalled(&mut test_values.update_receiver.into_future()),
            Poll::Pending
        );
    }

    #[fuchsia::test]
    fn connecting_state_gets_disconnect_request() {
        let mut exec = fasync::TestExecutor::new().expect("failed to create an executor");
        let mut test_values = test_setup();

        let first_network_ssid = types::Ssid::try_from("foo").unwrap();
        let bss_description = random_fidl_bss_description!(Wpa2, ssid: first_network_ssid.clone());
        let connect_request = types::ConnectRequest {
            target: types::ConnectionCandidate {
                network: types::NetworkIdentifier {
                    ssid: first_network_ssid.clone(),
                    security_type: types::SecurityType::Wpa2,
                },
                credential: Credential::Password(b"password".to_vec()),
                scanned: Some(types::ScannedCandidate {
                    bss_description: bss_description.clone(),
                    observation: types::ScanObservation::Passive,
                    has_multiple_bss_candidates: true,
                    security_type_detailed: types::SecurityTypeDetailed::Wpa2Personal,
                }),
            },
            reason: types::ConnectReason::RegulatoryChangeReconnect,
        };
        let (connect_sender, mut connect_receiver) = oneshot::channel();
        let connecting_options = ConnectingOptions {
            connect_responder: Some(connect_sender),
            connect_request: connect_request.clone(),
            attempt_counter: 0,
        };
        let initial_state = connecting_state(test_values.common_options, connecting_options);
        let fut = run_state_machine(initial_state);
        pin_mut!(fut);
        let sme_fut = test_values.sme_req_stream.into_future();
        pin_mut!(sme_fut);

        // Run the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Check the responder was acknowledged
        assert_variant!(exec.run_until_stalled(&mut connect_receiver), Poll::Ready(Ok(())));

        // Check for a connecting update
        let client_state_update = ClientStateUpdate {
            state: fidl_policy::WlanClientState::ConnectionsEnabled,
            networks: vec![ClientNetworkState {
                id: types::NetworkIdentifier {
                    ssid: first_network_ssid.clone(),
                    security_type: types::SecurityType::Wpa2,
                },
                state: fidl_policy::ConnectionState::Connecting,
                status: None,
            }],
        };
        assert_variant!(
            test_values.update_receiver.try_next(),
            Ok(Some(listener::Message::NotifyListeners(updates))) => {
            assert_eq!(updates, client_state_update);
        });

        // Send a disconnect request
        let mut client = Client::new(test_values.client_req_sender);
        let (disconnect_sender, mut disconnect_receiver) = oneshot::channel();
        client
            .disconnect(types::DisconnectReason::NetworkUnsaved, disconnect_sender)
            .expect("failed to make request");

        // Progress the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Check for a disconnect update
        let client_state_update = ClientStateUpdate {
            state: fidl_policy::WlanClientState::ConnectionsEnabled,
            networks: vec![ClientNetworkState {
                id: types::NetworkIdentifier {
                    ssid: first_network_ssid.clone(),
                    security_type: types::SecurityType::Wpa2,
                },
                state: fidl_policy::ConnectionState::Disconnected,
                status: Some(fidl_policy::DisconnectStatus::ConnectionStopped),
            }],
        };
        assert_variant!(
            test_values.update_receiver.try_next(),
            Ok(Some(listener::Message::NotifyListeners(updates))) => {
            assert_eq!(updates, client_state_update);
        });

        // There should be 2 requests to the SME stacked up
        // First SME request: connect to the first network
        assert_variant!(
            poll_sme_req(&mut exec, &mut sme_fut),
            Poll::Ready(fidl_sme::ClientSmeRequest::Connect{ req, txn: _, control_handle: _ }) => {
                assert_eq!(req.ssid, first_network_ssid.to_vec());
                assert_eq!(req.bss_description, bss_description.clone());
                // Don't bother sending response, listener is gone
            }
        );
        assert_variant!(
            poll_sme_req(&mut exec, &mut sme_fut),
            Poll::Ready(fidl_sme::ClientSmeRequest::Disconnect{ responder, reason: fidl_sme::UserDisconnectReason::NetworkUnsaved }) => {
                responder.send().expect("could not send sme response");
            }
        );
        // The state machine should exit after processing the disconnect.
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Ready(()));

        // Check the disconnect responder
        assert_variant!(exec.run_until_stalled(&mut disconnect_receiver), Poll::Ready(Ok(())));
    }

    #[fuchsia::test]
    fn connecting_state_has_broken_sme() {
        let mut exec = fasync::TestExecutor::new().expect("failed to create an executor");
        let test_values = test_setup();

        let first_network_ssid = types::Ssid::try_from("foo").unwrap();
        let connect_request = types::ConnectRequest {
            target: types::ConnectionCandidate {
                network: types::NetworkIdentifier {
                    ssid: first_network_ssid.clone(),
                    security_type: types::SecurityType::Wpa2,
                },
                credential: Credential::None,
                scanned: None,
            },
            reason: types::ConnectReason::RegulatoryChangeReconnect,
        };
        let (connect_sender, mut connect_receiver) = oneshot::channel();
        let connecting_options = ConnectingOptions {
            connect_responder: Some(connect_sender),
            connect_request: connect_request.clone(),
            attempt_counter: 0,
        };
        let initial_state = connecting_state(test_values.common_options, connecting_options);
        let fut = run_state_machine(initial_state);
        pin_mut!(fut);

        // Break the SME by dropping the server end of the SME stream, so it causes an error
        drop(test_values.sme_req_stream);

        // Ensure the state machine exits
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);
        assert!(exec.wake_next_timer().is_some());
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Ready(()));

        // Expect the responder to have a success, since the connection was attempted
        assert_variant!(exec.run_until_stalled(&mut connect_receiver), Poll::Ready(Ok(())));
    }

    #[fuchsia::test]
    fn connected_state_gets_disconnect_request() {
        let mut exec =
            fasync::TestExecutor::new_with_fake_time().expect("failed to create an executor");
        exec.set_fake_time(fasync::Time::from_nanos(0));

        let mut test_values = test_setup();
        let mut telemetry_receiver = test_values.telemetry_receiver;

        let network_ssid = types::Ssid::try_from("test").unwrap();
        let id = types::NetworkIdentifier {
            ssid: network_ssid.clone(),
            security_type: types::SecurityType::Wpa2,
        };
        let credential = Credential::Password(b"password".to_vec());
        let bss_description = random_bss_description!(Wpa2, ssid: network_ssid.clone());
        let connect_request = types::ConnectRequest {
            target: types::ConnectionCandidate {
                network: id.clone(),
                credential: credential.clone(),
                scanned: Some(types::ScannedCandidate {
                    bss_description: bss_description.clone().into(),
                    observation: types::ScanObservation::Passive,
                    has_multiple_bss_candidates: true,
                    security_type_detailed: types::SecurityTypeDetailed::Wpa2Personal,
                }),
            },
            reason: types::ConnectReason::RegulatoryChangeReconnect,
        };
        let (connect_txn_proxy, _connect_txn_stream) =
            create_proxy_and_stream::<fidl_sme::ConnectTransactionMarker>()
                .expect("failed to create a connect txn channel");
        let connection_attempt_time = fasync::Time::now();
        let time_to_connect = zx::Duration::from_seconds(10);
        let options = ConnectedOptions {
            currently_fulfilled_request: connect_request,
            multiple_bss_candidates: true,
            latest_ap_state: Box::new(bss_description.clone()),
            connect_txn_stream: connect_txn_proxy.take_event_stream(),
            connection_attempt_time,
            time_to_connect,
        };
        let initial_state = connected_state(test_values.common_options, options);
        let fut = run_state_machine(initial_state);
        pin_mut!(fut);
        let sme_fut = test_values.sme_req_stream.into_future();
        pin_mut!(sme_fut);

        // Run the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        let disconnect_time = fasync::Time::after(12.hours());
        exec.set_fake_time(disconnect_time);

        // Send a disconnect request
        let mut client = Client::new(test_values.client_req_sender);
        let (sender, mut receiver) = oneshot::channel();
        client
            .disconnect(types::DisconnectReason::FidlStopClientConnectionsRequest, sender)
            .expect("failed to make request");

        // Run the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Respond to the SME disconnect
        assert_variant!(
            poll_sme_req(&mut exec, &mut sme_fut),
            Poll::Ready(fidl_sme::ClientSmeRequest::Disconnect{ responder, reason: fidl_sme::UserDisconnectReason::FidlStopClientConnectionsRequest }) => {
                responder.send().expect("could not send sme response");
            }
        );

        // Once the disconnect is processed, the state machine should exit.
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Ready(()));

        // Check for a disconnect update and the responder
        let client_state_update = ClientStateUpdate {
            state: fidl_policy::WlanClientState::ConnectionsEnabled,
            networks: vec![ClientNetworkState {
                id: id.clone(),
                state: fidl_policy::ConnectionState::Disconnected,
                status: Some(fidl_policy::DisconnectStatus::ConnectionStopped),
            }],
        };
        assert_variant!(
            test_values.update_receiver.try_next(),
            Ok(Some(listener::Message::NotifyListeners(updates))) => {
            assert_eq!(updates, client_state_update);
        });
        assert_variant!(exec.run_until_stalled(&mut receiver), Poll::Ready(Ok(())));

        // Disconnect telemetry event sent
        assert_variant!(telemetry_receiver.try_next(), Ok(Some(event)) => {
            assert_variant!(event, TelemetryEvent::Disconnected { track_subsequent_downtime, info } => {
                assert!(!track_subsequent_downtime);
                assert_eq!(info, DisconnectInfo {
                    connected_duration: 12.hours(),
                    is_sme_reconnecting: false,
                    disconnect_source: fidl_sme::DisconnectSource::User(fidl_sme::UserDisconnectReason::FidlStopClientConnectionsRequest),
                    latest_ap_state: bss_description.clone(),
                });
            });
        });

        // The disconnect should have been recorded for the saved network config.
        let expected_recorded_connection = ConnectionRecord {
            id: id.clone(),
            credential: credential.clone(),
            data: PastConnectionData {
                bssid: bss_description.bssid,
                connection_attempt_time,
                time_to_connect,
                disconnect_time,
                connection_uptime: zx::Duration::from_hours(12),
                disconnect_reason: types::DisconnectReason::FidlStopClientConnectionsRequest,
                signal_data_at_disconnect: bss_selection::SignalData::new(
                    bss_description.rssi_dbm,
                    bss_description.snr_db,
                    bss_selection::EWMA_SMOOTHING_FACTOR,
                ),
                // TODO: record average phy rate over connection once available
                average_tx_rate: 0,
            },
        };
        assert_variant!(test_values.saved_networks_manager.get_recorded_past_connections().as_slice(), [connection_data] => {
            assert_eq!(connection_data, &expected_recorded_connection);
        });
    }

    #[fuchsia::test]
    fn connected_state_records_unexpected_disconnect() {
        let mut exec =
            fasync::TestExecutor::new_with_fake_time().expect("failed to create an executor");
        exec.set_fake_time(fasync::Time::from_nanos(0));

        let test_values = test_setup();
        let mut telemetry_receiver = test_values.telemetry_receiver;

        let network_ssid = types::Ssid::try_from("flaky-network").unwrap();
        let security = types::SecurityType::Wpa2;
        let credential = Credential::Password(b"password".to_vec());
        // Save the network in order to later record the disconnect to it.
        let save_fut = test_values.saved_networks_manager.store(
            network_config::NetworkIdentifier {
                ssid: network_ssid.clone(),
                security_type: security.into(),
            },
            credential.clone(),
        );
        pin_mut!(save_fut);
        assert_variant!(exec.run_until_stalled(&mut save_fut), Poll::Ready(Ok(None)));

        // Build the values for the connected state.
        let bss_description = random_bss_description!(Wpa2, ssid: network_ssid.clone());
        let bssid = bss_description.bssid;
        let id = types::NetworkIdentifier { ssid: network_ssid, security_type: security };
        let connect_request = types::ConnectRequest {
            target: types::ConnectionCandidate {
                network: id.clone(),
                credential: credential.clone(),
                scanned: Some(types::ScannedCandidate {
                    bss_description: bss_description.clone().into(),
                    observation: types::ScanObservation::Passive,
                    has_multiple_bss_candidates: true,
                    security_type_detailed: types::SecurityTypeDetailed::Wpa2Personal,
                }),
            },
            reason: types::ConnectReason::RetryAfterFailedConnectAttempt,
        };
        let (connect_txn_proxy, connect_txn_stream) =
            create_proxy_and_stream::<fidl_sme::ConnectTransactionMarker>()
                .expect("failed to create a connect txn channel");
        let connect_txn_handle = connect_txn_stream.control_handle();
        let connection_attempt_time = fasync::Time::now();
        let time_to_connect = zx::Duration::from_seconds(10);
        let options = ConnectedOptions {
            currently_fulfilled_request: connect_request.clone(),
            multiple_bss_candidates: true,
            latest_ap_state: Box::new(bss_description.clone()),
            connect_txn_stream: connect_txn_proxy.take_event_stream(),
            connection_attempt_time,
            time_to_connect,
        };

        // Start the state machine in the connected state.
        let initial_state = connected_state(test_values.common_options, options);
        let fut = run_state_machine(initial_state);
        pin_mut!(fut);
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        let disconnect_time = fasync::Time::after(12.hours());
        exec.set_fake_time(disconnect_time);

        // SME notifies Policy of disconnection
        let is_sme_reconnecting = false;
        let mut fidl_disconnect_info = generate_disconnect_info(is_sme_reconnecting);
        connect_txn_handle
            .send_on_disconnect(&mut fidl_disconnect_info)
            .expect("failed to send disconnection event");
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // The disconnect should have been recorded for the saved network config.
        let expected_recorded_connection = ConnectionRecord {
            id: id.clone(),
            credential: credential.clone(),
            data: PastConnectionData {
                bssid,
                connection_attempt_time,
                time_to_connect,
                disconnect_time,
                connection_uptime: zx::Duration::from_hours(12),
                disconnect_reason: types::DisconnectReason::DisconnectDetectedFromSme,
                signal_data_at_disconnect: bss_selection::SignalData::new(
                    bss_description.rssi_dbm,
                    bss_description.snr_db,
                    bss_selection::EWMA_SMOOTHING_FACTOR,
                ),
                // TODO: record average phy rate over connection once available
                average_tx_rate: 0,
            },
        };
        assert_variant!(test_values.saved_networks_manager.get_recorded_past_connections().as_slice(), [connection_data] => {
            assert_eq!(connection_data, &expected_recorded_connection);
        });

        assert_variant!(telemetry_receiver.try_next(), Ok(Some(event)) => {
            // Disconnect telemetry event sent
            assert_variant!(event, TelemetryEvent::Disconnected { track_subsequent_downtime, info } => {
                assert!(track_subsequent_downtime);
                assert_eq!(info, DisconnectInfo {
                    connected_duration: 12.hours(),
                    is_sme_reconnecting,
                    disconnect_source: fidl_disconnect_info.disconnect_source,
                    latest_ap_state: bss_description,
                });
            });
        });
        assert_variant!(telemetry_receiver.try_next(), Ok(Some(event)) => {
            // StartEstablishConnection event sent (because the state machine will attempt
            // to reconnect)
            assert_variant!(event, TelemetryEvent::StartEstablishConnection { reset_start_time: false });
        });
    }

    #[fuchsia::test]
    fn connected_state_reconnect_resets_connected_duration() {
        let mut exec =
            fasync::TestExecutor::new_with_fake_time().expect("failed to create an executor");
        exec.set_fake_time(fasync::Time::from_nanos(0));

        let test_values = test_setup();
        let mut telemetry_receiver = test_values.telemetry_receiver;

        let network_ssid = types::Ssid::try_from("test").unwrap();
        let bss_description = random_bss_description!(Wpa2, ssid: network_ssid.clone());
        let connect_request = types::ConnectRequest {
            target: types::ConnectionCandidate {
                network: types::NetworkIdentifier {
                    ssid: network_ssid,
                    security_type: types::SecurityType::Wpa2,
                },
                credential: Credential::None,
                scanned: Some(types::ScannedCandidate {
                    bss_description: bss_description.clone().into(),
                    observation: types::ScanObservation::Passive,
                    has_multiple_bss_candidates: true,
                    security_type_detailed: types::SecurityTypeDetailed::Wpa2Personal,
                }),
            },
            reason: types::ConnectReason::RegulatoryChangeReconnect,
        };
        let (connect_txn_proxy, connect_txn_stream) =
            create_proxy_and_stream::<fidl_sme::ConnectTransactionMarker>()
                .expect("failed to create a connect txn channel");
        let connect_txn_handle = connect_txn_stream.control_handle();
        let options = ConnectedOptions {
            currently_fulfilled_request: connect_request,
            latest_ap_state: Box::new(bss_description),
            multiple_bss_candidates: true,
            connect_txn_stream: connect_txn_proxy.take_event_stream(),
            connection_attempt_time: fasync::Time::now(),
            time_to_connect: zx::Duration::from_seconds(10),
        };
        let initial_state = connected_state(test_values.common_options, options);
        let fut = run_state_machine(initial_state);
        pin_mut!(fut);

        // Run the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        exec.set_fake_time(fasync::Time::after(12.hours()));

        // SME notifies Policy of disconnection with SME-initiated reconnect
        let is_sme_reconnecting = true;
        let mut fidl_disconnect_info = generate_disconnect_info(is_sme_reconnecting);
        connect_txn_handle
            .send_on_disconnect(&mut fidl_disconnect_info)
            .expect("failed to send disconnection event");
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Disconnect telemetry event sent
        assert_variant!(telemetry_receiver.try_next(), Ok(Some(event)) => {
            assert_variant!(event, TelemetryEvent::Disconnected { info, .. } => {
                assert_eq!(info.connected_duration, 12.hours());
            });
        });

        // SME notifies Policy of reconnection successful
        exec.set_fake_time(fasync::Time::after(1.second()));
        let mut connect_result =
            fidl_sme::ConnectResult { is_reconnect: true, ..fake_successful_connect_result() };
        connect_txn_handle
            .send_on_connect_result(&mut connect_result)
            .expect("failed to send connect result event");
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        assert_variant!(
            telemetry_receiver.try_next(),
            Ok(Some(TelemetryEvent::ConnectResult { .. }))
        );

        // SME notifies Policy of another disconnection
        exec.set_fake_time(fasync::Time::after(2.hours()));
        let is_sme_reconnecting = false;
        let mut fidl_disconnect_info = generate_disconnect_info(is_sme_reconnecting);
        connect_txn_handle
            .send_on_disconnect(&mut fidl_disconnect_info)
            .expect("failed to send disconnection event");
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Another disconnect telemetry event sent
        assert_variant!(telemetry_receiver.try_next(), Ok(Some(event)) => {
            assert_variant!(event, TelemetryEvent::Disconnected { info, .. } => {
                assert_eq!(info.connected_duration, 2.hours());
            });
        });
    }

    #[fuchsia::test]
    fn connected_state_records_unexpected_disconnect_unspecified_bss() {
        let mut exec =
            fasync::TestExecutor::new_with_fake_time().expect("failed to create an executor");
        let connection_attempt_time = fasync::Time::from_nanos(0);
        exec.set_fake_time(connection_attempt_time);
        let test_values = test_setup();

        let network_ssid = types::Ssid::try_from("flaky-network").unwrap();
        let security = types::SecurityType::Wpa2;
        let credential = Credential::Password(b"password".to_vec());
        let id = network_config::NetworkIdentifier {
            ssid: network_ssid.clone(),
            security_type: security.into(),
        };
        // Setup for network selection in the connecting state to select the intended network.
        let expected_config =
            network_config::NetworkConfig::new(id.clone().into(), credential.clone(), false)
                .expect("failed to create network config");
        test_values.saved_networks_manager.set_lookup_compatible_response(vec![expected_config]);

        let connect_request = types::ConnectRequest {
            target: types::ConnectionCandidate {
                network: id.clone(),
                credential: credential.clone(),
                scanned: None,
            },
            reason: types::ConnectReason::RetryAfterFailedConnectAttempt,
        };
        let (connect_sender, _connect_receiver) = oneshot::channel();
        let connecting_options = ConnectingOptions {
            connect_responder: Some(connect_sender),
            connect_request: connect_request.clone(),
            attempt_counter: 0,
        };
        let initial_state = connecting_state(test_values.common_options, connecting_options);
        let state_fut = run_state_machine(initial_state);
        pin_mut!(state_fut);
        let sme_fut = test_values.sme_req_stream.into_future();
        pin_mut!(sme_fut);

        // Run the state machine
        assert_variant!(exec.run_until_stalled(&mut state_fut), Poll::Pending);

        // Send a scan for the requested network
        let bss_description = random_fidl_bss_description!(Wpa2, ssid: network_ssid.clone());
        let mut scan_results = vec![fidl_sme::ScanResult {
            compatible: true,
            timestamp_nanos: zx::Time::get_monotonic().into_nanos(),
            bss_description: bss_description.clone(),
        }];
        assert_variant!(
            poll_sme_req(&mut exec, &mut sme_fut),
            Poll::Ready(fidl_sme::ClientSmeRequest::Scan {
                txn, req: _, control_handle: _
            }) => {
                // Send the scan results up
                let (_stream, ctrl) = txn
                    .into_stream_and_control_handle().expect("error accessing control handle");
                ctrl.send_on_result(&mut scan_results.iter_mut())
                    .expect("failed to send scan data");

                // Send the end of data
                ctrl.send_on_finished()
                    .expect("failed to send scan data");
            }
        );
        assert_variant!(exec.run_until_stalled(&mut state_fut), Poll::Pending);

        let time_to_connect = 10.seconds();
        exec.set_fake_time(fasync::Time::after(time_to_connect));

        // Process connect request sent to SME
        let connect_txn_handle = assert_variant!(
            poll_sme_req(&mut exec, &mut sme_fut),
            Poll::Ready(fidl_sme::ClientSmeRequest::Connect{ req: _, txn, control_handle: _ }) => {
                 // Send connection response.
                let (_stream, ctrl) = txn.expect("connect txn unused")
                    .into_stream_and_control_handle().expect("error accessing control handle");
                ctrl
            }
        );
        connect_txn_handle
            .send_on_connect_result(&mut fake_successful_connect_result())
            .expect("failed to send connection completion");
        assert_variant!(exec.run_until_stalled(&mut state_fut), Poll::Pending);

        // SME notifies Policy of disconnection.
        let disconnect_time = fasync::Time::after(5.hours());
        exec.set_fake_time(disconnect_time);
        let is_sme_reconnecting = false;
        connect_txn_handle
            .send_on_disconnect(&mut generate_disconnect_info(is_sme_reconnecting))
            .expect("failed to send disconnection event");
        assert_variant!(exec.run_until_stalled(&mut state_fut), Poll::Pending);

        // The connection data should have been recorded at disconnect.
        let expected_recorded_connection = ConnectionRecord {
            id: id.clone().into(),
            credential: credential.clone(),
            data: PastConnectionData {
                bssid: types::Bssid(bss_description.bssid),
                connection_attempt_time,
                time_to_connect,
                disconnect_time,
                connection_uptime: zx::Duration::from_hours(5),
                disconnect_reason: types::DisconnectReason::DisconnectDetectedFromSme,
                signal_data_at_disconnect: bss_selection::SignalData::new(
                    bss_description.rssi_dbm,
                    bss_description.snr_db,
                    bss_selection::EWMA_SMOOTHING_FACTOR,
                ),
                average_tx_rate: 0,
            },
        };
        assert_variant!(test_values.saved_networks_manager.get_recorded_past_connections().as_slice(), [connection_data] => {
            assert_eq!(connection_data, &expected_recorded_connection);
        });
    }

    #[fuchsia::test]
    fn connected_state_gets_duplicate_connect_request() {
        let mut exec =
            fasync::TestExecutor::new_with_fake_time().expect("failed to create an executor");
        exec.set_fake_time(fasync::Time::from_nanos(0));
        let test_values = test_setup();
        let mut telemetry_receiver = test_values.telemetry_receiver;

        let network_ssid = types::Ssid::try_from("test").unwrap();
        let bss_description = random_bss_description!(Wpa2, ssid: network_ssid.clone());
        let connect_request = types::ConnectRequest {
            target: types::ConnectionCandidate {
                network: types::NetworkIdentifier {
                    ssid: network_ssid.clone(),
                    security_type: types::SecurityType::Wpa2,
                },
                credential: Credential::None,
                scanned: Some(types::ScannedCandidate {
                    bss_description: bss_description.clone().into(),
                    observation: types::ScanObservation::Passive,
                    has_multiple_bss_candidates: true,
                    security_type_detailed: types::SecurityTypeDetailed::Wpa2Personal,
                }),
            },
            reason: types::ConnectReason::RegulatoryChangeReconnect,
        };
        let (connect_txn_proxy, _connect_txn_stream) =
            create_proxy_and_stream::<fidl_sme::ConnectTransactionMarker>()
                .expect("failed to create a connect txn channel");
        let options = ConnectedOptions {
            currently_fulfilled_request: connect_request.clone(),
            latest_ap_state: Box::new(bss_description),
            multiple_bss_candidates: true,
            connect_txn_stream: connect_txn_proxy.take_event_stream(),
            connection_attempt_time: fasync::Time::now(),
            time_to_connect: zx::Duration::from_seconds(10),
        };
        let initial_state = connected_state(test_values.common_options, options);
        let fut = run_state_machine(initial_state);
        pin_mut!(fut);
        let sme_fut = test_values.sme_req_stream.into_future();
        pin_mut!(sme_fut);

        // Send another duplicate request
        let mut client = Client::new(test_values.client_req_sender);
        let (sender, mut receiver) = oneshot::channel();
        client.connect(connect_request.clone(), sender).expect("failed to make request");

        // Run the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Ensure nothing was sent to the SME
        assert_variant!(poll_sme_req(&mut exec, &mut sme_fut), Poll::Pending);

        // Check the responder was acknowledged
        assert_variant!(exec.run_until_stalled(&mut receiver), Poll::Ready(Ok(())));

        // No telemetry event is sent
        assert_variant!(telemetry_receiver.try_next(), Err(_));
    }

    #[fuchsia::test]
    fn connected_state_gets_different_connect_request() {
        let mut exec =
            fasync::TestExecutor::new_with_fake_time().expect("failed to create an executor");
        exec.set_fake_time(fasync::Time::from_nanos(0));

        let mut test_values = test_setup();
        let mut telemetry_receiver = test_values.telemetry_receiver;

        let first_network_ssid = types::Ssid::try_from("foo").unwrap();
        let second_network_ssid = types::Ssid::try_from("bar").unwrap();
        let bss_description = random_bss_description!(Wpa2, ssid: first_network_ssid.clone());
        let id_1 = types::NetworkIdentifier {
            ssid: first_network_ssid.clone(),
            security_type: types::SecurityType::Wpa2,
        };
        let credential_1 = Credential::Password(b"some-password".to_vec());
        let connect_request = types::ConnectRequest {
            target: types::ConnectionCandidate {
                network: id_1.clone(),
                credential: credential_1.clone(),
                scanned: Some(types::ScannedCandidate {
                    bss_description: bss_description.clone().into(),
                    observation: types::ScanObservation::Passive,
                    has_multiple_bss_candidates: true,
                    security_type_detailed: types::SecurityTypeDetailed::Wpa2Personal,
                }),
            },
            reason: types::ConnectReason::IdleInterfaceAutoconnect,
        };
        let (connect_txn_proxy, _connect_txn_stream) =
            create_proxy_and_stream::<fidl_sme::ConnectTransactionMarker>()
                .expect("failed to create a connect txn channel");
        let connection_attempt_time = fasync::Time::now();
        let time_to_connect = zx::Duration::from_seconds(10);
        let options = ConnectedOptions {
            currently_fulfilled_request: connect_request.clone(),
            latest_ap_state: Box::new(bss_description.clone()),
            multiple_bss_candidates: true,
            connect_txn_stream: connect_txn_proxy.take_event_stream(),
            connection_attempt_time,
            time_to_connect,
        };
        let initial_state = connected_state(test_values.common_options, options);
        let fut = run_state_machine(initial_state);
        pin_mut!(fut);
        let sme_fut = test_values.sme_req_stream.into_future();
        pin_mut!(sme_fut);

        // Run the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        let disconnect_time = fasync::Time::after(12.hours());
        exec.set_fake_time(disconnect_time);

        // Send a different connect request
        let second_bss_desc = random_fidl_bss_description!(Wpa2, ssid: second_network_ssid.clone());
        let mut client = Client::new(test_values.client_req_sender);
        let (connect_sender2, mut connect_receiver2) = oneshot::channel();
        let connect_request2 = types::ConnectRequest {
            target: types::ConnectionCandidate {
                network: types::NetworkIdentifier {
                    ssid: second_network_ssid.clone(),
                    security_type: types::SecurityType::Wpa2,
                },
                credential: Credential::Password(b"password".to_vec()),
                scanned: Some(types::ScannedCandidate {
                    bss_description: second_bss_desc.clone().into(),
                    observation: types::ScanObservation::Passive,
                    has_multiple_bss_candidates: true,
                    security_type_detailed: types::SecurityTypeDetailed::Wpa2Personal,
                }),
            },
            reason: types::ConnectReason::ProactiveNetworkSwitch,
        };
        client.connect(connect_request2.clone(), connect_sender2).expect("failed to make request");

        // Run the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // There should be 2 requests to the SME stacked up
        // First SME request: disconnect
        assert_variant!(
            poll_sme_req(&mut exec, &mut sme_fut),
            Poll::Ready(fidl_sme::ClientSmeRequest::Disconnect{ responder, reason: fidl_sme::UserDisconnectReason::ProactiveNetworkSwitch }) => {
                responder.send().expect("could not send sme response");
            }
        );
        // Progress the state machine
        // TODO(fxbug.dev/53505): remove this once the disconnect request is fire-and-forget
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);
        // Second SME request: connect to the second network
        let connect_txn_handle = assert_variant!(
            poll_sme_req(&mut exec, &mut sme_fut),
            Poll::Ready(fidl_sme::ClientSmeRequest::Connect{ req, txn, control_handle: _ }) => {
                assert_eq!(req.ssid, second_network_ssid.to_vec());
                assert_eq!(req.bss_description, second_bss_desc.clone());
                 // Send connection response.
                let (_stream, ctrl) = txn.expect("connect txn unused")
                    .into_stream_and_control_handle().expect("error accessing control handle");
                ctrl
            }
        );
        connect_txn_handle
            .send_on_connect_result(&mut fake_successful_connect_result())
            .expect("failed to send connection completion");
        // Progress the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Check for a disconnect update
        let client_state_update = ClientStateUpdate {
            state: fidl_policy::WlanClientState::ConnectionsEnabled,
            networks: vec![ClientNetworkState {
                id: id_1.clone(),
                state: fidl_policy::ConnectionState::Disconnected,
                status: Some(fidl_policy::DisconnectStatus::ConnectionStopped),
            }],
        };
        assert_variant!(
            test_values.update_receiver.try_next(),
            Ok(Some(listener::Message::NotifyListeners(updates))) => {
            assert_eq!(updates, client_state_update);
        });

        // Disconnect telemetry event sent
        assert_variant!(telemetry_receiver.try_next(), Ok(Some(event)) => {
            assert_variant!(event, TelemetryEvent::Disconnected { track_subsequent_downtime, info } => {
                assert!(!track_subsequent_downtime);
                assert_eq!(info, DisconnectInfo {
                    connected_duration: 12.hours(),
                    is_sme_reconnecting: false,
                    disconnect_source: fidl_sme::DisconnectSource::User(fidl_sme::UserDisconnectReason::ProactiveNetworkSwitch),
                    latest_ap_state: bss_description.clone(),
                });
            });
        });

        // Check the responder was acknowledged
        assert_variant!(exec.run_until_stalled(&mut connect_receiver2), Poll::Ready(Ok(())));

        // Check for a connecting update
        let client_state_update = ClientStateUpdate {
            state: fidl_policy::WlanClientState::ConnectionsEnabled,
            networks: vec![ClientNetworkState {
                id: types::NetworkIdentifier {
                    ssid: second_network_ssid.clone(),
                    security_type: types::SecurityType::Wpa2,
                },
                state: fidl_policy::ConnectionState::Connecting,
                status: None,
            }],
        };
        assert_variant!(
            test_values.update_receiver.try_next(),
            Ok(Some(listener::Message::NotifyListeners(updates))) => {
            assert_eq!(updates, client_state_update);
        });
        // Check for a connected update
        let client_state_update = ClientStateUpdate {
            state: fidl_policy::WlanClientState::ConnectionsEnabled,
            networks: vec![ClientNetworkState {
                id: types::NetworkIdentifier {
                    ssid: second_network_ssid.clone(),
                    security_type: types::SecurityType::Wpa2,
                },
                state: fidl_policy::ConnectionState::Connected,
                status: None,
            }],
        };
        assert_variant!(
            test_values.update_receiver.try_next(),
            Ok(Some(listener::Message::NotifyListeners(updates))) => {
            assert_eq!(updates, client_state_update);
        });

        // Progress the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Ensure no further updates were sent to listeners
        assert_variant!(
            exec.run_until_stalled(&mut test_values.update_receiver.into_future()),
            Poll::Pending
        );

        // Check that the first connection was recorded
        let expected_recorded_connection = ConnectionRecord {
            id: id_1.clone(),
            credential: credential_1.clone(),
            data: PastConnectionData {
                bssid: bss_description.bssid,
                connection_attempt_time,
                time_to_connect,
                disconnect_time,
                connection_uptime: zx::Duration::from_hours(12),
                disconnect_reason: types::DisconnectReason::ProactiveNetworkSwitch,
                signal_data_at_disconnect: bss_selection::SignalData::new(
                    bss_description.rssi_dbm,
                    bss_description.snr_db,
                    bss_selection::EWMA_SMOOTHING_FACTOR,
                ),
                // TODO: record average phy rate over connection once available
                average_tx_rate: 0,
            },
        };
        assert_variant!(test_values.saved_networks_manager.get_recorded_past_connections().as_slice(), [connection_data] => {
            assert_eq!(connection_data, &expected_recorded_connection);
        });
    }

    #[fuchsia::test]
    fn connected_state_notified_of_network_disconnect_no_sme_reconnect() {
        let mut exec = fasync::TestExecutor::new().expect("failed to create an executor");
        // Do test set up manually to get stash server
        let (_client_req_sender, client_req_stream) = mpsc::channel(1);
        let (update_sender, mut update_receiver) = mpsc::unbounded();
        let (sme_proxy, sme_server) =
            create_proxy::<fidl_sme::ClientSmeMarker>().expect("failed to create an sme channel");
        let sme_req_stream = sme_server.into_stream().expect("could not create SME request stream");
        let (saved_networks, mut stash_server) =
            exec.run_singlethreaded(SavedNetworksManager::new_and_stash_server());
        let saved_networks_manager = Arc::new(saved_networks);
        let (persistence_req_sender, _persistence_stream) = create_inspect_persistence_channel();
        let (telemetry_sender, _telemetry_receiver) = mpsc::channel::<TelemetryEvent>(100);
        let telemetry_sender = TelemetrySender::new(telemetry_sender);
        let (stats_sender, _stats_receiver) = mpsc::unbounded();
        let network_selector = Arc::new(network_selection::NetworkSelector::new(
            saved_networks_manager.clone(),
            create_wlan_hasher(),
            inspect::Inspector::new().root().create_child("network_selector"),
            persistence_req_sender,
            telemetry_sender.clone(),
        ));
        let network_ssid = types::Ssid::try_from("foo").unwrap();
        let bss_description = random_bss_description!(Wpa2, ssid: network_ssid.clone());
        let connect_request = types::ConnectRequest {
            target: types::ConnectionCandidate {
                network: types::NetworkIdentifier {
                    ssid: network_ssid.clone(),
                    security_type: types::SecurityType::Wpa2,
                },
                credential: Credential::Password("Anything".as_bytes().to_vec()),
                scanned: Some(types::ScannedCandidate {
                    bss_description: bss_description.clone().into(),
                    observation: types::ScanObservation::Passive,
                    has_multiple_bss_candidates: true,
                    security_type_detailed: types::SecurityTypeDetailed::Wpa2Personal,
                }),
            },
            reason: types::ConnectReason::RegulatoryChangeReconnect,
        };

        // Store the network in the saved_networks_manager, so we can record connection success
        let save_fut = saved_networks_manager.store(
            connect_request.target.network.clone().into(),
            connect_request.target.credential.clone(),
        );
        pin_mut!(save_fut);
        assert_variant!(exec.run_until_stalled(&mut save_fut), Poll::Pending);
        process_stash_write(&mut exec, &mut stash_server);
        assert_variant!(exec.run_until_stalled(&mut save_fut), Poll::Ready(Ok(None)));

        let common_options = CommonStateOptions {
            proxy: sme_proxy,
            req_stream: client_req_stream.fuse(),
            update_sender: update_sender,
            saved_networks_manager: saved_networks_manager.clone(),
            network_selector,
            telemetry_sender,
            iface_id: 1,
            has_wpa3_support: false,
            stats_sender,
        };
        let (connect_txn_proxy, connect_txn_stream) =
            create_proxy_and_stream::<fidl_sme::ConnectTransactionMarker>()
                .expect("failed to create a connect txn channel");
        let connect_txn_handle = connect_txn_stream.control_handle();
        let options = ConnectedOptions {
            currently_fulfilled_request: connect_request.clone(),
            latest_ap_state: Box::new(bss_description),
            multiple_bss_candidates: true,
            connect_txn_stream: connect_txn_proxy.take_event_stream(),
            connection_attempt_time: fasync::Time::now(),
            time_to_connect: zx::Duration::from_seconds(10),
        };
        let initial_state = connected_state(common_options, options);
        let fut = run_state_machine(initial_state);
        pin_mut!(fut);
        let sme_fut = sme_req_stream.into_future();
        pin_mut!(sme_fut);

        // Run the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // SME notifies Policy of disconnection.
        let is_sme_reconnecting = false;
        connect_txn_handle
            .send_on_disconnect(&mut generate_disconnect_info(is_sme_reconnecting))
            .expect("failed to send disconnection event");

        // Run the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Check for an SME disconnect request
        assert_variant!(
            poll_sme_req(&mut exec, &mut sme_fut),
            Poll::Ready(fidl_sme::ClientSmeRequest::Disconnect{ responder, reason: fidl_sme::UserDisconnectReason::DisconnectDetectedFromSme }) => {
                responder.send().expect("could not send sme response");
            }
        );
        // Progress the state machine
        // TODO(fxbug.dev/53505): remove this once the disconnect request is fire-and-forget
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Check for a disconnect update
        let client_state_update = ClientStateUpdate {
            state: fidl_policy::WlanClientState::ConnectionsEnabled,
            networks: vec![ClientNetworkState {
                id: types::NetworkIdentifier {
                    ssid: network_ssid.clone(),
                    security_type: types::SecurityType::Wpa2,
                },
                state: fidl_policy::ConnectionState::Disconnected,
                status: Some(fidl_policy::DisconnectStatus::ConnectionFailed),
            }],
        };
        assert_variant!(
            update_receiver.try_next(),
            Ok(Some(listener::Message::NotifyListeners(updates))) => {
            assert_eq!(updates, client_state_update);
        });

        // Check for a scan to find a new BSS to reconnect with
        let new_bss_desc = random_fidl_bss_description!(Wpa2, ssid: network_ssid.clone());
        let expected_scan_request = fidl_sme::ScanRequest::Active(fidl_sme::ActiveScanRequest {
            ssids: vec![network_ssid.to_vec()],
            channels: vec![],
        });
        let mut scan_results = vec![fidl_sme::ScanResult {
            compatible: true,
            timestamp_nanos: zx::Time::get_monotonic().into_nanos(),
            bss_description: new_bss_desc.clone(),
        }];

        assert_variant!(
            poll_sme_req(&mut exec, &mut sme_fut),
            Poll::Ready(fidl_sme::ClientSmeRequest::Scan {
                txn, req, control_handle: _
            }) => {
                // Validate the request
                assert_eq!(req, expected_scan_request);
                // Send all the APs
                let (_stream, ctrl) = txn
                    .into_stream_and_control_handle().expect("error accessing control handle");
                ctrl.send_on_result(&mut scan_results.iter_mut())
                    .expect("failed to send scan data");

                // Send the end of data
                ctrl.send_on_finished()
                    .expect("failed to send scan data");
            }
        );

        // Run the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Check for an SME request to reconnect
        assert_variant!(
            poll_sme_req(&mut exec, &mut sme_fut),
            Poll::Ready(fidl_sme::ClientSmeRequest::Connect{ req, txn, control_handle: _ }) => {
                assert_eq!(req.ssid, network_ssid.to_vec());
                assert_eq!(req.bss_description, new_bss_desc.clone());
                assert_eq!(req.multiple_bss_candidates, false);
                 // Send connection response.
                let (_stream, ctrl) = txn.expect("connect txn unused")
                    .into_stream_and_control_handle().expect("error accessing control handle");
                ctrl
                    .send_on_connect_result(&mut fake_successful_connect_result())
                    .expect("failed to send connection completion");
            }
        );

        // Check for a connecting update
        let client_state_update = ClientStateUpdate {
            state: fidl_policy::WlanClientState::ConnectionsEnabled,
            networks: vec![ClientNetworkState {
                id: types::NetworkIdentifier {
                    ssid: network_ssid.clone(),
                    security_type: types::SecurityType::Wpa2,
                },
                state: fidl_policy::ConnectionState::Connecting,
                status: None,
            }],
        };
        assert_variant!(
            update_receiver.try_next(),
            Ok(Some(listener::Message::NotifyListeners(updates))) => {
            assert_eq!(updates, client_state_update);
        });
    }

    #[fuchsia::test]
    fn connected_state_notified_of_network_disconnect_sme_reconnect_successfully() {
        let mut exec = fasync::TestExecutor::new().expect("failed to create an executor");
        let mut test_values = test_setup();

        let network_ssid = types::Ssid::try_from("foo").unwrap();
        let bss_description = random_bss_description!(Wpa2, ssid: network_ssid.clone());
        let connect_request = types::ConnectRequest {
            target: types::ConnectionCandidate {
                network: types::NetworkIdentifier {
                    ssid: network_ssid.clone(),
                    security_type: types::SecurityType::Wpa2,
                },
                credential: Credential::None,
                scanned: Some(types::ScannedCandidate {
                    bss_description: bss_description.clone().into(),
                    observation: types::ScanObservation::Passive,
                    has_multiple_bss_candidates: true,
                    security_type_detailed: types::SecurityTypeDetailed::Wpa2Personal,
                }),
            },
            reason: types::ConnectReason::IdleInterfaceAutoconnect,
        };
        let (connect_txn_proxy, connect_txn_stream) =
            create_proxy_and_stream::<fidl_sme::ConnectTransactionMarker>()
                .expect("failed to create a connect txn channel");
        let connect_txn_handle = connect_txn_stream.control_handle();
        let options = ConnectedOptions {
            currently_fulfilled_request: connect_request.clone(),
            latest_ap_state: Box::new(bss_description),
            multiple_bss_candidates: true,
            connect_txn_stream: connect_txn_proxy.take_event_stream(),
            connection_attempt_time: fasync::Time::now(),
            time_to_connect: zx::Duration::from_seconds(10),
        };
        let initial_state = connected_state(test_values.common_options, options);
        let fut = run_state_machine(initial_state);
        pin_mut!(fut);

        // Run the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // SME notifies Policy of disconnection
        let is_sme_reconnecting = true;
        connect_txn_handle
            .send_on_disconnect(&mut generate_disconnect_info(is_sme_reconnecting))
            .expect("failed to send disconnection event");

        // Run the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // SME notifies Policy that reconnects succeeds
        let mut connect_result =
            fidl_sme::ConnectResult { is_reconnect: true, ..fake_successful_connect_result() };
        connect_txn_handle
            .send_on_connect_result(&mut connect_result)
            .expect("failed to send reconnection result");

        // Run the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Check there were no state updates
        assert_variant!(test_values.update_receiver.try_next(), Err(_));
    }

    #[fuchsia::test]
    fn connected_state_notified_of_network_disconnect_sme_reconnect_unsuccessfully() {
        let mut exec = fasync::TestExecutor::new().expect("failed to create an executor");
        let mut test_values = test_setup();
        let (saved_networks, mut stash_server) =
            exec.run_singlethreaded(SavedNetworksManager::new_and_stash_server());
        let saved_networks_manager = Arc::new(saved_networks);
        test_values.common_options.saved_networks_manager = saved_networks_manager.clone();
        let (persistence_req_sender, _persistence_stream) = create_inspect_persistence_channel();
        let (telemetry_sender, _telemetry_receiver) = mpsc::channel::<TelemetryEvent>(100);
        let network_selector = Arc::new(network_selection::NetworkSelector::new(
            saved_networks_manager.clone(),
            create_wlan_hasher(),
            inspect::Inspector::new().root().create_child("network_selector"),
            persistence_req_sender,
            TelemetrySender::new(telemetry_sender),
        ));
        test_values.common_options.network_selector = network_selector;

        let network_ssid = types::Ssid::try_from("foo").unwrap();
        let bss_description = random_bss_description!(Wpa2, ssid: network_ssid.clone());
        let connect_request = types::ConnectRequest {
            target: types::ConnectionCandidate {
                network: types::NetworkIdentifier {
                    ssid: network_ssid.clone(),
                    security_type: types::SecurityType::Wpa2,
                },
                credential: Credential::Password("Anything".as_bytes().to_vec()),
                scanned: Some(types::ScannedCandidate {
                    bss_description: bss_description.clone().into(),
                    observation: types::ScanObservation::Passive,
                    has_multiple_bss_candidates: true,
                    security_type_detailed: types::SecurityTypeDetailed::Wpa2Personal,
                }),
            },
            reason: types::ConnectReason::IdleInterfaceAutoconnect,
        };

        // Store the network in the saved_networks_manager, so we can record connection success
        let save_fut = saved_networks_manager.store(
            connect_request.target.network.clone().into(),
            connect_request.target.credential.clone(),
        );
        pin_mut!(save_fut);
        assert_variant!(exec.run_until_stalled(&mut save_fut), Poll::Pending);
        process_stash_write(&mut exec, &mut stash_server);
        assert_variant!(exec.run_until_stalled(&mut save_fut), Poll::Ready(Ok(None)));

        let (connect_txn_proxy, connect_txn_stream) =
            create_proxy_and_stream::<fidl_sme::ConnectTransactionMarker>()
                .expect("failed to create a connect txn channel");
        let mut connect_txn_handle = connect_txn_stream.control_handle();
        let options = ConnectedOptions {
            currently_fulfilled_request: connect_request.clone(),
            latest_ap_state: Box::new(bss_description),
            multiple_bss_candidates: true,
            connect_txn_stream: connect_txn_proxy.take_event_stream(),
            connection_attempt_time: fasync::Time::now(),
            time_to_connect: zx::Duration::from_seconds(10),
        };
        let initial_state = connected_state(test_values.common_options, options);
        let fut = run_state_machine(initial_state);
        pin_mut!(fut);
        let sme_fut = test_values.sme_req_stream.into_future();
        pin_mut!(sme_fut);

        // Run the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // SME notifies Policy of disconnection
        let is_sme_reconnecting = true;
        connect_txn_handle
            .send_on_disconnect(&mut generate_disconnect_info(is_sme_reconnecting))
            .expect("failed to send disconnection event");

        // Run the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // SME notifies Policy that reconnects fails
        let mut connect_result = fidl_sme::ConnectResult {
            code: fidl_ieee80211::StatusCode::RefusedReasonUnspecified,
            is_reconnect: true,
            ..fake_successful_connect_result()
        };
        connect_txn_handle
            .send_on_connect_result(&mut connect_result)
            .expect("failed to send reconnection result");

        // Run the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Check for an SME disconnect request
        assert_variant!(
            poll_sme_req(&mut exec, &mut sme_fut),
            Poll::Ready(fidl_sme::ClientSmeRequest::Disconnect{ responder, reason: fidl_sme::UserDisconnectReason::DisconnectDetectedFromSme }) => {
                responder.send().expect("could not send sme response");
            }
        );
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Check for a disconnect update
        let client_state_update = ClientStateUpdate {
            state: fidl_policy::WlanClientState::ConnectionsEnabled,
            networks: vec![ClientNetworkState {
                id: types::NetworkIdentifier {
                    ssid: network_ssid.clone(),
                    security_type: types::SecurityType::Wpa2,
                },
                state: fidl_policy::ConnectionState::Disconnected,
                status: Some(fidl_policy::DisconnectStatus::ConnectionFailed),
            }],
        };
        assert_variant!(
            test_values.update_receiver.try_next(),
            Ok(Some(listener::Message::NotifyListeners(updates))) => {
            assert_eq!(updates, client_state_update);
        });

        // Check for a scan to find a new BSS to reconnect with
        let new_bss_description = random_fidl_bss_description!(Wpa2, ssid: network_ssid.clone());
        let expected_scan_request = fidl_sme::ScanRequest::Active(fidl_sme::ActiveScanRequest {
            ssids: vec![network_ssid.to_vec()],
            channels: vec![],
        });
        let mut scan_results = vec![fidl_sme::ScanResult {
            compatible: true,
            timestamp_nanos: zx::Time::get_monotonic().into_nanos(),
            bss_description: new_bss_description.clone(),
        }];
        assert_variant!(
            poll_sme_req(&mut exec, &mut sme_fut),
            Poll::Ready(fidl_sme::ClientSmeRequest::Scan {
                txn, req, control_handle: _
            }) => {
                // Validate the request
                assert_eq!(req, expected_scan_request);
                // Send all the APs
                let (_stream, ctrl) = txn
                    .into_stream_and_control_handle().expect("error accessing control handle");
                ctrl.send_on_result(&mut scan_results.iter_mut())
                    .expect("failed to send scan data");

                // Send the end of data
                ctrl.send_on_finished()
                    .expect("failed to send scan data");
            }
        );

        // Run the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Check for an SME request to reconnect
        connect_txn_handle = assert_variant!(
            poll_sme_req(&mut exec, &mut sme_fut),
            Poll::Ready(fidl_sme::ClientSmeRequest::Connect{ req, txn, control_handle: _ }) => {
                assert_eq!(req.ssid, network_ssid.to_vec());
                assert_eq!(req.bss_description, new_bss_description.clone());
                assert_eq!(req.multiple_bss_candidates, false);
                 // Send connection response.
                let (_stream, ctrl) = txn.expect("connect txn unused")
                    .into_stream_and_control_handle().expect("error accessing control handle");
                ctrl
            }
        );
        connect_txn_handle
            .send_on_connect_result(&mut fake_successful_connect_result())
            .expect("failed to send connection completion");

        // Check for a connecting update
        let client_state_update = ClientStateUpdate {
            state: fidl_policy::WlanClientState::ConnectionsEnabled,
            networks: vec![ClientNetworkState {
                id: types::NetworkIdentifier {
                    ssid: network_ssid.clone(),
                    security_type: types::SecurityType::Wpa2,
                },
                state: fidl_policy::ConnectionState::Connecting,
                status: None,
            }],
        };
        assert_variant!(
            test_values.update_receiver.try_next(),
            Ok(Some(listener::Message::NotifyListeners(updates))) => {
            assert_eq!(updates, client_state_update);
        });
    }

    #[fuchsia::test]
    fn connected_state_on_signal_report() {
        let mut exec =
            fasync::TestExecutor::new_with_fake_time().expect("failed to create an executor");
        exec.set_fake_time(fasync::Time::from_nanos(0));

        let mut test_values = test_setup();
        let mut telemetry_receiver = test_values.telemetry_receiver;

        let network_ssid = types::Ssid::try_from("test").unwrap();
        let init_rssi = -40;
        let init_snr = 30;
        let bss_description = random_bss_description!(Wpa2, ssid: network_ssid.clone(), rssi_dbm: init_rssi, snr_db: init_snr);
        // Add a PastConnectionData for the connected network to be send in BSS quality data.
        let mut past_connections = PastConnectionList::new();
        let mut past_connection_data = random_connection_data();
        past_connection_data.bssid = bss_description.bssid;
        past_connections.add(past_connection_data);
        let mut saved_networks_manager = FakeSavedNetworksManager::new();
        saved_networks_manager.past_connections_response = past_connections.clone();
        test_values.common_options.saved_networks_manager = Arc::new(saved_networks_manager);

        // Set up the state machine, starting at the connected state.
        let (initial_state, connect_txn_stream) =
            connected_state_setup(test_values.common_options, bss_description.clone());
        let connect_txn_handle = connect_txn_stream.control_handle();
        let fut = run_state_machine(initial_state);
        pin_mut!(fut);
        let sme_fut = test_values.sme_req_stream.into_future();
        pin_mut!(sme_fut);

        // Send the first signal report from SME
        let rssi_1 = -50;
        let snr_1 = 25;
        let mut fidl_signal_report =
            fidl_internal::SignalReportIndication { rssi_dbm: rssi_1, snr_db: snr_1 };
        connect_txn_handle
            .send_on_signal_report(&mut fidl_signal_report)
            .expect("failed to send signal report");
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Verify telemetry event for signal report data then RSSI data.
        assert_variant!(telemetry_receiver.try_next(), Ok(Some(TelemetryEvent::OnSignalReport {ind, rssi_velocity})) => {
            assert_eq!(ind, fidl_signal_report);
            // verify that RSSI velocity is negative since the signal report RSSI is lower.
            assert_lt!(rssi_velocity, 0);
        });

        // Do a quick check that state machine does not exist and there's no disconnect to SME
        assert_variant!(poll_sme_req(&mut exec, &mut sme_fut), Poll::Pending);

        // Verify that connection stats are sent out
        let id = types::NetworkIdentifier {
            ssid: network_ssid,
            security_type: types::SecurityType::Wpa2,
        };
        let stats = test_values
            .stats_receiver
            .try_next()
            .expect("failed to get connection stats")
            .expect("next connection stats is missing");
        // Test setup always use iface ID 1.
        assert_eq!(stats.iface_id, 1);
        assert_eq!(stats.id, id);
        // EWMA RSSI and SNR should be between the initial and the newest values.
        let ewma_rssi_1 = stats.quality_data.signal_data.ewma_rssi;
        assert_lt!(ewma_rssi_1.get(), init_rssi);
        assert_gt!(ewma_rssi_1.get(), rssi_1);
        let ewma_snr_1 = stats.quality_data.signal_data.ewma_snr;
        assert_lt!(ewma_snr_1.get(), init_snr);
        assert_gt!(ewma_snr_1.get(), snr_1);
        // Check that RSSI velocity is negative.
        let rssi_velocity_1 = stats.quality_data.signal_data.rssi_velocity;
        assert_lt!(rssi_velocity_1, 0);
        // Check that the BssQualityData includes the past connection data.
        assert_eq!(stats.quality_data.past_connections_list, past_connections.clone());
        // Check that the channel is included.
        assert_eq!(stats.quality_data.channel, bss_description.channel);

        // Send a second signal report with higher RSSI and SNR than the previous reports.
        let rssi_1 = -30;
        let snr_1 = 35;
        let mut fidl_signal_report =
            fidl_internal::SignalReportIndication { rssi_dbm: rssi_1, snr_db: snr_1 };
        connect_txn_handle
            .send_on_signal_report(&mut fidl_signal_report)
            .expect("failed to send signal report");
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Verify that a telemetry event is sent
        assert_variant!(telemetry_receiver.try_next(), Ok(Some(event)) => {
            assert_variant!(event, TelemetryEvent::OnSignalReport { ind, rssi_velocity } => {
                assert_eq!(ind, fidl_signal_report);
                // Velocity should be greater than previous one since RSSI is higher,
                assert_gt!(rssi_velocity, rssi_velocity_1);
            });
        });

        // Verify that the new EWMA values are higher than the previous values.
        let stats = test_values
            .stats_receiver
            .try_next()
            .expect("failed to get connection stats")
            .expect("next connection stats is missing");
        assert_eq!(stats.iface_id, 1);
        assert_eq!(stats.id, id);
        // Check that EWMA RSSI and SNR values are greater than the previous values.
        assert_gt!(stats.quality_data.signal_data.ewma_rssi.get(), ewma_rssi_1.get());
        assert_gt!(stats.quality_data.signal_data.ewma_snr.get(), ewma_snr_1.get());
        // Check that RSSI velocity is greater than the previous velocity.
        assert_gt!(stats.quality_data.signal_data.rssi_velocity, rssi_velocity_1);
        // Check that the BssQualityData includes the past connection data.
        assert_eq!(stats.quality_data.past_connections_list, past_connections);
        // Check that the channel is included.
        assert_eq!(stats.quality_data.channel, bss_description.channel);
    }

    #[fuchsia::test]
    fn connected_state_should_roam() {
        let mut exec =
            fasync::TestExecutor::new_with_fake_time().expect("failed to create an executor");
        exec.set_fake_time(fasync::Time::from_nanos(0));

        let test_values = test_setup();
        let mut telemetry_receiver = test_values.telemetry_receiver;

        let network_ssid = types::Ssid::try_from("test").unwrap();
        let init_rssi = -75;
        let init_snr = 30;
        let bss_description = random_bss_description!(
            Wpa2,
            ssid: network_ssid.clone(),
            rssi_dbm: init_rssi,
            snr_db: init_snr,
        );

        // Set up the state machine, starting at the connected state.
        let (initial_state, connect_txn_stream) =
            connected_state_setup(test_values.common_options, bss_description.clone());
        let connect_txn_handle = connect_txn_stream.control_handle();
        let fut = run_state_machine(initial_state);
        pin_mut!(fut);

        // Send a signal report indicating the connection is weak.
        let rssi_1 = -90;
        let snr_1 = 25;
        let mut fidl_signal_report =
            fidl_internal::SignalReportIndication { rssi_dbm: rssi_1, snr_db: snr_1 };
        connect_txn_handle
            .send_on_signal_report(&mut fidl_signal_report)
            .expect("failed to send signal report");
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Verify that telemetry events are sent for the signal report and a Roam Scan.
        assert_variant!(
            telemetry_receiver.try_next(),
            Ok(Some(TelemetryEvent::OnSignalReport {ind, rssi_velocity})) => {
                assert_eq!(ind, fidl_signal_report);
                // verify that RSSI velocity is negative since the signal report RSSI is lower.
                assert_lt!(rssi_velocity, 0);
            }
        );
        assert_variant!(telemetry_receiver.try_next(), Ok(Some(TelemetryEvent::RoamingScan {})));
    }

    #[fuchsia::test]
    fn connected_state_on_channel_switched() {
        let mut exec =
            fasync::TestExecutor::new_with_fake_time().expect("failed to create an executor");
        exec.set_fake_time(fasync::Time::from_nanos(0));

        let test_values = test_setup();
        let mut telemetry_receiver = test_values.telemetry_receiver;

        let network_ssid = types::Ssid::try_from("test").unwrap();
        let bss_description = random_bss_description!(Wpa2, ssid: network_ssid.clone());
        let (initial_state, connect_txn_stream) =
            connected_state_setup(test_values.common_options, bss_description);
        let connect_txn_handle = connect_txn_stream.control_handle();
        let fut = run_state_machine(initial_state);
        pin_mut!(fut);

        // Run the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        let mut channel_switch_info = fidl_internal::ChannelSwitchInfo { new_channel: 10 };
        connect_txn_handle
            .send_on_channel_switched(&mut channel_switch_info)
            .expect("failed to send signal report");
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Verify telemetry event
        assert_variant!(telemetry_receiver.try_next(), Ok(Some(event)) => {
            assert_variant!(event, TelemetryEvent::OnChannelSwitched { info } => {
                assert_eq!(info, channel_switch_info);
            });
        });

        // Have SME notify Policy of disconnection so we can see whether the channel in the
        // BssDescription has changed.
        let is_sme_reconnecting = false;
        let mut fidl_disconnect_info = generate_disconnect_info(is_sme_reconnecting);
        connect_txn_handle
            .send_on_disconnect(&mut fidl_disconnect_info)
            .expect("failed to send disconnection event");
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Verify telemetry event
        assert_variant!(telemetry_receiver.try_next(), Ok(Some(event)) => {
            assert_variant!(event, TelemetryEvent::Disconnected { info, .. } => {
                assert_eq!(info.latest_ap_state.channel.primary, 10);
            });
        });
    }

    // Set up connected state, returning its fut, connect_txn_stream, and a view into
    // BssDescription held by the connected state
    fn connected_state_setup(
        common_options: CommonStateOptions,
        bss_description: BssDescription,
    ) -> (impl Future<Output = Result<State, ExitReason>>, fidl_sme::ConnectTransactionRequestStream)
    {
        let protection = bss_description.protection();
        let connect_request = types::ConnectRequest {
            target: types::ConnectionCandidate {
                network: types::NetworkIdentifier {
                    ssid: bss_description.ssid.clone(),
                    security_type: match protection {
                        Protection::Open => types::SecurityType::None,
                        Protection::Wep => types::SecurityType::Wep,
                        Protection::Wpa1 => types::SecurityType::Wpa,
                        Protection::Wpa2Personal => types::SecurityType::Wpa2,
                        Protection::Wpa3Personal => types::SecurityType::Wpa3,
                        _ => panic!("unsupported BssDescription protection type for unit tests."),
                    },
                },
                credential: Credential::None,
                scanned: Some(types::ScannedCandidate {
                    bss_description: bss_description.clone().into(),
                    observation: types::ScanObservation::Passive,
                    has_multiple_bss_candidates: true,
                    security_type_detailed: protection.into(),
                }),
            },
            reason: types::ConnectReason::RegulatoryChangeReconnect,
        };
        let (connect_txn_proxy, connect_txn_stream) =
            create_proxy_and_stream::<fidl_sme::ConnectTransactionMarker>()
                .expect("failed to create a connect txn channel");
        let options = ConnectedOptions {
            currently_fulfilled_request: connect_request,
            latest_ap_state: Box::new(bss_description.clone()),
            multiple_bss_candidates: true,
            connect_txn_stream: connect_txn_proxy.take_event_stream(),
            connection_attempt_time: fasync::Time::now(),
            time_to_connect: zx::Duration::from_seconds(10),
        };
        let initial_state = connected_state(common_options, options);
        (initial_state, connect_txn_stream)
    }

    #[fuchsia::test]
    fn disconnecting_state_completes_and_exits() {
        let mut exec = fasync::TestExecutor::new().expect("failed to create an executor");
        let mut test_values = test_setup();

        let (sender, mut receiver) = oneshot::channel();
        let disconnecting_options = DisconnectingOptions {
            disconnect_responder: Some(sender),
            previous_network: None,
            next_network: None,
            reason: types::DisconnectReason::RegulatoryRegionChange,
        };
        let initial_state = disconnecting_state(test_values.common_options, disconnecting_options);
        let fut = run_state_machine(initial_state);
        pin_mut!(fut);
        let sme_fut = test_values.sme_req_stream.into_future();
        pin_mut!(sme_fut);

        // Run the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Ensure a disconnect request is sent to the SME
        assert_variant!(
            poll_sme_req(&mut exec, &mut sme_fut),
            Poll::Ready(fidl_sme::ClientSmeRequest::Disconnect{ responder, reason: fidl_sme::UserDisconnectReason::RegulatoryRegionChange }) => {
                responder.send().expect("could not send sme response");
            }
        );

        // Ensure the state machine exits once the disconnect is processed.
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Ready(()));

        // The state machine should have sent a listener update
        assert_variant!(
            test_values.update_receiver.try_next(),
            Ok(Some(listener::Message::NotifyListeners(ClientStateUpdate {
                state: fidl_policy::WlanClientState::ConnectionsEnabled,
                networks
            }))) => {
                assert!(networks.is_empty());
            }
        );

        // Expect the responder to be acknowledged
        assert_variant!(exec.run_until_stalled(&mut receiver), Poll::Ready(Ok(())));
    }

    #[fuchsia::test]
    fn disconnecting_state_completes_disconnect_to_connecting() {
        let mut exec = fasync::TestExecutor::new().expect("failed to create an executor");
        let mut test_values = test_setup();

        let previous_network_ssid = types::Ssid::try_from("foo").unwrap();
        let next_network_ssid = types::Ssid::try_from("bar").unwrap();
        let bss_description = random_fidl_bss_description!(Wpa2, ssid: next_network_ssid.clone());
        let connect_request = types::ConnectRequest {
            target: types::ConnectionCandidate {
                network: types::NetworkIdentifier {
                    ssid: next_network_ssid.clone(),
                    security_type: types::SecurityType::Wpa2,
                },
                credential: Credential::Password(b"password".to_vec()),
                scanned: Some(types::ScannedCandidate {
                    bss_description: bss_description.clone(),
                    observation: types::ScanObservation::Passive,
                    has_multiple_bss_candidates: true,
                    security_type_detailed: types::SecurityTypeDetailed::Wpa2Personal,
                }),
            },
            reason: types::ConnectReason::ProactiveNetworkSwitch,
        };
        let (connect_sender, _connect_receiver) = oneshot::channel();
        let connecting_options = ConnectingOptions {
            connect_responder: Some(connect_sender),
            connect_request: connect_request.clone(),
            attempt_counter: 0,
        };
        let (disconnect_sender, mut disconnect_receiver) = oneshot::channel();
        // Include both a "previous" and "next" network
        let disconnecting_options = DisconnectingOptions {
            disconnect_responder: Some(disconnect_sender),
            previous_network: Some((
                types::NetworkIdentifier {
                    ssid: previous_network_ssid.clone(),
                    security_type: types::SecurityType::Wpa2,
                },
                fidl_policy::DisconnectStatus::ConnectionStopped,
            )),
            next_network: Some(connecting_options),
            reason: types::DisconnectReason::ProactiveNetworkSwitch,
        };
        let initial_state = disconnecting_state(test_values.common_options, disconnecting_options);
        let fut = run_state_machine(initial_state);
        pin_mut!(fut);
        let sme_fut = test_values.sme_req_stream.into_future();
        pin_mut!(sme_fut);

        // Run the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Ensure a disconnect request is sent to the SME
        assert_variant!(
            poll_sme_req(&mut exec, &mut sme_fut),
            Poll::Ready(fidl_sme::ClientSmeRequest::Disconnect{ responder, reason: fidl_sme::UserDisconnectReason::ProactiveNetworkSwitch }) => {
                responder.send().expect("could not send sme response");
            }
        );

        // Progress the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Check for a disconnect update and the disconnect responder
        let client_state_update = ClientStateUpdate {
            state: fidl_policy::WlanClientState::ConnectionsEnabled,
            networks: vec![ClientNetworkState {
                id: types::NetworkIdentifier {
                    ssid: previous_network_ssid.clone(),
                    security_type: types::SecurityType::Wpa2,
                },
                state: fidl_policy::ConnectionState::Disconnected,
                status: Some(fidl_policy::DisconnectStatus::ConnectionStopped),
            }],
        };
        assert_variant!(
            test_values.update_receiver.try_next(),
            Ok(Some(listener::Message::NotifyListeners(updates))) => {
            assert_eq!(updates, client_state_update);
        });
        assert_variant!(exec.run_until_stalled(&mut disconnect_receiver), Poll::Ready(Ok(())));

        // Ensure a connect request is sent to the SME
        assert_variant!(
            poll_sme_req(&mut exec, &mut sme_fut),
            Poll::Ready(fidl_sme::ClientSmeRequest::Connect{ req, txn, control_handle: _ }) => {
                assert_eq!(req.ssid, next_network_ssid.to_vec());
                assert_eq!(connect_request.target.credential, req.authentication.credentials);
                assert_eq!(req.deprecated_scan_type, fidl_fuchsia_wlan_common::ScanType::Active);
                assert_eq!(req.bss_description, bss_description.clone());
                assert_eq!(req.multiple_bss_candidates, true);
                 // Send connection response.
                let (_stream, ctrl) = txn.expect("connect txn unused")
                    .into_stream_and_control_handle().expect("error accessing control handle");
                ctrl
                    .send_on_connect_result(&mut fake_successful_connect_result())
                    .expect("failed to send connection completion");
            }
        );
    }

    #[fuchsia::test]
    fn disconnecting_state_has_broken_sme() {
        let mut exec = fasync::TestExecutor::new().expect("failed to create an executor");
        let test_values = test_setup();

        let (sender, mut receiver) = oneshot::channel();
        let disconnecting_options = DisconnectingOptions {
            disconnect_responder: Some(sender),
            previous_network: None,
            next_network: None,
            reason: types::DisconnectReason::NetworkConfigUpdated,
        };
        let initial_state = disconnecting_state(test_values.common_options, disconnecting_options);
        let fut = run_state_machine(initial_state);
        pin_mut!(fut);

        // Break the SME by dropping the server end of the SME stream, so it causes an error
        drop(test_values.sme_req_stream);

        // Ensure the state machine exits
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Ready(()));

        // Expect the responder to have an error
        assert_variant!(exec.run_until_stalled(&mut receiver), Poll::Ready(Err(_)));
    }

    #[fuchsia::test]
    fn serve_loop_handles_startup() {
        let mut exec = fasync::TestExecutor::new().expect("failed to create an executor");
        let test_values = test_setup();
        let sme_proxy = test_values.common_options.proxy;
        let sme_event_stream = sme_proxy.take_event_stream();
        let (_client_req_sender, client_req_stream) = mpsc::channel(1);
        let sme_fut = test_values.sme_req_stream.into_future();
        pin_mut!(sme_fut);

        // Create a connect request so that the state machine does not immediately exit.
        let connect_req = types::ConnectRequest {
            target: types::ConnectionCandidate {
                network: types::NetworkIdentifier {
                    ssid: types::Ssid::try_from("no_password").unwrap(),
                    security_type: types::SecurityType::None,
                },
                credential: Credential::None,
                scanned: None,
            },
            reason: types::ConnectReason::IdleInterfaceAutoconnect,
        };
        let (sender, _receiver) = oneshot::channel();

        let fut = serve(
            0,
            false,
            sme_proxy,
            sme_event_stream,
            client_req_stream,
            test_values.common_options.update_sender,
            test_values.common_options.saved_networks_manager,
            Some((connect_req, sender)),
            test_values.common_options.network_selector,
            test_values.common_options.telemetry_sender,
            test_values.common_options.stats_sender,
        );
        pin_mut!(fut);

        // Run the state machine so it sends the initial SME disconnect request.
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);
        assert_variant!(
            poll_sme_req(&mut exec, &mut sme_fut),
            Poll::Ready(fidl_sme::ClientSmeRequest::Disconnect{ responder, reason: fidl_sme::UserDisconnectReason::Startup }) => {
                responder.send().expect("could not send sme response");
            }
        );

        // Run the future again and ensure that it has not exited after receiving the response.
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);
    }

    #[fuchsia::test]
    fn serve_loop_handles_sme_disappearance() {
        let mut exec = fasync::TestExecutor::new().expect("failed to create an executor");
        let test_values = test_setup();
        let (_client_req_sender, client_req_stream) = mpsc::channel(1);

        // Make our own SME proxy for this test
        let (sme_proxy, sme_server) =
            create_proxy::<fidl_sme::ClientSmeMarker>().expect("failed to create an sme channel");
        let (sme_req_stream, sme_control_handle) = sme_server
            .into_stream_and_control_handle()
            .expect("could not create SME request stream");

        let sme_fut = sme_req_stream.into_future();
        pin_mut!(sme_fut);

        let sme_event_stream = sme_proxy.take_event_stream();

        // Create a connect request so that the state machine does not immediately exit.
        let connect_req = types::ConnectRequest {
            target: types::ConnectionCandidate {
                network: types::NetworkIdentifier {
                    ssid: types::Ssid::try_from("no_password").unwrap(),
                    security_type: types::SecurityType::None,
                },
                credential: Credential::None,
                scanned: None,
            },
            reason: types::ConnectReason::FidlConnectRequest,
        };
        let (sender, _receiver) = oneshot::channel();

        let fut = serve(
            0,
            false,
            sme_proxy,
            sme_event_stream,
            client_req_stream,
            test_values.common_options.update_sender,
            test_values.common_options.saved_networks_manager,
            Some((connect_req, sender)),
            test_values.common_options.network_selector,
            test_values.common_options.telemetry_sender,
            test_values.common_options.stats_sender,
        );
        pin_mut!(fut);

        // Run the state machine so it sends the initial SME disconnect request.
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);
        assert_variant!(
            poll_sme_req(&mut exec, &mut sme_fut),
            Poll::Ready(fidl_sme::ClientSmeRequest::Disconnect{ responder, reason: fidl_sme::UserDisconnectReason::Startup }) => {
                responder.send().expect("could not send sme response");
            }
        );

        // Run the future again and ensure that it has not exited after receiving the response.
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        sme_control_handle.shutdown_with_epitaph(zx::Status::UNAVAILABLE);

        // Ensure the state machine has no further actions and is exited
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Ready(()));
    }

    #[fuchsia::test]
    fn serve_loop_handles_disconnect() {
        let mut exec = fasync::TestExecutor::new().expect("failed to create an executor");
        let test_values = test_setup();
        let sme_proxy = test_values.common_options.proxy;
        let sme_event_stream = sme_proxy.take_event_stream();
        let (client_req_sender, client_req_stream) = mpsc::channel(1);
        let sme_fut = test_values.sme_req_stream.into_future();
        pin_mut!(sme_fut);

        // Create a connect request so that the state machine does not immediately exit.
        let ssid = "no_password".as_bytes().to_vec();
        let bss_description = random_fidl_bss_description!(Open);
        let connect_req = types::ConnectRequest {
            target: types::ConnectionCandidate {
                network: types::NetworkIdentifier {
                    ssid: types::Ssid::try_from(ssid.clone()).unwrap(),
                    security_type: types::SecurityType::None,
                },
                credential: Credential::None,
                scanned: Some(types::ScannedCandidate {
                    bss_description: bss_description.clone(),
                    observation: types::ScanObservation::Passive,
                    has_multiple_bss_candidates: true,
                    security_type_detailed: types::SecurityTypeDetailed::Open,
                }),
            },
            reason: types::ConnectReason::RegulatoryChangeReconnect,
        };
        let (sender, _receiver) = oneshot::channel();

        let fut = serve(
            0,
            false,
            sme_proxy,
            sme_event_stream,
            client_req_stream,
            test_values.common_options.update_sender,
            test_values.common_options.saved_networks_manager,
            Some((connect_req, sender)),
            test_values.common_options.network_selector,
            test_values.common_options.telemetry_sender,
            test_values.common_options.stats_sender,
        );
        pin_mut!(fut);

        // Run the state machine so it sends the initial SME disconnect request.
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);
        assert_variant!(
            poll_sme_req(&mut exec, &mut sme_fut),
            Poll::Ready(fidl_sme::ClientSmeRequest::Disconnect{ responder, reason: fidl_sme::UserDisconnectReason::Startup }) => {
                responder.send().expect("could not send sme response");
            }
        );

        // Run the future again and ensure that it has not exited after receiving the response.
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Absorb the connect request.
        let connect_txn_handle = assert_variant!(
            poll_sme_req(&mut exec, &mut sme_fut),
            Poll::Ready(fidl_sme::ClientSmeRequest::Connect{ req: _, txn, control_handle: _ }) => {
                // Send connection response.
                let (_stream, ctrl) = txn.expect("connect txn unused")
                    .into_stream_and_control_handle().expect("error accessing control handle");
                ctrl
            }
        );
        connect_txn_handle
            .send_on_connect_result(&mut fake_successful_connect_result())
            .expect("failed to send connection completion");
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Send a disconnect request
        let mut client = Client::new(client_req_sender);
        let (sender, mut receiver) = oneshot::channel();
        client
            .disconnect(
                PolicyDisconnectionMigratedMetricDimensionReason::NetworkConfigUpdated,
                sender,
            )
            .expect("failed to make request");

        // Run the state machine so that it handles the disconnect message.
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);
        assert_variant!(
            poll_sme_req(&mut exec, &mut sme_fut),
            Poll::Ready(fidl_sme::ClientSmeRequest::Disconnect{ responder, reason: fidl_sme::UserDisconnectReason::NetworkConfigUpdated }) => {
                responder.send().expect("could not send sme response");
            }
        );

        // The state machine should exit following the disconnect request.
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Ready(()));

        // Expect the responder to be acknowledged
        assert_variant!(exec.run_until_stalled(&mut receiver), Poll::Ready(Ok(())));
    }

    #[fuchsia::test]
    fn serve_loop_handles_state_machine_error() {
        let mut exec = fasync::TestExecutor::new().expect("failed to create an executor");
        let test_values = test_setup();
        let sme_proxy = test_values.common_options.proxy;
        let sme_event_stream = sme_proxy.take_event_stream();
        let (_client_req_sender, client_req_stream) = mpsc::channel(1);

        // Create a connect request so that the state machine does not immediately exit.
        let connect_req = types::ConnectRequest {
            target: types::ConnectionCandidate {
                network: types::NetworkIdentifier {
                    ssid: types::Ssid::try_from("no_password").unwrap(),
                    security_type: types::SecurityType::None,
                },
                credential: Credential::None,
                scanned: None,
            },
            reason: types::ConnectReason::FidlConnectRequest,
        };
        let (sender, _receiver) = oneshot::channel();

        let fut = serve(
            0,
            false,
            sme_proxy,
            sme_event_stream,
            client_req_stream,
            test_values.common_options.update_sender,
            test_values.common_options.saved_networks_manager,
            Some((connect_req, sender)),
            test_values.common_options.network_selector,
            test_values.common_options.telemetry_sender,
            test_values.common_options.stats_sender,
        );
        pin_mut!(fut);

        // Drop the server end of the SME stream, so it causes an error
        drop(test_values.sme_req_stream);

        // Ensure the state machine exits
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Ready(()));
    }

    fn fake_successful_connect_result() -> fidl_sme::ConnectResult {
        fidl_sme::ConnectResult {
            code: fidl_ieee80211::StatusCode::Success,
            is_credential_rejected: false,
            is_reconnect: false,
        }
    }
}
