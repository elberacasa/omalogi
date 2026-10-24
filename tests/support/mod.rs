//! An emulated wired G502 X (046d:c099) for integration tests.
//!
//! It answers HID++ 2.0 requests with the bytes a real device returned
//! (`tests/fixtures/g502x-c099.json`), replies with HID++ error reports where a
//! device would, and keeps mode, active profile and profile memory as state.
//! Memory writes follow the sequence libratbag uses; factory sectors refuse them.

use std::{
    collections::HashMap,
    error::Error,
    sync::{Arc, Mutex},
};

use hidpp::{async_trait, channel::RawHidChannel};
use omalogi::onboard::{
    edit::{Table, binding_range},
    format::crc_ccitt,
};
use tokio::sync::{Mutex as AsyncMutex, mpsc};

const FIXTURE: &str = include_str!("../fixtures/g502x-c099.json");

const LONG_REPORT_ID: u8 = 0x11;
const LONG_REPORT_LEN: usize = 20;
const ERROR_FEATURE_INDEX: u8 = 0xFF;
const MEMORY_CHUNK: usize = 16;
const FIRST_FACTORY_SECTOR: u16 = 0x0100;
const MAX_SECTOR_LEN: usize = 4096;

// HID++ 2.0 error codes.
const ERR_INVALID_ARGUMENT: u8 = 0x02;
const ERR_INVALID_FEATURE_INDEX: u8 = 0x06;
const ERR_INVALID_FUNCTION_ID: u8 = 0x07;

type BoxError = Box<dyn Error + Sync + Send>;

/// A memory write between `memoryAddrWrite` and `memoryWriteEnd`.
pub struct PendingWrite {
    sector: u16,
    size: usize,
    data: Vec<u8>,
}

/// Mutable device state, shared with the test for setup and assertions.
pub struct State {
    /// `getMode` value: 1 onboard, 2 host.
    pub mode: u8,
    /// 1-based active profile index.
    pub current_profile: u8,
    /// When set, `setCurrentProfile` succeeds without changing anything.
    pub ignore_profile_switch: bool,
    /// Profiles the firmware loaded, in order. Like the real mouse, it loads a profile's
    /// settings only when switching to a different profile.
    pub loads: Vec<u8>,
    /// The profile memory the firmware last loaded. Profile N lives in sector N here.
    pub loaded_sector: Option<Vec<u8>>,
    /// Profile memory by sector number.
    pub sectors: HashMap<u16, Vec<u8>>,
    pub pending_write: Option<PendingWrite>,
    /// When set, the next committed write stores a flipped first byte.
    pub corrupt_next_write: bool,
    /// When set, the committed write with this index (0-based) stores a flipped first byte.
    pub corrupt_write_number: Option<usize>,
    /// When set, a committed write to the profile directory makes the firmware report
    /// this profile as current, without loading it. A wired G502 X and a G502 X Lightspeed
    /// both move their current profile when a profile before the active one is turned on
    /// or off (docs/hardware-tests.md, 2026-09-30); the exact rule differs, so tests name
    /// the outcome instead of modelling it.
    pub current_after_directory_write: Option<u8>,
    /// Sectors committed by `memoryWriteEnd`, in order.
    pub committed: Vec<u16>,
    /// Every report the host wrote, in order.
    pub requests: Vec<Vec<u8>>,
}

struct Feature {
    index: u8,
    id: u16,
    version: u8,
}

struct Fixture {
    features: Vec<Feature>,
    device_info: Vec<u8>,
    fw_entities: Vec<Vec<u8>>,
    dpi_list: Vec<u8>,
    dpi_current: Vec<u8>,
    rate_list: Vec<u8>,
    rate_current: Vec<u8>,
    description: Vec<u8>,
}

