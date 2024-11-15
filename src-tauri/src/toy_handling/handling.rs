use crate::config::OSCNetworking;
use crate::frontend::frontend_types::FeCoreEvent;
use crate::frontend::frontend_types::FeScanEvent;
use crate::frontend::frontend_types::FeToyEvent;
use crate::frontend::frontend_types::FeVCToy;
use crate::frontend::ToFrontend;
use crate::osc::logic::toy_input_routine;
use crate::toy_handling::toy_manager::ToyManager;
use crate::toy_handling::toyops::LevelTweaks;
use crate::toy_handling::toyops::ToyParameter;
use crate::toy_handling::toyops::VCFeatureType;
use crate::toy_handling::toyops::{VCToy, VCToyFeatures};
use crate::toy_handling::ToyPower;
use crate::toy_handling::ToySig;
use crate::vcore::core::ToyManagementEvent;
use crate::vcore::core::VibeCheckState;
use crate::{vcore::core::TmSig, vcore::core::ToyUpdate, vcore::core::VCError};
use buttplug::client::ButtplugClientDevice;
use buttplug::client::ButtplugClientEvent;
use buttplug::client::RotateCommand::RotateMap;
use buttplug::client::ScalarCommand::ScalarMap;
use buttplug::core::message::ActuatorType;
use futures::StreamExt;
use futures_timer::Delay;
use log::debug;
use log::{error as logerr, info, trace, warn};
use parking_lot::Mutex;
use rosc::OscMessage;
use rosc::OscType;
use std::collections::HashMap;
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use std::time::Instant;
use tauri::api::notification::Notification;
use tauri::AppHandle;
use tauri::Manager;
use tokio::runtime::Runtime;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::{
    self,
    broadcast::{Receiver as BReceiver, Sender as BSender},
};
use tokio::task::JoinHandle;
use std::sync::atomic::{AtomicU64, Ordering};

use super::toyops::ProcessingMode;
use super::toyops::ProcessingModeValues;
use super::toyops::RateProcessingValues;
use super::ModeProcessorInput;
use super::ModeProcessorInputType;
use super::RateParser;
use super::SmoothParser;

pub struct ToyRateLimiter {
    last_update: AtomicU64,
    messages_per_second: AtomicU64,
}

impl ToyRateLimiter {
    pub fn new(messages_per_second: u64) -> Self {
        Self {
            last_update: AtomicU64::new(0),
            messages_per_second: AtomicU64::new(messages_per_second),
        }
    }

    pub fn update_rate(&self, messages_per_second: u64) {
        self.messages_per_second.store(messages_per_second, Ordering::Relaxed);
    }

    pub fn can_send(&self) -> bool {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        
        let last = self.last_update.load(Ordering::Relaxed);
        let mps = self.messages_per_second.load(Ordering::Relaxed);
        let interval_ms = 1000 / mps;

        if now - last >= interval_ms {
            self.last_update.store(now, Ordering::Relaxed);
            true
        } else {
            false
        }
    }
}

lazy_static::lazy_static! {
    pub static ref TOY_RATE_LIMITER: ToyRateLimiter = ToyRateLimiter::new(10);
}

