//
// Copyright (c) 2022 ZettaScale Technology
//
// This program and the accompanying materials are made available under the
// terms of the Eclipse Public License 2.0 which is available at
// http://www.eclipse.org/legal/epl-2.0, or the Apache License, Version 2.0
// which is available at https://www.apache.org/licenses/LICENSE-2.0.
//
// SPDX-License-Identifier: EPL-2.0 OR Apache-2.0
//
// Contributors:
//   ZettaScale Zenoh Team, <zenoh@zettascale.tech>
//
use std::{
    collections::HashMap,
    ffi::{CStr, CString},
    fmt,
    mem::MaybeUninit,
    slice,
    sync::Arc,
    time::Duration,
};

use cyclors::{
    qos::{History, HistoryKind, Qos},
    *,
};
use flume::Sender;
use serde::{Deserialize, Serialize, Serializer};
use tracing::{debug, error, warn};
use zenoh::{
    bytes::ZBytes,
    key_expr::{KeyExpr, OwnedKeyExpr},
    qos::CongestionControl,
    Session, Wait,
};

const MAX_SAMPLES: usize = 32;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) enum RouteStatus {
    Routed(OwnedKeyExpr), // Routing is active, with the zenoh key expression used for the route
    NotAllowed,           // Routing was not allowed per configuration
    CreationFailure(String), // The route creation failed
    _QoSConflict,         // A route was already established but with conflicting QoS
}

#[derive(Debug)]
pub(crate) struct TypeInfo {
    ptr: *mut dds_typeinfo_t,
}

impl TypeInfo {
    pub(crate) unsafe fn new(ptr: *const dds_typeinfo_t) -> TypeInfo {
        let ptr = ddsi_typeinfo_dup(ptr);
        TypeInfo { ptr }
    }
}

impl Drop for TypeInfo {
    fn drop(&mut self) {
        unsafe {
            ddsi_typeinfo_free(self.ptr);
        }
    }
}

unsafe impl Send for TypeInfo {}
unsafe impl Sync for TypeInfo {}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct DdsEntity {
    pub(crate) key: String,
    pub(crate) participant_key: String,
    pub(crate) topic_name: String,
    pub(crate) type_name: String,
    #[serde(skip)]
    pub(crate) type_info: Option<TypeInfo>,
    pub(crate) keyless: bool,
    pub(crate) qos: Qos,
    pub(crate) routes: HashMap<String, RouteStatus>, // map of routes statuses indexed by partition ("*" only if no partition)
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct DdsParticipant {
    pub(crate) key: String,
    pub(crate) qos: Qos,
}

#[derive(Debug)]
pub(crate) enum DiscoveryEvent {
    DiscoveredPublication { entity: DdsEntity },
    UndiscoveredPublication { key: String },
    DiscoveredSubscription { entity: DdsEntity },
    UndiscoveredSubscription { key: String },
    DiscoveredParticipant { entity: DdsParticipant },
    UndiscoveredParticipant { key: String },
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum DiscoveryType {
    Participant,
    Publication,
    Subscription,
}

impl fmt::Display for DiscoveryType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DiscoveryType::Participant => write!(f, "participant"),
            DiscoveryType::Publication => write!(f, "publication"),
            DiscoveryType::Subscription => write!(f, "subscription"),
        }
    }
}

pub(crate) struct DDSRawSample {
    sdref: *mut ddsi_serdata,
    data: ddsrt_iovec_t,
}

impl DDSRawSample {
    pub(crate) unsafe fn create(serdata: *const ddsi_serdata) -> Result<DDSRawSample, String> {
        let sdref: *mut ddsi_serdata;
        let mut data = ddsrt_iovec_t {
            iov_base: std::ptr::null_mut(),
            iov_len: 0,
        };

        if (*serdata).loan.is_null() {
            let size = ddsi_serdata_size(serdata);
            sdref = ddsi_serdata_to_ser_ref(serdata, 0, size as usize, &mut data);
        } else {
            let loan = (*serdata).loan;
            let metadata = (*loan).metadata;

            // Based on the current Cyclone DDS implementation loan should only contain RAW sample data at this point
            if (*metadata).sample_state == dds_loaned_sample_state_DDS_LOANED_SAMPLE_STATE_RAW_DATA
            {
                // Before forwarding to Zenoh the data first needs to be serialized
                if (*(*serdata).ops).from_sample.is_some() {
                    // We have the type information necessary to serialize so use from_sample()
                    let serialized_serdata = ddsi_serdata_from_sample(
                        (*serdata).type_,
                        (*serdata).kind,
                        (*loan).sample_ptr,
                    );

                    let size = ddsi_serdata_size(serialized_serdata);
                    sdref =
                        ddsi_serdata_to_ser_ref(serialized_serdata, 0, size as usize, &mut data);
                    ddsi_serdata_unref(serialized_serdata);
                } else {
                    // Type information not available so unable to serialize sample using from_sample()
                    return Err(String::from(
                        "Received sample from DDS contains a loan for which incomplete type information is held",
                    ));
                }
            } else {
                return Err(String::from(
                    "Received sample from DDS contains a loan with an unexpected sample state",
                ));
            }
        }

        Ok(DDSRawSample { sdref, data })
    }