pub struct FakeG502x {
    fixture: Fixture,
    product_id: u16,
    /// What `DeviceTypeAndName` (0x0005) answers: the type (3 is a mouse) and the name.
    device_type: u8,
    device_name: String,
    state: Arc<Mutex<State>>,
    responses: mpsc::UnboundedSender<Vec<u8>>,
    reports: AsyncMutex<mpsc::UnboundedReceiver<Vec<u8>>>,
}

/// The dump's profile memory by sector number.
pub fn fixture_sectors() -> HashMap<u16, Vec<u8>> {
    let json: serde_json::Value = serde_json::from_str(FIXTURE).expect("fixture is JSON");
    json["onboard"]["sectors"]
        .as_object()
        .expect("sectors")
        .iter()
        .map(|(id, data)| {
            (
                u16::from_str_radix(id, 16).expect("sector id"),
                hex(data.as_str().expect("hex")),
            )
        })
        .collect()
}

impl FakeG502x {
    pub fn new() -> Self {
        let json: serde_json::Value = serde_json::from_str(FIXTURE).expect("fixture is JSON");
        let hex_at = |pointer: &str| {
            hex(json
                .pointer(pointer)
                .and_then(serde_json::Value::as_str)
                .unwrap_or_else(|| panic!("fixture has no string at {pointer}")))
        };
        let fixture = Fixture {
            features: json["features"]
                .as_array()
                .expect("feature list")
                .iter()
                .map(|f| Feature {
                    index: u8::try_from(f["index"].as_u64().expect("index")).expect("u8"),
                    id: u16::from_str_radix(f["id"].as_str().expect("id"), 16).expect("hex id"),
                    version: u8::try_from(f["version"].as_u64().expect("version")).expect("u8"),
                })
                .collect(),
            device_info: hex_at("/device_info"),
            fw_entities: json["fw_entities"]
                .as_array()
                .expect("firmware entities")
                .iter()
                .map(|e| hex(e.as_str().expect("hex")))
                .collect(),
            dpi_list: hex_at("/dpi_sensor0/dpi_list_raw"),
            dpi_current: hex_at("/dpi_sensor0/current_raw"),
            rate_list: hex_at("/report_rate/list_bitmap_raw"),
            rate_current: hex_at("/report_rate/current_raw"),
            description: hex_at("/onboard/description"),
        };
        let current_profile = hex_at("/onboard/current_profile")[1];
        let sectors = fixture_sectors();
        let state = State {
            mode: hex_at("/onboard/mode")[0],
            current_profile,
            ignore_profile_switch: false,
            loads: Vec::new(),
            loaded_sector: sectors.get(&u16::from(current_profile)).cloned(),
            sectors,
            pending_write: None,
            corrupt_next_write: false,
            corrupt_write_number: None,
            current_after_directory_write: None,
            committed: Vec::new(),
            requests: Vec::new(),
        };
        let (responses, reports) = mpsc::unbounded_channel();
        Self {
            fixture,
            product_id: 0xC099,
            device_type: 3,
            device_name: "G502 X".to_owned(),
            state: Arc::new(Mutex::new(state)),
            responses,
            reports: AsyncMutex::new(reports),
        }
    }

    /// The same emulated mouse under another USB product id, as an untested model.
    #[allow(dead_code)]
    pub fn with_product_id(mut self, product_id: u16) -> Self {
        self.product_id = product_id;
        self
    }

    /// Appends a Softdevice entity whose build is binary `0x00a9` and whose prefix is NUL
    /// bytes, as a G502 Lightspeed reports.
    #[allow(dead_code)]
    pub fn with_non_bcd_firmware_entity(self) -> Self {
        self.with_firmware_entity(vec![
            0x05, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xA9, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00,
        ])
    }

    /// Appends an entity whose prefix is not text, which openlogi-hidpp cannot decode.
    #[allow(dead_code)]
    pub fn with_unreadable_firmware_entity(self) -> Self {
        self.with_firmware_entity(vec![
            0x05, 0xFF, 0xFF, 0xFF, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00,
        ])
    }