/*
    This handler will handle the adding and removal of toys
    Needs Signals in and out to communicate with main thread
    - communicate errors and handler state (Errors to tell main thread its shutting down && State to receive shutdown from main thread) RECV/SEND
    - communicate toy events (add/remove) ONLY SEND?
*/
// Uses CEH send channel
pub async fn client_event_handler(
    mut event_stream: impl futures::Stream<Item = ButtplugClientEvent> + std::marker::Unpin,
    vibecheck_state_pointer: Arc<Mutex<VibeCheckState>>,
    identifier: String,
    app_handle: AppHandle,
    tme_send: UnboundedSender<ToyManagementEvent>,
    _error_tx: Sender<VCError>,
) {
    // Listen for toys and add them if it connects send add update
    // If a toy disconnects send remove update

    trace!("BP Client Event Handler Handling Events..");
    loop {
        if let Some(event) = event_stream.next().await {
            match event {
                ButtplugClientEvent::DeviceAdded(dev) => {
                    Delay::new(Duration::from_secs(3)).await;

                    // Can use this to differ between toys with batteries and toys without!
                    let toy_power = if dev.has_battery_level() {
                        match dev.battery_level().await {
                            Ok(battery_lvl) => ToyPower::Battery(battery_lvl),
                            Err(_e) => {
                                warn!("Device battery_level() error: {:?}", _e);
                                ToyPower::Pending
                            }
                        }
                    } else {
                        ToyPower::NoBattery
                    };

                    let sub_id = {
                        let vc_lock = vibecheck_state_pointer.lock();
                        let mut toy_dup_count = 0;
                        vc_lock
                            .core_toy_manager
                            .as_ref()
                            .unwrap()
                            .online_toys
                            .iter()
                            .for_each(|toy| {
                                if &toy.1.toy_name == dev.name() {
                                    toy_dup_count += 1;
                                }
                            });
                        toy_dup_count
                    };

                    // Load toy config for name of toy if it exists otherwise create the config for the toy name
                    let mut toy = VCToy {
                        toy_id: dev.index(),
                        toy_name: dev.name().clone(),
                        toy_power: toy_power.clone(),
                        toy_connected: dev.connected(),
                        toy_features: dev.message_attributes().clone(),
                        parsed_toy_features: VCToyFeatures::new(),
                        osc_data: false,
                        listening: false,
                        device_handle: dev.clone(),
                        config: None,
                        sub_id,
                    };

                    // Load config with toy name
                    match toy.load_toy_config() {
                        Ok(()) => info!("Toy config loaded successfully."),
                        Err(e) => warn!("Toy config failed to load: {:?}", e),
                    }

                    if toy.config.is_none() {
                        // First time toy load
                        toy.populate_toy_config();
                        let mut vc_lock = vibecheck_state_pointer.lock();
                        vc_lock
                            .core_toy_manager
                            .as_mut()
                            .unwrap()
                            .populate_configs();
                    } else {
                        toy.populate_toy_config();
                    }

                    {
                        let mut vc_lock = vibecheck_state_pointer.lock();
                        vc_lock
                            .core_toy_manager
                            .as_mut()
                            .unwrap()
                            .online_toys
                            .insert(toy.toy_id, toy.clone());
                    }
                    trace!("Toy inserted into VibeCheckState toys");

                    tme_send
                        .send(ToyManagementEvent::Tu(ToyUpdate::AddToy(toy.clone())))
                        .unwrap();

                    let _ = app_handle.emit_all(
                        "fe_toy_event",
                        FeToyEvent::Add({
                            FeVCToy {
                                toy_id: Some(toy.toy_id),
                                toy_name: toy.toy_name.clone(),
                                toy_anatomy: toy.config.as_ref().unwrap().anatomy.to_fe(),
                                toy_power,
                                toy_connected: toy.toy_connected,
                                features: toy.parsed_toy_features.features.to_frontend(),
                                listening: toy.listening,
                                osc_data: toy.osc_data,
                                sub_id: toy.sub_id,
                            }
                        }),
                    );

                    {
                        let vc_lock = vibecheck_state_pointer.lock();
                        if vc_lock.config.desktop_notifications {
                            let _ = Notification::new(identifier.clone())
                                .title("Toy Connected")
                                .body(
                                    format!("{} ({})", toy.toy_name, toy.toy_power.to_string())
                                        .as_str(),
                                )
                                .show();
                        }
                    }

                    info!("Toy Connected: {} | {}", toy.toy_name, toy.toy_id);
                }
                ButtplugClientEvent::DeviceRemoved(dev) => {
                    // Get scan on disconnect and toy
                    let (sod, toy) = {
                        let mut vc_lock = vibecheck_state_pointer.lock();
                        (
                            vc_lock.config.scan_on_disconnect,
                            vc_lock
                                .core_toy_manager
                                .as_mut()
                                .unwrap()
                                .online_toys
                                .remove(&dev.index()),
                        )
                    };

                    // Check if toy is valid
                    if let Some(toy) = toy {
                        trace!("Removed toy from VibeCheckState toys");
                        tme_send
                            .send(ToyManagementEvent::Tu(ToyUpdate::RemoveToy(dev.index())))
                            .unwrap();

                        let _ =
                            app_handle.emit_all("fe_toy_event", FeToyEvent::Remove(dev.index()));

                        {
                            let vc_lock = vibecheck_state_pointer.lock();
                            if vc_lock.config.desktop_notifications {
                                let _ = Notification::new(identifier.clone())
                                    .title("Toy Disconnected")
                                    .body(toy.toy_name.to_string())
                                    .show();
                            }
                        }

                        if sod {
                            info!("Scan on disconnect is enabled.. Starting scan.");
                            let vc_lock = vibecheck_state_pointer.lock();
                            if vc_lock.bp_client.is_some() && vc_lock.config.scan_on_disconnect {
                                vc_lock
                                    .async_rt
                                    .spawn(vc_lock.bp_client.as_ref().unwrap().start_scanning());
                            }
                            let _ = app_handle
                                .emit_all("fe_core_event", FeCoreEvent::Scan(FeScanEvent::Start));
                        }
                    }
                }
                ButtplugClientEvent::ScanningFinished => info!("Scanning finished!"),
                ButtplugClientEvent::ServerDisconnect => break,
                ButtplugClientEvent::PingTimeout => break,
                ButtplugClientEvent::Error(e) => {
                    logerr!("Client Event Error: {:?}", e);
                }
                ButtplugClientEvent::ServerConnect => {
                    info!("Server Connect");
                }
            }
        } else {
            warn!("GOT NONE IN EVENT HANDLER: THIS SHOULD NEVER HAPPEN LOL");
        }
    }
    info!("Event handler returning!");
}