    fn data_as_slice(&self) -> &[u8] {
        #[cfg(not(target_os = "windows"))]
        unsafe {
            slice::from_raw_parts(self.data.iov_base as *const u8, self.data.iov_len)
        }
        #[cfg(target_os = "windows")]
        unsafe {
            slice::from_raw_parts(self.data.iov_base as *const u8, self.data.iov_len as usize)
        }
    }

    pub(crate) fn payload_as_slice(&self) -> &[u8] {
        #[cfg(not(target_os = "windows"))]
        unsafe {
            &slice::from_raw_parts(self.data.iov_base as *const u8, self.data.iov_len)[4..]
        }
        #[cfg(target_os = "windows")]
        unsafe {
            &slice::from_raw_parts(self.data.iov_base as *const u8, self.data.iov_len as usize)[4..]
        }
    }

    pub(crate) fn hex_encode(&self) -> String {
        let mut encoded = String::new();
        let data_encoded = hex::encode(self.data_as_slice());
        encoded.push_str(data_encoded.as_str());
        encoded
    }
}

impl Drop for DDSRawSample {
    fn drop(&mut self) {
        unsafe {
            ddsi_serdata_to_ser_unref(self.sdref, &self.data);
        }
    }
}

impl fmt::Debug for DDSRawSample {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:02x?}", self.data_as_slice())
    }
}

impl From<DDSRawSample> for ZBytes {
    fn from(buf: DDSRawSample) -> Self {
        buf.data_as_slice().into()
    }
}

