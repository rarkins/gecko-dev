// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![deny(missing_docs)]

//! Glean is a modern approach for recording and sending Telemetry data.
//!
//! It's in use at Mozilla.
//!
//! All documentation can be found online:
//!
//! ## [The Glean SDK Book](https://mozilla.github.io/glean)
//!
//! ## Example
//!
//! Initialize Glean, register a ping and then send it.
//!
//! ```rust,no_run
//! # use glean::{Configuration, ClientInfoMetrics, Error, private::*};
//! let cfg = Configuration {
//!     data_path: "/tmp/data".into(),
//!     application_id: "org.mozilla.glean_core.example".into(),
//!     upload_enabled: true,
//!     max_events: None,
//!     delay_ping_lifetime_io: false,
//!     channel: None,
//!     server_endpoint: None,
//!     uploader: None,
//! };
//! glean::initialize(cfg, ClientInfoMetrics::unknown());
//!
//! let prototype_ping = PingType::new("prototype", true, true, vec!());
//!
//! glean::register_ping_type(&prototype_ping);
//!
//! prototype_ping.submit(None);
//! ```

use once_cell::sync::OnceCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

pub use configuration::Configuration;
use configuration::DEFAULT_GLEAN_ENDPOINT;
pub use core_metrics::ClientInfoMetrics;
pub use glean_core::{global_glean, setup_glean, CommonMetricData, Error, Glean, Lifetime, Result};
use private::RecordedExperimentData;

mod configuration;
mod core_metrics;
mod dispatcher;
mod glean_metrics;
pub mod net;
pub mod private;
mod system;

const LANGUAGE_BINDING_NAME: &str = "Rust";

/// State to keep track for the Rust Language bindings.
///
/// This is useful for setting Glean SDK-owned metrics when
/// the state of the upload is toggled.
#[derive(Debug)]
struct RustBindingsState {
    /// The channel the application is being distributed on.
    channel: Option<String>,

    /// Client info metrics set by the application.
    client_info: ClientInfoMetrics,
}

/// Set when `glean::initialize()` returns.
/// This allows to detect calls that happen before `glean::initialize()` was called.
/// Note: The initialization might still be in progress, as it runs in a separate thread.
static INITIALIZE_CALLED: AtomicBool = AtomicBool::new(false);

/// A global singleton storing additional state for Glean.
///
/// Requires a Mutex, because in tests we can actual reset this.
static STATE: OnceCell<Mutex<RustBindingsState>> = OnceCell::new();

/// Get a reference to the global state object.
///
/// Panics if no global state object was set.
fn global_state() -> &'static Mutex<RustBindingsState> {
    STATE.get().unwrap()
}

/// Set or replace the global bindings State object.
fn setup_state(state: RustBindingsState) {
    // The `OnceCell` type wrapping our state is thread-safe and can only be set once.
    // Therefore even if our check for it being empty succeeds, setting it could fail if a
    // concurrent thread is quicker in setting it.
    // However this will not cause a bigger problem, as the second `set` operation will just fail.
    // We can log it and move on.
    //
    // For all wrappers this is not a problem, as the State object is intialized exactly once on
    // calling `initialize` on the global singleton and further operations check that it has been
    // initialized.
    if STATE.get().is_none() {
        if STATE.set(Mutex::new(state)).is_err() {
            log::error!(
                "Global Glean state object is initialized already. This probably happened concurrently."
            );
        }
    } else {
        // We allow overriding the global State object to support test mode.
        // In test mode the State object is fully destroyed and recreated.
        // This all happens behind a mutex and is therefore also thread-safe.
        let mut lock = STATE.get().unwrap().lock().unwrap();
        *lock = state;
    }
}

/// An instance of the upload manager.
///
/// Requires a Mutex, because in tests we can actual reset this.
static UPLOAD_MANAGER: OnceCell<Mutex<net::UploadManager>> = OnceCell::new();

/// Get a reference to the global upload manager.
///
/// Panics if no global state object was set.
fn get_upload_manager() -> &'static Mutex<net::UploadManager> {
    UPLOAD_MANAGER.get().unwrap()
}

/// Set or replace the global upload object.
fn setup_upload_manager(upload_manager: net::UploadManager) {
    if UPLOAD_MANAGER.get().is_none() {
        if UPLOAD_MANAGER.set(Mutex::new(upload_manager)).is_err() {
            log::error!(
                "Global upload state object is initialized already. This probably happened concurrently."
            );
        }
    } else {
        let mut lock = UPLOAD_MANAGER.get().unwrap().lock().unwrap();
        *lock = upload_manager;
    }
}