// Parse scalar levels and logic for level tweaks
#[inline]
pub async fn scalar_parse_levels_send_toy_cmd(
    dev: &Arc<ButtplugClientDevice>,
    scalar_level: f64,
    feature_index: u32,
    actuator_type: ActuatorType,
    flip_float: bool,
    feature_levels: LevelTweaks,
) {
    let new_level = clamp_and_flip(scalar_level, flip_float, feature_levels);
    #[cfg(debug_assertions)]
    {
        let message_prefix = if scalar_level == 0.0 {
            "IDLE"
        } else {
            "SENDING"
        };
        info!(
            "{} FI[{}] AT[{}] SL[{}]",
            message_prefix, feature_index, actuator_type, new_level
        );
    }
    match dev
        .scalar(&ScalarMap(HashMap::from([(
            feature_index,
            (new_level, actuator_type),
        )])))
        .await
    {
        Ok(()) => {}
        Err(e) => {
            logerr!("Send scalar to device error: {}", e);
        }
    }
}

#[inline]
fn clamp_and_flip(value: f64, flip: bool, levels: LevelTweaks) -> f64 {
    let mut new_value;
    if value == 0.0 {
        new_value = levels.idle_level;
    } else {
        new_value = value.clamp(levels.minimum_level, levels.maximum_level);
    }
    if flip {
        new_value = flip_float64(new_value)
    }
    new_value
}

#[inline]
pub fn flip_float64(orig: f64) -> f64 {
    //1.00 - orig
    ((1.00 - orig) * 100.0).round() / 100.0
}

#[inline(always)]
fn parse_smoothing(
    smooth_queue: &mut Vec<f64>,
    feature_levels: LevelTweaks,
    mut float_level: f64,
    flip_float: bool,
) -> SmoothParser {
    debug!("!flip_float && *float_level == 0.0: [{}] || [{}] flip_float && *float_level == 1.0\nCOMBINED: [{}]", !flip_float && float_level == 0.0, flip_float && float_level == 1.0,
    smooth_queue.len() == feature_levels.smooth_rate as usize && (!flip_float && float_level == 0.0 || flip_float && float_level == 1.0)
);
    // Reached smooth rate maximum and not a 0 value
    if smooth_queue.len() == feature_levels.smooth_rate as usize {
        debug!("smooth_queue: {}", smooth_queue.len());
        if !flip_float && float_level == 0.0 || flip_float && float_level == 1.0 {
            // Don't return just set to 0
            debug!("float level is 0 but will be forgotten!");

            // Clear smooth queue bc restarting from 0
            smooth_queue.clear();
        } else {
            debug!("Setting float_level with smoothed float");
            // Get Mean of all numbers in smoothing rate and then round to hundredths and cast as f64
            float_level = (smooth_queue.iter().sum::<f64>() / smooth_queue.len() as f64 * 100.0)
                .round()
                / 100.0;
            smooth_queue.clear();

            smooth_queue.push(float_level);
            return SmoothParser::Smoothed(float_level);
        }

        // Have not reached smoothing maximum
    }

    // Maybe move this to be before queue is full check?
    if !flip_float && float_level == 0.0 || flip_float && float_level == 1.0 {
        debug!("Bypassing smoother: {:.5}", float_level);
        // let 0 through
        return SmoothParser::SkipZero(float_level);
    }

    debug!(
        "Adding float {} to smoothing.. queue size: {}",
        float_level,
        smooth_queue.len()
    );
    smooth_queue.push(float_level);
    // Continue receiving smooth floats
    SmoothParser::Smoothing
}