    fn with_firmware_entity(mut self, entity: Vec<u8>) -> Self {
        self.fixture.device_info[0] = self.fixture.device_info[0].saturating_add(1);
        self.fixture.fw_entities.push(entity);
        self
    }

    /// The same device, answering `DeviceTypeAndName` with another type (0 is a keyboard).
    #[allow(dead_code)]
    pub fn with_device_type(mut self, device_type: u8) -> Self {
        self.device_type = device_type;
        self
    }

    /// The same device, naming itself `name`.
    #[allow(dead_code)]
    pub fn with_device_name(mut self, name: &str) -> Self {
        name.clone_into(&mut self.device_name);
        self
    }

    /// The same memory, reported under another onboard profile format.
    #[allow(dead_code)]
    pub fn with_profile_format(mut self, profile_format: u8) -> Self {
        self.fixture.description[1] = profile_format;
        self
    }

    /// The same mouse with profile 4 holding firmware actions with the nonzero tail bytes
    /// other mice store: the G502 Hero's `90 xx ff ff` and the G502 X Lightspeed's
    /// `90 xx ff 00`, where this dump's wired G502 X holds `90 xx 00 00`. The two slots
    /// are written as raw bytes, since the editor would leave an unchanged action alone,
    /// and the sector's checksum fixed, so the profile reads back as one a mouse of either
    /// kind would answer with.
    #[allow(dead_code)]
    pub fn with_special_action_tails(self) -> Self {
        let mut sector = {
            let state = self.state.lock().expect("fake device state");
            state.sectors[&4].clone()
        };
        for (slot, raw) in [
            (4usize, [0x90u8, 0x07, 0xFF, 0xFF]),
            (5, [0x90, 0x03, 0xFF, 0x00]),
        ] {
            let at = binding_range(Table::Buttons, slot);
            sector[at].copy_from_slice(&raw);
        }
        let crc_at = sector.len() - 2;
        let crc = crc_ccitt(&sector[..crc_at]);
        sector[crc_at..].copy_from_slice(&crc.to_be_bytes());

        self.state
            .lock()
            .expect("fake device state")
            .sectors
            .insert(4, sector);
        self
    }

    /// The same mouse before anything was written to its user memory: the directory and
    /// every user profile sector erased to `0xFF`, as a G502 X Lightspeed that G HUB never
    /// set up reads its directory. Only the factory profiles hold data.
    #[allow(dead_code)]
    pub fn with_blank_user_memory(self) -> Self {
        {
            let mut state = self.state.lock().expect("fake device state");
            for (_, data) in state
                .sectors
                .iter_mut()
                .filter(|(sector, _)| **sector < FIRST_FACTORY_SECTOR)
            {
                data.fill(0xFF);
            }
            state.loaded_sector = None;
        }
        self
    }

    /// The same mouse reporting no factory profiles.
    #[allow(dead_code)]
    pub fn without_factory_profiles(mut self) -> Self {
        self.fixture.description[4] = 0;
        self
    }

    /// The same mouse reporting `count` user sectors.
    #[allow(dead_code)]
    pub fn with_sector_count(mut self, count: u8) -> Self {
        self.fixture.description[6] = count;
        self
    }

    pub fn state(&self) -> Arc<Mutex<State>> {
        Arc::clone(&self.state)
    }

    /// The feature table index the device assigns to `id`.
    // Each test binary compiles this module; not every one needs every helper.
    #[allow(dead_code)]
    pub fn feature_index(&self, id: u16) -> u8 {
        self.fixture
            .features
            .iter()
            .find(|f| f.id == id)
            .map(|f| f.index)
            .expect("feature present in fixture")
    }