fn with_glean<F, R>(f: F) -> R
where
    F: FnOnce(&Glean) -> R,
{
    let glean = global_glean().expect("Global Glean object not initialized");
    let lock = glean.lock().unwrap();
    f(&lock)
}

fn with_glean_mut<F, R>(f: F) -> R
where
    F: FnOnce(&mut Glean) -> R,
{
    let glean = global_glean().expect("Global Glean object not initialized");
    let mut lock = glean.lock().unwrap();
    f(&mut lock)
}

/// Creates and initializes a new Glean object.
///
/// See `glean_core::Glean::new` for more information.
///
/// # Arguments
///
/// * `cfg` - the `Configuration` options to initialize with.
/// * `client_info` - the `ClientInfoMetrics` values used to set Glean
///   core metrics.
pub fn initialize(cfg: Configuration, client_info: ClientInfoMetrics) {
    if was_initialize_called() {
        log::error!("Glean should not be initialized multiple times");
        return;
    }

    std::thread::spawn(move || {
        let core_cfg = glean_core::Configuration {
            upload_enabled: cfg.upload_enabled,
            data_path: cfg.data_path.clone(),
            application_id: cfg.application_id.clone(),
            language_binding_name: LANGUAGE_BINDING_NAME.into(),
            max_events: cfg.max_events,
            delay_ping_lifetime_io: cfg.delay_ping_lifetime_io,
        };

        let glean = match Glean::new(core_cfg) {
            Ok(glean) => glean,
            Err(err) => {
                log::error!("Failed to initialize Glean: {}", err);
                return;
            }
        };

        // glean-core already takes care of logging errors: other bindings
        // simply do early returns, as we're doing.
        if glean_core::setup_glean(glean).is_err() {
            return;
        }

        log::info!("Glean initialized");

        // Now make this the global object available to others.
        setup_state(RustBindingsState {
            channel: cfg.channel,
            client_info,
        });

        // Initialize the ping uploader.
        setup_upload_manager(net::UploadManager::new(
            cfg.server_endpoint
                .unwrap_or_else(|| DEFAULT_GLEAN_ENDPOINT.to_string()),
            cfg.uploader
                .unwrap_or_else(|| Box::new(net::HttpUploader) as Box<dyn net::PingUploader>),
        ));

        let upload_enabled = cfg.upload_enabled;

        with_glean_mut(|glean| {
            let state = global_state().lock().unwrap();

            // Get the current value of the dirty flag so we know whether to
            // send a dirty startup baseline ping below.  Immediately set it to
            // `false` so that dirty startup pings won't be sent if Glean
            // initialization does not complete successfully.
            // TODO Bug 1672956 will decide where to set this flag again.
            let dirty_flag = glean.is_dirty_flag_set();
            glean.set_dirty_flag(false);

            // Register builtin pings.
            // Unfortunately we need to manually list them here to guarantee
            // they are registered synchronously before we need them.
            // We don't need to handle the deletion-request ping. It's never touched
            // from the language implementation.
            glean.register_ping_type(&glean_metrics::pings::baseline.ping_type);
            glean.register_ping_type(&glean_metrics::pings::metrics.ping_type);
            glean.register_ping_type(&glean_metrics::pings::events.ping_type);

            // TODO: perform registration of pings that were attempted to be
            // registered before init. See bug 1673850.

            // If this is the first time ever the Glean SDK runs, make sure to set
            // some initial core metrics in case we need to generate early pings.
            // The next times we start, we would have them around already.
            let is_first_run = glean.is_first_run();
            if is_first_run {
                initialize_core_metrics(&glean, &state.client_info, state.channel.clone());
            }

            // Deal with any pending events so we can start recording new ones
            let pings_submitted = glean.on_ready_to_submit_pings();

            // We need to kick off upload in these cases:
            // 1. Pings were submitted through Glean and it is ready to upload those pings;
            // 2. Upload is disabled, to upload a possible deletion-request ping.
            if pings_submitted || !upload_enabled {
                let uploader = get_upload_manager().lock().unwrap();
                uploader.trigger_upload();
            }

            // Set up information and scheduling for Glean owned pings. Ideally, the "metrics"
            // ping startup check should be performed before any other ping, since it relies
            // on being dispatched to the API context before any other metric.
            // TODO: start the metrics ping scheduler, will happen in bug 1672951.

            // Check if the "dirty flag" is set. That means the product was probably
            // force-closed. If that's the case, submit a 'baseline' ping with the
            // reason "dirty_startup". We only do that from the second run.
            if !is_first_run && dirty_flag {
                // TODO: bug 1672956 - submit_ping_by_name_sync("baseline", "dirty_startup");
            }

            // From the second time we run, after all startup pings are generated,
            // make sure to clear `lifetime: application` metrics and set them again.
            // Any new value will be sent in newly generated pings after startup.
            if !is_first_run {
                glean.clear_application_lifetime_metrics();
                initialize_core_metrics(&glean, &state.client_info, state.channel.clone());
            }
        });

        // Signal Dispatcher that init is complete
        if let Err(err) = dispatcher::flush_init() {
            log::error!("Unable to flush the preinit queue: {}", err);
        }
    });

    // Mark the initialization as called: this needs to happen outside of the
    // dispatched block!
    INITIALIZE_CALLED.store(true, Ordering::SeqCst);
}