#[inline(always)]
fn parse_rate(
    processor: &mut RateProcessingValues,
    decrement_rate: f64,
    mut float_level: f64,
    flip_float: bool,
) -> RateParser {
    // Skip because got 0 value to stop toy.
    if !flip_float && float_level <= 0.0 || flip_float && float_level >= 1.0 {
        debug!("Bypassing rate input");
        processor.rate_saved_level = float_level;
        processor.rate_saved_osc_input = float_level;
        return RateParser::SkipZero;
    } else {
        // Increase toy level

        // Store new input then get the distance of the new input from the last input
        // Add that distance to the internal float level

        // get distance between newest input and last input
        // Set the distance between as the new motor speed
        if processor.rate_saved_osc_input > float_level {
            processor.rate_saved_level +=
                (processor.rate_saved_osc_input - float_level).clamp(0.00, 1.0);
        } else {
            processor.rate_saved_level +=
                (float_level - processor.rate_saved_osc_input).clamp(0.00, 1.0);
        }

        // Dont let internal level go over 1.0
        processor.rate_saved_level = processor.rate_saved_level.clamp(0.00, 1.00);

        // Set the newest input as the recent input
        processor.rate_saved_osc_input = float_level;

        // Set the internal rate state to the float level
        float_level = processor.rate_saved_level;

        // Save the internal motor speed
        //*rate_internal_level += *float_level;

        trace!("float level rate increased");
    }

    // Decrement testing
    if let Some(instant) = processor.rate_timestamp {
        // Decrease tick
        if instant.elapsed().as_secs_f64() >= 0.15 {
            // Decrease the internal rate level
            // This decrease rate should be tuneable
            processor.rate_saved_level =
                (processor.rate_saved_level - decrement_rate).clamp(0.00, 1.0);
            debug!(
                "internal level after decrement: {}",
                processor.rate_saved_level
            );

            // Set float level to decremented internal rate
            float_level = processor.rate_saved_level;

            trace!("decrease timer reset");
            return RateParser::RateCalculated(float_level, true);
        }
    }

    RateParser::RateCalculated(float_level, false)
}

async fn mode_processor<'toy_parameter>(
    input: ModeProcessorInput<'_>,
    feature_levels: LevelTweaks,
    flip_input: bool,
) -> Option<f64> {
    // Parse if input is from an Input Processor or raw input

    match input {
        // Input is from an Input Processor
        ModeProcessorInput::InputProcessor((input_type, processing_mode_values)) => {
            match input_type {
                ModeProcessorInputType::Float(f_input) => {
                    // Input Processor & Float
                    mode_processor_logic(
                        ModeProcessorInputType::Float(f_input),
                        processing_mode_values,
                        feature_levels,
                        flip_input,
                    )
                    .await
                }
                ModeProcessorInputType::Boolean(b_input) => {
                    // Input Processor & Boolean
                    mode_processor_logic(
                        ModeProcessorInputType::Boolean(b_input),
                        processing_mode_values,
                        feature_levels,
                        flip_input,
                    )
                    .await
                } // Input Processor & Boolean
            }
        }
        // Input is from parameter parsing
        ModeProcessorInput::RawInput(input_type, toy_parameter) => {
            match input_type {
                ModeProcessorInputType::Float(f_input) => {
                    // Raw Input & Float
                    mode_processor_logic(
                        ModeProcessorInputType::Float(f_input),
                        &mut toy_parameter.processing_mode_values,
                        feature_levels,
                        flip_input,
                    )
                    .await
                }
                ModeProcessorInputType::Boolean(b_input) => {
                    // Raw Input & Boolean
                    mode_processor_logic(
                        ModeProcessorInputType::Boolean(b_input),
                        &mut toy_parameter.processing_mode_values,
                        feature_levels,
                        flip_input,
                    )
                    .await
                } // Raw Input & Boolean
            }
        }
    }
}

async fn mode_processor_logic(
    input: ModeProcessorInputType,
    processor: &mut ProcessingModeValues,
    feature_levels: LevelTweaks,
    flip_input: bool,
) -> Option<f64> {
    // Process logic for each mode processing type
    match processor {
        // Raw Mode Handling
        // Raw = mode processing so just return the original value
        ProcessingModeValues::Raw => match input {
            ModeProcessorInputType::Float(float_level) => Some(float_level),
            ModeProcessorInputType::Boolean(b) => {
                if b {
                    // True == 1.0
                    Some(1.0)
                } else {
                    //False == 0.0
                    Some(0.0)
                }
            }
        },
        // Smoothing Mode Handling
        // Smooth = do smoothing logic with input and processor
        ProcessingModeValues::Smooth(values) => {
            //trace!("parse_moothing()");

            match input {
                ModeProcessorInputType::Float(float_level) => {
                    match parse_smoothing(
                        &mut values.smooth_queue,
                        feature_levels,
                        float_level,
                        flip_input,
                    ) {
                        // If smooth parser calculates a smooth value or the input is 0 return it
                        SmoothParser::SkipZero(f_out) | SmoothParser::Smoothed(f_out) => {
                            Some(f_out)
                        }
                        // None so that we don't send the value to the device
                        // None because smoother is still smoothing
                        SmoothParser::Smoothing => None,
                    }
                }
                ModeProcessorInputType::Boolean(_b) => None, // No support for Smoothing mode and Boolean
            }
            // Return processed input
        }
        // Rate Mode Handling
        ProcessingModeValues::Rate(values) => {
            //trace!("parse_rate()");
            // Need to set rate_timestamp when feature enabled
            if values.rate_timestamp.is_none() {
                values.rate_timestamp = Some(Instant::now());
            }

            match input {
                ModeProcessorInputType::Float(float_level) => {
                    match parse_rate(values, feature_levels.rate_tune, float_level, flip_input) {
                        RateParser::SkipZero => Some(0.), // Skip zero and send to toy
                        RateParser::RateCalculated(f_out, reset_timer) => {
                            // Rate calculated reset timer and send calculated value to toy
                            if reset_timer {
                                values.rate_timestamp = Some(Instant::now())
                            }
                            Some(f_out)
                        }
                    }
                }
                ModeProcessorInputType::Boolean(_b) => None, // No support for Rate and Boolean
            }
        }
        // Constant Mode Handling
        ProcessingModeValues::Constant => match input {
            ModeProcessorInputType::Float(float_level) => {
                if float_level >= 0.5 {
                    Some(feature_levels.constant_level)
                } else {
                    Some(0.0)
                }
            }
            ModeProcessorInputType::Boolean(b) => {
                if b {
                    Some(feature_levels.constant_level)
                } else {
                    Some(0.0)
                }
            }
        },
    }
}