    fn respond(&self, request: &[u8]) -> Option<Vec<u8>> {
        let &[
            _report_id,
            device_index,
            feature_index,
            function_sw,
            ref params @ ..,
        ] = request
        else {
            return None;
        };
        let function = function_sw >> 4;
        let result = if feature_index == 0 {
            self.root(function, params)
        } else {
            match self
                .fixture
                .features
                .iter()
                .find(|f| f.index == feature_index)
            {
                Some(feature) => self.feature(feature.id, function, params),
                None => Err(ERR_INVALID_FEATURE_INDEX),
            }
        };

        let mut report = vec![0; LONG_REPORT_LEN];
        report[0] = LONG_REPORT_ID;
        report[1] = device_index;
        match result {
            Ok(payload) => {
                report[2] = feature_index;
                report[3] = function_sw;
                let len = payload.len().min(LONG_REPORT_LEN - 4);
                report[4..4 + len].copy_from_slice(&payload[..len]);
            }
            Err(code) => {
                report[2] = ERROR_FEATURE_INDEX;
                report[3] = feature_index;
                report[4] = function_sw;
                report[5] = code;
            }
        }
        Some(report)
    }

    fn root(&self, function: u8, params: &[u8]) -> Result<Vec<u8>, u8> {
        match function {
            // getFeature(id) -> index, type, version; index 0 when absent.
            0 => {
                let id = u16::from_be_bytes([params[0], params[1]]);
                Ok(match self.fixture.features.iter().find(|f| f.id == id) {
                    Some(feature) => vec![feature.index, 0, feature.version],
                    None => vec![0, 0, 0],
                })
            }
            // getProtocolVersion: HID++ 4.2, echoing the ping byte.
            1 => Ok(vec![4, 2, params[2]]),
            _ => Err(ERR_INVALID_FUNCTION_ID),
        }
    }