/// Checks if `glean::initialize` was ever called.
///
/// # Returns
///
/// `true` if it was, `false` otherwise.
fn was_initialize_called() -> bool {
    INITIALIZE_CALLED.load(Ordering::SeqCst)
}

fn initialize_core_metrics(
    glean: &Glean,
    client_info: &ClientInfoMetrics,
    channel: Option<String>,
) {
    let core_metrics = core_metrics::InternalMetrics::new();

    core_metrics
        .app_build
        .set(glean, &client_info.app_build[..]);
    core_metrics
        .app_display_version
        .set(glean, &client_info.app_display_version[..]);
    if let Some(app_channel) = channel {
        core_metrics.app_channel.set(glean, app_channel);
    }
    core_metrics.os_version.set(glean, "unknown".to_string());
    core_metrics
        .architecture
        .set(glean, system::ARCH.to_string());
    core_metrics
        .device_manufacturer
        .set(glean, "unknown".to_string());
    core_metrics.device_model.set(glean, "unknown".to_string());
}

/// Sets whether upload is enabled or not.
///
/// See `glean_core::Glean.set_upload_enabled`.
pub fn set_upload_enabled(enabled: bool) {
    if !was_initialize_called() {
        let msg =
            "Changing upload enabled before Glean is initialized is not supported.\n \
            Pass the correct state into `Glean.initialize()`.\n \
            See documentation at https://mozilla.github.io/glean/book/user/general-api.html#initializing-the-glean-sdk";
        log::error!("{}", msg);
        return;
    }

    // Changing upload enabled always happens asynchronous.
    // That way it follows what a user expect when calling it inbetween other calls:
    // it executes in the right order.
    //
    // Because the dispatch queue is halted until Glean is fully initialized
    // we can safely enqueue here and it will execute after initialization.
    dispatcher::launch(move || {
        with_glean_mut(|glean| {
            let state = global_state().lock().unwrap();
            let old_enabled = glean.is_upload_enabled();
            glean.set_upload_enabled(enabled);

            // TODO: Cancel upload and any outstanding metrics ping scheduler
            // task. Will happen on bug 1672951.

            if !old_enabled && enabled {
                // If uploading is being re-enabled, we have to restore the
                // application-lifetime metrics.
                initialize_core_metrics(&glean, &state.client_info, state.channel.clone());
            }

            if old_enabled && !enabled {
                // If uploading is disabled, we need to send the deletion-request ping:
                // note that glean-core takes care of generating it.
                let uploader = get_upload_manager().lock().unwrap();
                uploader.trigger_upload();
            }
        });
    });
}

/// Register a new [`PingType`](metrics/struct.PingType.html).
pub fn register_ping_type(ping: &private::PingType) {
    // If this happens after Glean.initialize is called (and returns),
    // we dispatch ping registration on the thread pool.
    // Registering a ping should not block the application.
    // Submission itself is also dispatched, so it will always come after the registration.
    if was_initialize_called() {
        let ping = ping.clone();
        dispatcher::launch(move || {
            with_glean_mut(|glean| {
                glean.register_ping_type(&ping.ping_type);
            })
        })
    }
}

/// Collects and submits a ping for eventual uploading.
///
/// See `glean_core::Glean.submit_ping`.
pub fn submit_ping(ping: &private::PingType, reason: Option<&str>) {
    submit_ping_by_name(&ping.name, reason)
}