/*
    This handler will send and receive updates to toys
    - communicate ToyUpdate to and from main thread SEND/RECV (Toys will be indexed on the main thread) (Connects and disconnect toy updates are handled by client event handler)
        + Keep a thread count of connected toys. Add/remove as recvs ToyUpdates from main thread
        + Send toy updates like (battery updates)
*/
// Uses TME send and recv channel

pub async fn toy_management_handler(
    tme_send: UnboundedSender<ToyManagementEvent>,
    mut tme_recv: UnboundedReceiver<ToyManagementEvent>,
    mut core_toy_manager: ToyManager,
    mut vc_config: OSCNetworking,
    app_handle: AppHandle,
) {
    let f = |dev: Arc<ButtplugClientDevice>,
             mut toy_bcst_rx: BReceiver<ToySig>,
             mut vc_toy_features: VCToyFeatures| {
        // Read toy config here?
        async move {
            // Put smooth_queue here
            // Put rate tracking here
            // Time tracking here?
            // Async runtime wrapped in Option for rate updating here????

            // Lock this to a user-set HZ value
            while dev.connected() {
                let Ok(ts) = toy_bcst_rx.recv().await else {
                    continue;
                };
                match ts {
                    ToySig::OSCMsg(mut msg) => {
                        parse_osc_message(&mut msg, dev.clone(), &mut vc_toy_features).await
                    }
                    ToySig::UpdateToy(toy) => update_toy(toy, dev.clone(), &mut vc_toy_features),
                }
            }
            info!(
                "Device {} disconnected! Leaving listening routine!",
                dev.index()
            );
        }
    }; // Toy listening routine

    let mut listening = false;

    // Management loop
    loop {
        // Recv event (not listening)
        if let Some(event) = tme_recv.recv().await {
            match event {
                // Handle Toy Update Signals
                ToyManagementEvent::Tu(tu) => match tu {
                    ToyUpdate::AddToy(toy) => {
                        core_toy_manager.online_toys.insert(toy.toy_id, toy);
                    }
                    ToyUpdate::RemoveToy(id) => {
                        core_toy_manager.online_toys.remove(&id);
                    }
                    ToyUpdate::AlterToy(toy) => {
                        core_toy_manager.online_toys.insert(toy.toy_id, toy);
                    }
                },
                // Handle Management Signals
                ToyManagementEvent::Sig(tm_sig) => {
                    match tm_sig {
                        TmSig::StartListening(osc_net) => {
                            vc_config = osc_net;
                            listening = true;
                        }
                        TmSig::StopListening => {
                            // Already not listening
                            info!("StopListening but not listening");
                        }
                        TmSig::TMHReset => {
                            info!("TMHReset but not listening");
                        }
                        _ => {}
                    }
                }
            } // Event handled
        }

        if !listening {
            continue;
        }

        // This is a nested runtime maybe remove
        // Would need to pass toy thread handles to VibeCheckState
        let toy_async_rt = Runtime::new().unwrap();
        info!("Started listening!");
        // Recv events (listening)
        // Create toy bcst channel

        // Toy threads
        let mut running_toy_ths: HashMap<u32, JoinHandle<()>> = HashMap::new();

        // Broadcast channels for toy commands
        let (toy_bcst_tx, _toy_bcst_rx): (BSender<ToySig>, BReceiver<ToySig>) =
            sync::broadcast::channel(1024);

        // Create toy threads
        for toy in &core_toy_manager.online_toys {
            let f_run = f(
                toy.1.device_handle.clone(),
                toy_bcst_tx.subscribe(),
                toy.1.parsed_toy_features.clone(),
            );
            running_toy_ths.insert(
                *toy.0,
                toy_async_rt.spawn(async move {
                    f_run.await;
                }),
            );
            info!("Toy: {} started listening..", *toy.0);
        }

        // Create OSC listener thread
        let toy_bcst_tx_osc = toy_bcst_tx.clone();
        info!("Spawning OSC listener..");
        let vc_conf_clone = vc_config.clone();
        let tme_send_clone = tme_send.clone();
        let app_handle_clone = app_handle.clone();
        thread::spawn(move || {
            toy_input_routine(
                toy_bcst_tx_osc,
                tme_send_clone,
                app_handle_clone,
                vc_conf_clone,
            )
        });

        loop {
            // Recv event (listening)
            let event = tme_recv.recv().await;
            let Some(event) = event else { continue };
            match event {
                // Handle Toy Update Signals
                ToyManagementEvent::Tu(tu) => {
                    match tu {
                        ToyUpdate::AddToy(toy) => {
                            core_toy_manager.online_toys.insert(toy.toy_id, toy.clone());
                            let f_run = f(
                                toy.device_handle,
                                toy_bcst_tx.subscribe(),
                                toy.parsed_toy_features.clone(),
                            );
                            running_toy_ths.insert(
                                toy.toy_id,
                                toy_async_rt.spawn(async move {
                                    f_run.await;
                                }),
                            );
                            info!("Toy: {} started listening..", toy.toy_id);
                        }
                        ToyUpdate::RemoveToy(id) => {
                            // OSC Listener thread will only die on StopListening event
                            if let Some(toy) = running_toy_ths.remove(&id) {
                                toy.abort();
                                match toy.await {
                                    Ok(()) => info!("Toy {} thread finished", id),
                                    Err(e) => {
                                        warn!("Toy {} thread failed to reach completion: {}", id, e)
                                    }
                                }
                                info!("[TOY ID: {}] Stopped listening. (ToyUpdate::RemoveToy)", id);
                                running_toy_ths.remove(&id);
                                core_toy_manager.online_toys.remove(&id);
                            }
                        }
                        ToyUpdate::AlterToy(toy) => {
                            match toy_bcst_tx
                                .send(ToySig::UpdateToy(ToyUpdate::AlterToy(toy.clone())))
                            {
                                Ok(receivers) => {
                                    info!("Sent ToyUpdate broadcast to {} toys", receivers - 1)
                                }
                                Err(e) => {
                                    logerr!("Failed to send UpdateToy: {}", e)
                                }
                            }
                            core_toy_manager.online_toys.insert(toy.toy_id, toy);
                        }
                    }
                }
                // Handle Management Signals
                ToyManagementEvent::Sig(tm_sig) => {
                    match tm_sig {
                        TmSig::StartListening(osc_net) => {
                            vc_config = osc_net;
                            // Already listening
                        }
                        TmSig::StopListening => {
                            // Stop listening on every device and clean running thread hashmap

                            for toy in &mut running_toy_ths {
                                toy.1.abort();
                                match toy.1.await {
                                    Ok(()) => {
                                        info!("Toy {} thread finished", toy.0)
                                    }
                                    Err(e) => warn!(
                                        "Toy {} thread failed to reach completion: {}",
                                        toy.0, e
                                    ),
                                }
                                info!("[TOY ID: {}] Stopped listening. (TMSIG)", toy.0);
                            }
                            running_toy_ths.clear();
                            drop(_toy_bcst_rx); // Causes OSC listener to die
                            toy_async_rt.shutdown_background();
                            listening = false;
                            info!("Toys: {}", core_toy_manager.online_toys.len());
                            break; //Stop Listening
                        }
                        TmSig::TMHReset => {
                            // Stop listening on every device and clean running thread hashmap
                            info!("TMHReset");

                            for toy in &mut running_toy_ths {
                                toy.1.abort();
                                match toy.1.await {
                                    Ok(()) => {
                                        info!("Toy {} thread finished", toy.0)
                                    }
                                    Err(e) => warn!(
                                        "Toy {} thread failed to reach completion: {}",
                                        toy.0, e
                                    ),
                                }
                                info!("[TOY ID: {}] Stopped listening. (TMSIG)", toy.0);
                            }
                            running_toy_ths.clear();
                            drop(_toy_bcst_rx); // Causes OSC listener to die
                            toy_async_rt.shutdown_background();
                            listening = false;
                            info!("Toys: {}", core_toy_manager.online_toys.len());
                            break; //Stop Listening
                        }
                        _ => {}
                    }
                } // Event handled
            }
        }
    } // Management loop
}