unsafe extern "C" fn on_data(dr: dds_entity_t, arg: *mut std::os::raw::c_void) {
    let btx = Box::from_raw(arg as *mut (DiscoveryType, Sender<DiscoveryEvent>));
    let discovery_type = btx.0;
    let sender = &btx.1;
    let dp = dds_get_participant(dr);
    let mut dpih: dds_instance_handle_t = 0;
    let _ = dds_get_instance_handle(dp, &mut dpih);

    #[allow(clippy::uninit_assumed_init)]
    let mut si = MaybeUninit::<[dds_sample_info_t; MAX_SAMPLES]>::uninit();
    let mut samples: [*mut ::std::os::raw::c_void; MAX_SAMPLES] =
        [std::ptr::null_mut(); MAX_SAMPLES];
    samples[0] = std::ptr::null_mut();

    let n = dds_take(
        dr,
        samples.as_mut_ptr(),
        si.as_mut_ptr() as *mut dds_sample_info_t,
        MAX_SAMPLES,
        MAX_SAMPLES as u32,
    );
    let si = si.assume_init();

    for i in 0..n {
        match discovery_type {
            DiscoveryType::Publication | DiscoveryType::Subscription => {
                let sample = samples[i as usize] as *mut dds_builtintopic_endpoint_t;
                if (*sample).participant_instance_handle == dpih {
                    // Ignore discovery of entities created by our own participant
                    continue;
                }
                let is_alive = si[i as usize].instance_state == dds_instance_state_DDS_IST_ALIVE;
                let key = hex::encode((*sample).key.v);

                if is_alive {
                    let topic_name = match CStr::from_ptr((*sample).topic_name).to_str() {
                        Ok(s) => s,
                        Err(e) => {
                            warn!("Discovery of an invalid topic name: {}", e);
                            continue;
                        }
                    };
                    if topic_name.starts_with("DCPS") {
                        debug!(
                            "Ignoring discovery of {} ({} is a builtin topic)",
                            key, topic_name
                        );
                        continue;
                    }

                    let type_name = match CStr::from_ptr((*sample).type_name).to_str() {
                        Ok(s) => s,
                        Err(e) => {
                            warn!("Discovery of an invalid topic type: {}", e);
                            continue;
                        }
                    };
                    let participant_key = hex::encode((*sample).participant_key.v);
                    let keyless = (*sample).key.v[15] == 3 || (*sample).key.v[15] == 4;

                    debug!(
                        "Discovered DDS {} {} from Participant {} on {} with type {} (keyless: {})",
                        discovery_type, key, participant_key, topic_name, type_name, keyless
                    );

                    let mut type_info: *const dds_typeinfo_t = std::ptr::null();
                    let ret = dds_builtintopic_get_endpoint_type_info(sample, &mut type_info);

                    let type_info = match ret {
                        0 => match type_info.is_null() {
                            false => Some(TypeInfo::new(type_info)),
                            true => {
                                debug!("Type information not available for type {}", type_name);
                                None
                            }
                        },
                        _ => {
                            warn!(
                                "Failed to lookup type information({})",
                                CStr::from_ptr(dds_strretcode(ret))
                                    .to_str()
                                    .unwrap_or("unrecoverable DDS retcode")
                            );
                            None
                        }
                    };

                    // send a DiscoveryEvent
                    let entity = DdsEntity {
                        key: key.clone(),
                        participant_key: participant_key.clone(),
                        topic_name: String::from(topic_name),
                        type_name: String::from(type_name),
                        keyless,
                        type_info,
                        qos: Qos::from_qos_native((*sample).qos),
                        routes: HashMap::<String, RouteStatus>::new(),
                    };

                    if let DiscoveryType::Publication = discovery_type {
                        send_discovery_event(
                            sender,
                            DiscoveryEvent::DiscoveredPublication { entity },
                        );
                    } else {
                        send_discovery_event(
                            sender,
                            DiscoveryEvent::DiscoveredSubscription { entity },
                        );
                    }
                } else if let DiscoveryType::Publication = discovery_type {
                    send_discovery_event(sender, DiscoveryEvent::UndiscoveredPublication { key });
                } else {
                    send_discovery_event(sender, DiscoveryEvent::UndiscoveredSubscription { key });
                }
            }
            DiscoveryType::Participant => {
                let sample = samples[i as usize] as *mut dds_builtintopic_participant_t;
                let is_alive = si[i as usize].instance_state == dds_instance_state_DDS_IST_ALIVE;
                let key = hex::encode((*sample).key.v);

                let mut guid = dds_builtintopic_guid { v: [0; 16] };
                let _ = dds_get_guid(dp, &mut guid);
                let guid = hex::encode(guid.v);

                if key == guid {
                    // Ignore discovery of entities created by our own participant
                    continue;
                }

                if is_alive {
                    debug!("Discovered DDS Participant {})", key,);

                    // Send a DiscoveryEvent
                    let entity = DdsParticipant {
                        key: key.clone(),
                        qos: Qos::from_qos_native((*sample).qos),
                    };

                    send_discovery_event(sender, DiscoveryEvent::DiscoveredParticipant { entity });
                } else {
                    send_discovery_event(sender, DiscoveryEvent::UndiscoveredParticipant { key });
                }
            }
        }
    }
    dds_return_loan(dr, samples.as_mut_ptr(), MAX_SAMPLES as i32);
    let _ = Box::into_raw(btx);
}

fn send_discovery_event(sender: &Sender<DiscoveryEvent>, event: DiscoveryEvent) {
    if let Err(e) = sender.try_send(event) {
        error!(
            "INTERNAL ERROR sending DiscoveryEvent to internal channel: {:?}",
            e
        );
    }
}

pub(crate) fn run_discovery(dp: dds_entity_t, tx: Sender<DiscoveryEvent>) {
    unsafe {
        let ptx = Box::new((DiscoveryType::Publication, tx.clone()));
        let stx = Box::new((DiscoveryType::Subscription, tx.clone()));
        let dptx = Box::new((DiscoveryType::Participant, tx));
        let sub_listener = dds_create_listener(Box::into_raw(ptx) as *mut std::os::raw::c_void);
        dds_lset_data_available(sub_listener, Some(on_data));

        let _pr = dds_create_reader(
            dp,
            DDS_BUILTIN_TOPIC_DCPSPUBLICATION,
            std::ptr::null(),
            sub_listener,
        );

        let sub_listener = dds_create_listener(Box::into_raw(stx) as *mut std::os::raw::c_void);
        dds_lset_data_available(sub_listener, Some(on_data));
        let _sr = dds_create_reader(
            dp,
            DDS_BUILTIN_TOPIC_DCPSSUBSCRIPTION,
            std::ptr::null(),
            sub_listener,
        );

        let sub_listener = dds_create_listener(Box::into_raw(dptx) as *mut std::os::raw::c_void);
        dds_lset_data_available(sub_listener, Some(on_data));
        let _dpr = dds_create_reader(
            dp,
            DDS_BUILTIN_TOPIC_DCPSPARTICIPANT,
            std::ptr::null(),
            sub_listener,
        );
    }
}