    fn feature(&self, id: u16, function: u8, params: &[u8]) -> Result<Vec<u8>, u8> {
        let fx = &self.fixture;
        let mut state = self.state.lock().expect("fake device state");
        match (id, function) {
            (0x0003, 0) => Ok(fx.device_info.clone()),
            // getDeviceNameCount, getDeviceName(index), getDeviceType.
            (0x0005, 0) => Ok(vec![
                u8::try_from(self.device_name.len()).unwrap_or(u8::MAX),
            ]),
            (0x0005, 1) => {
                let name = self.device_name.as_bytes();
                let from = usize::from(params[0]).min(name.len());
                let mut chunk = name[from..].to_vec();
                chunk.resize(16, 0);
                Ok(chunk)
            }
            (0x0005, 2) => Ok(vec![self.device_type]),
            (0x0003, 1) => fx
                .fw_entities
                .get(usize::from(params[0]))
                .cloned()
                .ok_or(ERR_INVALID_ARGUMENT),
            (0x2201, 0) => Ok(vec![1]),
            (0x2201, 1) => Ok(fx.dpi_list.clone()),
            // getSensorDpi and getReportRate: what the loaded profile sets, as the mouse
            // runs it, or the dump's own values before any profile was loaded.
            (0x2201, 2) => Ok(match &state.loaded_sector {
                Some(sector) => {
                    let index = usize::from(sector[1]).min(4);
                    let dpi = u16::from_le_bytes([sector[3 + index * 2], sector[4 + index * 2]]);
                    let mut reply = fx.dpi_current.clone();
                    reply[1..3].copy_from_slice(&dpi.to_be_bytes());
                    reply
                }
                None => fx.dpi_current.clone(),
            }),
            // setSensorDpi: a G502 X running its onboard profiles refuses it.
            (0x2201, 3) if state.mode == 1 => Err(ERR_INVALID_ARGUMENT),
            (0x8060, 0) => Ok(fx.rate_list.clone()),
            (0x8060, 1) => Ok(match &state.loaded_sector {
                Some(sector) => {
                    let mut reply = fx.rate_current.clone();
                    reply[0] = sector[0];
                    reply
                }
                None => fx.rate_current.clone(),
            }),
            (0x8100, 0) => Ok(fx.description.clone()),
            (0x8100, 2) => Ok(vec![state.mode]),
            (0x8100, 3) => {
                let index = params[1];
                if !(1..=fx.description[3]).contains(&index) {
                    return Err(ERR_INVALID_ARGUMENT);
                }
                if !state.ignore_profile_switch {
                    if index != state.current_profile {
                        state.loads.push(index);
                        state.loaded_sector = state.sectors.get(&u16::from(index)).cloned();
                    }
                    state.current_profile = index;
                }
                Ok(Vec::new())
            }
            (0x8100, 4) => Ok(vec![0, state.current_profile]),
            (0x8100, 5) => {
                let sector = u16::from_be_bytes([params[0], params[1]]);
                let offset = usize::from(u16::from_be_bytes([params[2], params[3]]));
                let data = state.sectors.get(&sector).ok_or(ERR_INVALID_ARGUMENT)?;
                data.get(offset..offset + MEMORY_CHUNK)
                    .map(<[u8]>::to_vec)
                    .ok_or(ERR_INVALID_ARGUMENT)
            }
            // memoryAddrWrite(sector, offset, size): whole user sectors only.
            (0x8100, 6) => {
                let sector = u16::from_be_bytes([params[0], params[1]]);
                let offset = u16::from_be_bytes([params[2], params[3]]);
                let size = usize::from(u16::from_be_bytes([params[4], params[5]]));
                if sector >= FIRST_FACTORY_SECTOR
                    || offset != 0
                    || size == 0
                    || size > MAX_SECTOR_LEN
                {
                    return Err(ERR_INVALID_ARGUMENT);
                }
                state.pending_write = Some(PendingWrite {
                    sector,
                    size,
                    data: Vec::with_capacity(size),
                });
                Ok(Vec::new())
            }
            (0x8100, 7) => {
                let pending = state.pending_write.as_mut().ok_or(ERR_INVALID_ARGUMENT)?;
                pending.data.extend_from_slice(&params[..MEMORY_CHUNK]);
                Ok(Vec::new())
            }
            (0x8100, 8) => {
                let mut pending = state.pending_write.take().ok_or(ERR_INVALID_ARGUMENT)?;
                if pending.data.len() < pending.size {
                    return Err(ERR_INVALID_ARGUMENT);
                }
                pending.data.truncate(pending.size);
                if state.corrupt_next_write
                    || state.corrupt_write_number == Some(state.committed.len())
                {
                    state.corrupt_next_write = false;
                    pending.data[0] ^= 0xFF;
                }
                if pending.sector == 0
                    && let Some(index) = state.current_after_directory_write
                {
                    state.current_profile = index;
                }
                state.sectors.insert(pending.sector, pending.data);
                state.committed.push(pending.sector);
                Ok(Vec::new())
            }
            _ => Err(ERR_INVALID_FUNCTION_ID),
        }
    }
}

#[async_trait]
impl RawHidChannel for FakeG502x {
    fn vendor_id(&self) -> u16 {
        0x046D
    }

    fn product_id(&self) -> u16 {
        self.product_id
    }

    async fn write_report(&self, src: &[u8]) -> Result<usize, BoxError> {
        self.state
            .lock()
            .expect("fake device state")
            .requests
            .push(src.to_vec());
        if let Some(response) = self.respond(src) {
            self.responses.send(response)?;
        }
        Ok(src.len())
    }

    async fn read_report(&self, buf: &mut [u8]) -> Result<usize, BoxError> {
        let Some(report) = self.reports.lock().await.recv().await else {
            // The sender lives as long as the device; never reached while it exists.
            return std::future::pending().await;
        };
        let len = report.len().min(buf.len());
        buf[..len].copy_from_slice(&report[..len]);
        Ok(len)
    }

    fn supports_short_long_hidpp(&self) -> Option<(bool, bool)> {
        Some((true, true))
    }

    async fn get_report_descriptor(&self, _buf: &mut [u8]) -> Result<usize, BoxError> {
        Err("the emulated device has no report descriptor".into())
    }
}

fn hex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).expect("fixture hex"))
        .collect()
}