#[inline(always)]
async fn parse_osc_message(
    msg: &mut OscMessage,
    dev: Arc<ButtplugClientDevice>,
    vc_toy_features: &mut VCToyFeatures,
) {
    // Parse OSC msgs to toys commands
    //debug!("msg.addr = {} | msg.args = {:?}", msg.addr, msg.args);
    /*
     * Do Penetration System parsing first?
     * Then parameter parsing?
     * Mode processor is a function now so it can be used in both!
     */

    // msg args pop should go here
    //let newest_msg_addr = msg.addr.clone();
    let newest_msg_val = msg.args.pop().unwrap();

    /*
     * Input mode processing
     * Get all features with an enabled Input mode
     * For each feature with a penetration system do processing for the current OSC input
     * If get a value from processing check if the Input mode has a processing mode associated
     *
     */

    // Get all features with an enabled input processor?
    if let Some(input_processor_system_features) =
        vc_toy_features.get_features_with_input_processors(&msg.addr)
    {
        match newest_msg_val {
            OscType::Float(lvl) => {
                for feature in input_processor_system_features {
                    let float_level = ((lvl * 100.0).round() / 100.0) as f64;
                    // pen_system is checked for None in get_features_with_penetration_systems method.
                    // Give access to internal mode values here (input, internal_values)
                    if let Some(i_mode_processed_value) = feature
                        .penetration_system
                        .pen_system
                        .as_mut()
                        .unwrap()
                        .process(
                            msg.addr.as_str(),
                            ModeProcessorInputType::Float(float_level),
                        )
                    {
                        // Send to mode processor if specified (Raw = no mode processing)
                        if let ProcessingMode::Raw =
                            feature.penetration_system.pen_system_processing_mode
                        {
                            command_toy(
                                dev.clone(),
                                feature.feature_type,
                                i_mode_processed_value,
                                feature.feature_index,
                                feature.flip_input_float,
                                feature.feature_levels,
                            )
                            .await;
                        } else {
                            // If mode processor returns a value send to toy
                            if let Some(i) = mode_processor(
                                ModeProcessorInput::InputProcessor((
                                    ModeProcessorInputType::Float(i_mode_processed_value),
                                    &mut feature
                                        .penetration_system
                                        .pen_system_processing_mode_values,
                                )),
                                feature.feature_levels,
                                feature.flip_input_float,
                            )
                            .await
                            {
                                command_toy(
                                    dev.clone(),
                                    feature.feature_type,
                                    i,
                                    feature.feature_index,
                                    feature.flip_input_float,
                                    feature.feature_levels,
                                )
                                .await;
                            }
                        }
                    }
                }
            }
            // Boolean can be supported in the process trait method
            OscType::Bool(b) => {
                for feature in input_processor_system_features {
                    // Boolean to float transformation here
                    if let Some(i_mode_processed_value) = feature
                        .penetration_system
                        .pen_system
                        .as_mut()
                        .unwrap()
                        .process(msg.addr.as_str(), ModeProcessorInputType::Boolean(b))
                    {
                        // Send to mode processor if specified (Raw = no mode processing)
                        if let ProcessingMode::Raw =
                            feature.penetration_system.pen_system_processing_mode
                        {
                            command_toy(
                                dev.clone(),
                                feature.feature_type,
                                i_mode_processed_value,
                                feature.feature_index,
                                feature.flip_input_float,
                                feature.feature_levels,
                            )
                            .await;
                        } else if let Some(i) = mode_processor(
                            ModeProcessorInput::InputProcessor((
                                ModeProcessorInputType::Float(i_mode_processed_value),
                                &mut feature.penetration_system.pen_system_processing_mode_values,
                            )),
                            feature.feature_levels,
                            feature.flip_input_float,
                        )
                        .await
                        {
                            command_toy(
                                dev.clone(),
                                feature.feature_type,
                                i,
                                feature.feature_index,
                                feature.flip_input_float,
                                feature.feature_levels,
                            )
                            .await;
                        }
                    }
                }
            }
            _ => (),
        } // End match OscType for Input processors
    } // End Input processing

    if let Some(features) = vc_toy_features.get_features_from_param(&msg.addr) {
        match newest_msg_val {
            OscType::Float(lvl) => {
                // Clamp float accuracy to hundredths and cast as 64 bit float
                let float_level = ((lvl * 100.0).round() / 100.0) as f64;
                //debug!("Received and cast float lvl: {:.5}", float_level);

                for feature in features {
                    // Get ToyParameter here
                    // We unwrap here because the call to get_features_from_param guarantees the parameter exists.
                    let mut toy_parameter = feature
                        .osc_parameters
                        .iter_mut()
                        .filter_map(|param| {
                            if param.parameter == msg.addr {
                                Some(param)
                            } else {
                                None
                            }
                        })
                        .collect::<Vec<&mut ToyParameter>>();

                    if let Some(first_toy_param) = toy_parameter.first_mut() {
                        if let Some(mode_processed_value) = mode_processor(
                            ModeProcessorInput::RawInput(
                                ModeProcessorInputType::Float(float_level),
                                first_toy_param,
                            ),
                            feature.feature_levels,
                            feature.flip_input_float,
                        )
                        .await
                        {
                            command_toy(
                                dev.clone(),
                                feature.feature_type,
                                mode_processed_value,
                                feature.feature_index,
                                feature.flip_input_float,
                                feature.feature_levels,
                            )
                            .await;
                        }
                    } // If no matching toy parameter skip feature
                }
            }
            OscType::Bool(b) => {
                info!("Got a Bool! {} = {}", msg.addr, b);
                for feature in features {
                    // Get ToyParameter here
                    let mut toy_parameter = feature
                        .osc_parameters
                        .iter_mut()
                        .filter_map(|param| {
                            if param.parameter == msg.addr {
                                Some(param)
                            } else {
                                None
                            }
                        })
                        .collect::<Vec<&mut ToyParameter>>();

                    if let Some(first_toy_param) = toy_parameter.first_mut() {
                        if let Some(i) = mode_processor(
                            ModeProcessorInput::RawInput(
                                ModeProcessorInputType::Boolean(b),
                                first_toy_param,
                            ),
                            feature.feature_levels,
                            feature.flip_input_float,
                        )
                        .await
                        {
                            command_toy(
                                dev.clone(),
                                feature.feature_type,
                                i,
                                feature.feature_index,
                                feature.flip_input_float,
                                feature.feature_levels,
                            )
                            .await;
                        }
                    }
                }
            }
            _ => {} // Skip parameter because unsuppported OSC type
        }
    }
}