/// Collects and submits a ping for eventual uploading by name.
///
/// See `glean_core::Glean.submit_ping_by_name`.
pub fn submit_ping_by_name(ping: &str, reason: Option<&str>) {
    let ping = ping.to_string();
    let reason = reason.map(|s| s.to_string());
    dispatcher::launch(move || {
        submit_ping_by_name_sync(&ping, reason.as_deref());
    })
}

/// Collect and submit a ping (by its name) for eventual upload, synchronously.
///
/// The ping will be looked up in the known instances of `private::PingType`. If the
/// ping isn't known, an error is logged and the ping isn't queued for uploading.
///
/// The ping content is assembled as soon as possible, but upload is not
/// guaranteed to happen immediately, as that depends on the upload
/// policies.
///
/// If the ping currently contains no content, it will not be assembled and
/// queued for sending, unless explicitly specified otherwise in the registry
/// file.
///
/// ## Arguments
///
/// * `ping_name` - the name of the ping to submit.
/// * `reason` - the reason the ping is being submitted.
pub(crate) fn submit_ping_by_name_sync(ping: &str, reason: Option<&str>) {
    if !was_initialize_called() {
        log::error!("Glean must be initialized before submitting pings.");
        return;
    }

    let submitted_ping = with_glean(|glean| {
        if !glean.is_upload_enabled() {
            log::info!("Glean disabled: not submitting any pings.");
            // This won't actually return from `submit_ping_by_name`, but
            // returning `false` here skips spinning up the uploader below,
            // which is basically the same.
            return Some(false);
        }

        glean.submit_ping_by_name(&ping, reason.as_deref()).ok()
    });

    if let Some(true) = submitted_ping {
        let uploader = get_upload_manager().lock().unwrap();
        uploader.trigger_upload();
    }
}

/// Indicate that an experiment is running.  Glean will then add an
/// experiment annotation to the environment which is sent with pings. This
/// infomration is not persisted between runs.
///
/// See [`glean_core::Glean::set_experiment_active`].
pub fn set_experiment_active(
    experiment_id: String,
    branch: String,
    extra: Option<HashMap<String, String>>,
) {
    dispatcher::launch(move || {
        with_glean(|glean| {
            glean.set_experiment_active(
                experiment_id.to_owned(),
                branch.to_owned(),
                extra.to_owned(),
            )
        });
    })
}

/// Indicate that an experiment is no longer running.
///
/// See [`glean_core::Glean::set_experiment_inactive`].
pub fn set_experiment_inactive(experiment_id: String) {
    dispatcher::launch(move || {
        with_glean(|glean| glean.set_experiment_inactive(experiment_id.to_owned()))
    })
}

/// TEST ONLY FUNCTION.
/// Checks if an experiment is currently active.
#[allow(dead_code)]
pub(crate) fn test_is_experiment_active(experiment_id: String) -> bool {
    dispatcher::block_on_queue();
    with_glean(|glean| glean.test_is_experiment_active(experiment_id.to_owned()))
}

/// TEST ONLY FUNCTION.
/// Returns the `RecordedExperimentData` for the given `experiment_id` or panics if
/// the id isn't found.
#[allow(dead_code)]
pub(crate) fn test_get_experiment_data(experiment_id: String) -> RecordedExperimentData {
    dispatcher::block_on_queue();
    with_glean(|glean| {
        let json_data = glean
            .test_get_experiment_data_as_json(experiment_id.to_owned())
            .unwrap_or_else(|| panic!("No experiment found for id: {}", experiment_id));
        serde_json::from_str::<RecordedExperimentData>(&json_data).unwrap()
    })
}

/// TEST ONLY FUNCTION.
/// Resets the Glean state and triggers init again.
#[cfg(test)]
#[allow(dead_code)]
pub(crate) fn reset_glean(cfg: Configuration, client_info: ClientInfoMetrics, clear_stores: bool) {
    // Destroy the existing glean instance from glean-core.
    if was_initialize_called() {
        // We need to check if the Glean object (from glean-core) is
        // initialized, otherwise this will crash on the first test
        // due to bug 1675215 (this check can be removed once that
        // bug is fixed).
        if global_glean().is_some() {
            with_glean_mut(|glean| {
                if clear_stores {
                    glean.test_clear_all_stores()
                }
                glean.destroy_db()
            });
        }
        // Allow us to go through initialization again.
        INITIALIZE_CALLED.store(false, Ordering::SeqCst);
        // Reset the dispatcher.
        dispatcher::reset_dispatcher();
    }

    // Always log pings for tests
    //Glean.setLogPings(true)
    initialize(cfg, client_info);
}

#[cfg(test)]
mod test;