unsafe extern "C" fn data_forwarder_listener(dr: dds_entity_t, arg: *mut std::os::raw::c_void) {
    let pa = arg as *mut (String, KeyExpr, Arc<Session>, CongestionControl);
    let mut zp: *mut ddsi_serdata = std::ptr::null_mut();
    #[allow(clippy::uninit_assumed_init)]
    let mut si = MaybeUninit::<[dds_sample_info_t; 1]>::uninit();
    while dds_takecdr(
        dr,
        &mut zp,
        1,
        si.as_mut_ptr() as *mut dds_sample_info_t,
        DDS_ANY_STATE,
    ) > 0
    {
        let si = si.assume_init();
        if si[0].valid_data {
            let raw_sample = DDSRawSample::create(zp);

            match raw_sample {
                Ok(raw_sample) => {
                    if *crate::LOG_PAYLOAD {
                        tracing::trace!(
                            "Route data from DDS {} to zenoh key={} - payload: {:02x?}",
                            &(*pa).0,
                            &(*pa).1,
                            raw_sample
                        );
                    } else {
                        tracing::trace!(
                            "Route data from DDS {} to zenoh key={}",
                            &(*pa).0,
                            &(*pa).1
                        );
                    }
                    let _ = (*pa)
                        .2
                        .put(&(*pa).1, raw_sample)
                        .congestion_control((*pa).3)
                        .wait();
                }
                Err(error) => {
                    tracing::warn!(
                        "Failed to route data from DDS {} to zenoh key={} (msg: {})",
                        &(*pa).0,
                        &(*pa).1,
                        error
                    );
                }
            }
        }
        ddsi_serdata_unref(zp);
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn create_forwarding_dds_reader(
    dp: dds_entity_t,
    topic_name: String,
    type_name: String,
    type_info: &Option<TypeInfo>,
    keyless: bool,
    mut qos: Qos,
    z_key: KeyExpr,
    z: Arc<Session>,
    read_period: Option<Duration>,
    congestion_ctrl: CongestionControl,
) -> Result<dds_entity_t, String> {
    unsafe {
        let t = create_topic(dp, &topic_name, &type_name, type_info, keyless)?;

        match read_period {
            None => {
                // Use a Listener to route data as soon as it arrives
                let arg = Box::new((topic_name, z_key, z, congestion_ctrl));
                let sub_listener =
                    dds_create_listener(Box::into_raw(arg) as *mut std::os::raw::c_void);
                dds_lset_data_available(sub_listener, Some(data_forwarder_listener));
                let qos_native = qos.to_qos_native();
                let reader = dds_create_reader(dp, t, qos_native, sub_listener);
                Qos::delete_qos_native(qos_native);
                if reader >= 0 {
                    let res = dds_reader_wait_for_historical_data(reader, qos::DDS_100MS_DURATION);
                    if res < 0 {
                        tracing::error!(
                            "Error calling dds_reader_wait_for_historical_data(): {}",
                            CStr::from_ptr(dds_strretcode(-res))
                                .to_str()
                                .unwrap_or("unrecoverable DDS retcode")
                        );
                    }
                    Ok(reader)
                } else {
                    Err(format!(
                        "Error creating DDS Reader: {}",
                        CStr::from_ptr(dds_strretcode(-reader))
                            .to_str()
                            .unwrap_or("unrecoverable DDS retcode")
                    ))
                }
            }
            Some(period) => {
                // Use a periodic task that takes data to route from a Reader with KEEP_LAST 1
                qos.history = Some(History {
                    kind: HistoryKind::KEEP_LAST,
                    depth: 1,
                });
                let qos_native = qos.to_qos_native();
                let reader = dds_create_reader(dp, t, qos_native, std::ptr::null());
                let z_key = z_key.into_owned();
                tokio::task::spawn(async move {
                    // loop while reader's instance handle remain the same
                    // (if reader was deleted, its dds_entity_t value might have been
                    // reused by a new entity... don't trust it! Only trust instance handle)
                    let mut original_handle: dds_instance_handle_t = 0;
                    dds_get_instance_handle(reader, &mut original_handle);
                    let mut handle: dds_instance_handle_t = 0;
                    while dds_get_instance_handle(reader, &mut handle) == DDS_RETCODE_OK as i32 {
                        if handle != original_handle {
                            break;
                        }

                        tokio::time::sleep(period).await;
                        let mut zp: *mut ddsi_serdata = std::ptr::null_mut();
                        #[allow(clippy::uninit_assumed_init)]
                        let mut si = MaybeUninit::<[dds_sample_info_t; 1]>::uninit();
                        while dds_takecdr(
                            reader,
                            &mut zp,
                            1,
                            si.as_mut_ptr() as *mut dds_sample_info_t,
                            DDS_ANY_STATE,
                        ) > 0
                        {
                            let si = si.assume_init();
                            if si[0].valid_data {
                                tracing::trace!(
                                    "Route (periodic) data to zenoh resource with rid={}",
                                    z_key
                                );

                                let raw_sample = DDSRawSample::create(zp);

                                match raw_sample {
                                    Ok(raw_sample) => {
                                        let _ = z
                                            .put(&z_key, raw_sample)
                                            .congestion_control(congestion_ctrl)
                                            .wait();
                                    }
                                    Err(error) => {
                                        tracing::warn!(
                                            "Failed to route (periodic) data to zenoh resource with rid={} (msg: {})",
                                            z_key,
                                            error
                                        );
                                    }
                                };
                            }
                            ddsi_serdata_unref(zp);
                        }
                    }
                });
                Ok(reader)
            }
        }
    }
}

unsafe fn create_topic(
    dp: dds_entity_t,
    topic_name: &str,
    type_name: &str,
    type_info: &Option<TypeInfo>,
    keyless: bool,
) -> Result<dds_entity_t, String> {
    let cton = CString::new(topic_name.to_owned()).unwrap().into_raw();
    let ctyn = CString::new(type_name.to_owned()).unwrap().into_raw();

    let topic: dds_entity_t = match type_info {
        None => cdds_create_blob_topic(dp, cton, ctyn, keyless),
        Some(type_info) => {
            let mut descriptor: *mut dds_topic_descriptor_t = std::ptr::null_mut();

            let ret = dds_create_topic_descriptor(
                dds_find_scope_DDS_FIND_SCOPE_GLOBAL,
                dp,
                type_info.ptr,
                500000000,
                &mut descriptor,
            );
            if ret == (DDS_RETCODE_OK as i32) {
                let topic =
                    dds_create_topic(dp, descriptor, cton, std::ptr::null(), std::ptr::null());
                dds_delete_topic_descriptor(descriptor);
                topic
            } else {
                ret
            }
        }
    };
    if topic < 0 {
        Err(format!(
            "Error creating DDS Topic: {}",
            CStr::from_ptr(dds_strretcode(-topic))
                .to_str()
                .unwrap_or("unrecoverable DDS retcode")
        ))
    } else {
        Ok(topic)
    }
}

pub fn create_forwarding_dds_writer(
    dp: dds_entity_t,
    topic_name: String,
    type_name: String,
    type_info: &Option<TypeInfo>,
    keyless: bool,
    mut qos: Qos,
) -> Result<dds_entity_t, String> {
    unsafe {
        let t = create_topic(dp, &topic_name, &type_name, type_info, keyless)?;

        // force RELIABLE QoS for Writers (#165)
        if let Some(qos::Reliability {
            kind: qos::ReliabilityKind::BEST_EFFORT,
            ..
        }) = &mut qos.reliability
        {
            // Per DDS specification, the default Reliability value for DataWriters is RELIABLE with max_blocking_time=100ms
            // Thus just use default value.
            qos.reliability = None;
        }

        let qos_native = qos.to_qos_native();
        let writer: i32 = dds_create_writer(dp, t, qos_native, std::ptr::null_mut());
        Qos::delete_qos_native(qos_native);
        if writer >= 0 {
            Ok(writer)
        } else {
            Err(format!(
                "Error creating DDS Writer: {}",
                CStr::from_ptr(dds_strretcode(-writer))
                    .to_str()
                    .unwrap_or("unrecoverable DDS retcode")
            ))
        }
    }
}

pub fn delete_dds_entity(entity: dds_entity_t) -> Result<(), String> {
    unsafe {
        let r = dds_delete(entity);
        match r {
            0 | DDS_RETCODE_ALREADY_DELETED => Ok(()),
            e => Err(format!("Error deleting DDS entity - retcode={e}")),
        }
    }
}

pub fn get_guid(entity: &dds_entity_t) -> Result<String, String> {
    unsafe {
        let mut guid = dds_guid_t { v: [0; 16] };
        let r = dds_get_guid(*entity, &mut guid);
        if r == 0 {
            Ok(hex::encode(guid.v))
        } else {
            Err(format!("Error getting GUID of DDS entity - retcode={r}"))
        }
    }
}

pub fn serialize_entity_guid<S>(entity: &dds_entity_t, s: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    match get_guid(entity) {
        Ok(guid) => s.serialize_str(&guid),
        Err(_) => s.serialize_str("UNKOWN_GUID"),
    }
}