#[inline(always)]
fn update_toy(toy: ToyUpdate, dev: Arc<ButtplugClientDevice>, vc_toy_features: &mut VCToyFeatures) {
    let ToyUpdate::AlterToy(new_toy) = toy else {
        return;
    };
    if new_toy.toy_id != dev.index() {
        return;
    }
    *vc_toy_features = new_toy.parsed_toy_features;
    info!("Altered toy: {}", new_toy.toy_id);
}

/*
 * Sends commands to toys
 */
pub async fn command_toy(
    dev: Arc<ButtplugClientDevice>,
    feature_type: VCFeatureType,
    float_level: f64,
    feature_index: u32,
    flip_float: bool,
    feature_levels: LevelTweaks,
) {
    if !TOY_RATE_LIMITER.can_send() {
        trace!("Rate limited, skipping command");
        return;
    }

    match feature_type {
        VCFeatureType::Vibrator => {
            scalar_parse_levels_send_toy_cmd(
                &dev,
                float_level,
                feature_index,
                ActuatorType::Vibrate,
                flip_float,
                feature_levels,
            )
            .await;
        }
        // We handle Rotator differently because it is not included in the Scalar feature set
        VCFeatureType::Rotator => {
            let new_level = clamp_and_flip(float_level, flip_float, feature_levels);
            let _ = dev
                .rotate(&RotateMap(HashMap::from([(
                    feature_index,
                    (new_level, true),
                )])))
                .await;
        }
        VCFeatureType::Constrict => {
            scalar_parse_levels_send_toy_cmd(
                &dev,
                float_level,
                feature_index,
                ActuatorType::Constrict,
                flip_float,
                feature_levels,
            )
            .await;
        }
        VCFeatureType::Oscillate => {
            scalar_parse_levels_send_toy_cmd(
                &dev,
                float_level,
                feature_index,
                ActuatorType::Oscillate,
                flip_float,
                feature_levels,
            )
            .await;
        }
        VCFeatureType::Position => {
            scalar_parse_levels_send_toy_cmd(
                &dev,
                float_level,
                feature_index,
                ActuatorType::Position,
                flip_float,
                feature_levels,
            )
            .await;
        }
        VCFeatureType::Inflate => {
            scalar_parse_levels_send_toy_cmd(
                &dev,
                float_level,
                feature_index,
                ActuatorType::Inflate,
                flip_float,
                feature_levels,
            )
            .await;
        }
        // We handle Linear differently because it is not included in the Scalar feature set
        VCFeatureType::Linear => {
            let new_level = clamp_and_flip(float_level, flip_float, feature_levels);
            let _ = dev
                .linear(&buttplug::client::LinearCommand::LinearMap(HashMap::from(
                    [(
                        feature_index,
                        (feature_levels.linear_position_speed, new_level),
                    )],
                )))
                .await;
        }
        VCFeatureType::ScalarRotator => {
            scalar_parse_levels_send_toy_cmd(
                &dev,
                float_level,
                feature_index,
                ActuatorType::Rotate,
                flip_float,
                feature_levels,
            )
            .await;
        }
    }
}
