//! `omalogi check`: a read-only test that Omalogi reads a mouse's profile memory the way the
//! mouse does.
//!
//! Edits patch only the bytes of the fields they change (see [`crate::onboard::edit`]), so
//! on a layout read at the wrong offsets an edit would write a field in the wrong place.
//! Each check compares what Omalogi decodes with something that does not depend on that
//! decoding: the sector's own checksum, a second read, the bytes each decoded value
//! encodes back to, and the DPI and report rate the mouse reports for itself. Nothing is
//! written, so it is safe on a mouse nobody has verified yet.

use std::fmt;

use serde::Serialize;

use crate::{
    consent::Support,
    device::{Firmware, Info, Session, SessionError, interval_to_hz, read_directory},
    editing::EditError,
    onboard::{
        Mode, OnboardProfilesFeature, action,
        edit::{ProfileEditor, Table, binding_range},
        format::{self, BINDING_LEN, BUTTON_SLOTS, Binding, DPI_STAGE_COUNT, Description, Profile},
    },
};

/// How a check came out, ordered from best to worst.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Pass,
    /// Unusual, but not a sign that Omalogi misreads the mouse.
    Warn,
    /// Omalogi reads this differently from the mouse; editing it is not safe.
    Fail,
}

#[derive(Debug, Clone, Serialize)]
pub struct Finding {
    /// A stable name for the check, such as `checksum`.
    pub check: &'static str,
    /// The profile it is about (1-based), or none for the mouse as a whole.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<usize>,
    pub outcome: Outcome,
    pub detail: String,
}

impl fmt::Display for Finding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.profile {
            Some(number) => write!(f, "profile {number} {}: {}", self.check, self.detail),
            None => write!(f, "{}: {}", self.check, self.detail),
        }
    }
}

/// The result of `omalogi check`. It holds no unit ID and no profile memory, so it can be
/// posted in an issue as it is.
#[derive(Debug, Clone, Serialize)]
pub struct CheckReport {
    pub device: &'static str,
    pub product_id: u16,
    pub firmware: Vec<Firmware>,
    pub support: Support,
    pub mode: Mode,
    /// The worst outcome of any finding.
    pub outcome: Outcome,
    pub findings: Vec<Finding>,
}

#[derive(Default)]
struct Findings(Vec<Finding>);

impl Findings {
    fn push(
        &mut self,
        check: &'static str,
        profile: Option<usize>,
        outcome: Outcome,
        detail: impl Into<String>,
    ) {
        self.0.push(Finding {
            check,
            profile,
            outcome,
            detail: detail.into(),
        });
    }
}

impl CheckReport {
    /// The failures that make editing unsafe: every one but the profile directory's. A
    /// damaged directory is rebuilt by `profiles repair`, which is itself an edit, so it
    /// must not stand in the way of accepting the mouse.
    pub fn misreads(&self) -> impl Iterator<Item = &Finding> {
        self.findings
            .iter()
            .filter(|finding| finding.outcome == Outcome::Fail && finding.check != "directory")
    }
}

impl Session {
    /// Reads the mouse's profile memory and checks that Omalogi decodes it as the mouse
    /// does. Writes nothing.
    pub async fn check(&mut self) -> Result<CheckReport, EditError> {
        let info = self.info().await?;
        let support = self.support().await?;
        let feature = self.onboard_feature().await?;
        let description = feature.description().await?;
        let mut findings = Findings::default();

        if description.is_decodable() {
            findings.push(
                "layout",
                None,
                Outcome::Pass,
                format!(
                    "memory model {}, profile format {}: {}",
                    description.memory_model,
                    description.profile_format,
                    if description.is_verified() {
                        "a layout verified on real hardware"
                    } else {
                        "read like the verified layouts, not tested on hardware yet"
                    }
                ),
            );
            check_memory(&feature, &description, &info, &mut findings).await?;
        } else {
            findings.push(
                "layout",
                None,
                Outcome::Fail,
                format!(
                    "memory model {}, profile format {} is a layout Omalogi does not read",
                    description.memory_model, description.profile_format
                ),
            );
        }

        let findings = findings.0;
        Ok(CheckReport {
            device: info.name,
            product_id: info.product_id,
            firmware: info.firmware,
            support,
            mode: info.onboard_mode,
            outcome: findings
                .iter()
                .map(|finding| finding.outcome)
                .max()
                .unwrap_or(Outcome::Pass),
            findings,
        })
    }
}

async fn check_memory(
    feature: &OnboardProfilesFeature,
    description: &Description,
    info: &Info,
    findings: &mut Findings,
) -> Result<(), EditError> {
    let entries = match read_directory(feature, description).await {
        Ok(entries) => entries,
        Err(SessionError::DirectoryNeverWritten) => {
            findings.push(
                "directory",
                None,
                Outcome::Warn,
                "the mouse has no profile directory yet and runs its factory profiles; \
                 `omalogi profiles repair` sets one up, then run the check again",
            );
            return Ok(());
        }
        Err(SessionError::InvalidDirectoryChecksum) => {
            findings.push(
                "directory",
                None,
                Outcome::Fail,
                "the profile directory fails its checksum; `omalogi profiles repair \
                 --dry-run` says whether it can be rebuilt",
            );
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    };
    findings.push(
        "directory",
        None,
        Outcome::Pass,
        format!(
            "its checksum matches, and it lists {} profiles",
            entries.len()
        ),
    );

    let active = format::current_profile_position(
        feature
            .current_profile_index()
            .await
            .map_err(SessionError::from)?,
    );
    let size = description.sector_size;
    for (position, entry) in entries.iter().enumerate() {
        let number = Some(position + 1);
        let first = feature.read_sector(entry.sector, size).await?;
        let second = feature.read_sector(entry.sector, size).await?;
        // Reads that overlap another program's traffic can come back wrong, so two reads
        // that differ say nothing about the memory itself.
        if first != second {
            findings.push(
                "checksum",
                number,
                Outcome::Fail,
                format!(
                    "two reads of sector {:04x} differ; close other programs that talk to \
                     the mouse and run the check again",
                    entry.sector
                ),
            );
            continue;
        }
        if !format::sector_crc_valid(&first) {
            findings.push(
                "checksum",
                number,
                Outcome::Fail,
                format!("sector {:04x} fails its checksum", entry.sector),
            );
            continue;
        }
        findings.push(
            "checksum",
            number,
            Outcome::Pass,
            format!(
                "sector {:04x} matches its checksum and reads the same twice",
                entry.sector
            ),
        );
        match Profile::parse(&first, description) {
            Ok(profile) => check_profile(
                position + 1,
                &first,
                &profile,
                description,
                info,
                active == Some(position),
                findings,
            ),
            Err(error) => findings.push("decode", number, Outcome::Fail, error.to_string()),
        }
    }
    Ok(())
}

/// The checks of one profile sector that need no device: `sector` as read, `profile` as
/// decoded from it, and `info` as the mouse reports itself.
fn check_profile(
    number: usize,
    sector: &[u8],
    profile: &Profile,
    description: &Description,
    info: &Info,
    active: bool,
    findings: &mut Findings,
) {
    let number = Some(number);
    check_round_trip(number, sector, profile, description, findings);
    check_bindings(number, sector, findings);
    check_dpi(number, profile, info, findings);

    let hz = interval_to_hz(profile.report_rate_ms);
    match hz {
        None => findings.push(
            "report_rate",
            number,
            Outcome::Fail,
            format!(
                "the report interval byte reads {}, which is no report rate",
                profile.report_rate_ms
            ),
        ),
        Some(hz) if !info.report_rates_hz.contains(&hz) => findings.push(
            "report_rate",
            number,
            Outcome::Warn,
            format!("{hz} Hz is not a rate the mouse lists"),
        ),
        Some(hz) => findings.push("report_rate", number, Outcome::Pass, format!("{hz} Hz")),
    }

    if !active {
        return;
    }
    match (hz, info.report_rate_hz) {
        (Some(hz), Some(live)) if hz == live => findings.push(
            "live_report_rate",
            number,
            Outcome::Pass,
            format!("the profile in use stores {hz} Hz, and the mouse runs at {live} Hz"),
        ),
        (_, live) => findings.push(
            "live_report_rate",
            number,
            Outcome::Warn,
            format!(
                "the profile in use stores {}, but the mouse runs at {}; another program \
                 may have set the rate",
                hz.map_or_else(|| "no rate".to_owned(), |hz| format!("{hz} Hz")),
                live.map_or_else(|| "an unknown rate".to_owned(), |hz| format!("{hz} Hz")),
            ),
        ),
    }
    let stages: Vec<u16> = profile.dpi_stages.iter().flatten().copied().collect();
    if stages.contains(&info.dpi) {
        findings.push(
            "live_dpi",
            number,
            Outcome::Pass,
            format!(
                "the mouse runs at {} DPI, one of the profile's stages",
                info.dpi
            ),
        );
    } else {
        findings.push(
            "live_dpi",
            number,
            Outcome::Warn,
            format!(
                "the mouse runs at {} DPI, which is none of the profile's stages; another \
                 program may have set it",
                info.dpi
            ),
        );
    }
}

/// Every decoded field written back through the editor must leave the sector as read, CRC
/// included.
fn check_round_trip(
    number: Option<usize>,
    sector: &[u8],
    profile: &Profile,
    description: &Description,
    findings: &mut Findings,
) {
    let Ok(mut editor) = ProfileEditor::new(sector, description) else {
        findings.push(
            "round_trip",
            number,
            Outcome::Fail,
            "the editor cannot open this sector",
        );
        return;
    };
    editor.set_report_rate_ms(profile.report_rate_ms);
    editor.set_default_dpi_index(profile.default_dpi_index);
    editor.set_shift_dpi_index(profile.shift_dpi_index);
    editor.set_dpi_stages(profile.dpi_stages);
    for (table, bindings) in [
        (Table::Buttons, &profile.buttons),
        (Table::GShift, &profile.gshift_buttons),
    ] {
        for (slot, binding) in bindings.iter().enumerate() {
            editor.set_binding(table, slot, *binding);
        }
    }
    editor.set_name(profile.name.as_deref());
    let rebuilt = editor.finish();
    let changed: Vec<String> = sector
        .iter()
        .zip(&rebuilt)
        .enumerate()
        .filter(|(_, (before, after))| before != after)
        .map(|(offset, _)| offset.to_string())
        .collect();
    if changed.is_empty() {
        findings.push(
            "round_trip",
            number,
            Outcome::Pass,
            format!(
                "writing every decoded field back reproduces all {} bytes",
                sector.len()
            ),
        );
    } else {
        findings.push(
            "round_trip",
            number,
            Outcome::Fail,
            format!(
                "writing the decoded fields back changes bytes {}",
                changed.join(", ")
            ),
        );
    }
}

/// Each binding must encode back to the bytes it was read from, and its action text must
/// read back as the same action.
fn check_bindings(number: Option<usize>, sector: &[u8], findings: &mut Findings) {
    let mut problems = Vec::new();
    let mut unknown = Vec::new();
    let mut texts = 0;
    for (table, name) in [
        (Table::Buttons, "button"),
        (Table::GShift, "G-Shift button"),
    ] {
        for slot in 0..BUTTON_SLOTS {
            let raw: [u8; BINDING_LEN] = sector[binding_range(table, slot)]
                .try_into()
                .expect("slice has binding length");
            let binding = Binding::decode(raw);
            if binding.encode() != raw {
                problems.push(format!("{name} {slot} does not encode back to its bytes"));
            }
            if matches!(binding, Binding::Unknown { .. }) {
                unknown.push(format!("{name} {slot}"));
            }
            if let Some(text) = action::action_text(&binding) {
                texts += 1;
                match action::parse_action(&text) {
                    Ok(parsed) if parsed.same_action(&binding) => {}
                    _ => problems.push(format!(
                        "{name} {slot}'s action `{text}` does not read back as itself"
                    )),
                }
            }
        }
    }
    let total = BUTTON_SLOTS * 2;
    if !problems.is_empty() {
        findings.push("bindings", number, Outcome::Fail, problems.join("; "));
    } else if !unknown.is_empty() {
        findings.push(
            "bindings",
            number,
            Outcome::Warn,
            format!(
                "{} of {total} bindings are of a kind Omalogi does not know ({}); they are \
                 kept as stored",
                unknown.len(),
                unknown.join(", ")
            ),
        );
    } else {
        findings.push(
            "bindings",
            number,
            Outcome::Pass,
            format!(
                "all {total} bindings encode back to their bytes, and {texts} with action \
                 text read back as themselves"
            ),
        );
    }
}

fn check_dpi(number: Option<usize>, profile: &Profile, info: &Info, findings: &mut Findings) {
    let stages: Vec<u16> = profile.dpi_stages.iter().flatten().copied().collect();
    let (Some(&min), Some(&max)) = (info.dpi_values.first(), info.dpi_values.last()) else {
        findings.push(
            "dpi",
            number,
            Outcome::Warn,
            "the mouse lists no DPI values to compare the stages with",
        );
        return;
    };
    if stages.is_empty() {
        findings.push("dpi", number, Outcome::Fail, "the profile has no DPI stage");
        return;
    }
    if let Some(dpi) = stages.iter().find(|dpi| !(min..=max).contains(*dpi)) {
        findings.push(
            "dpi",
            number,
            Outcome::Fail,
            format!("a stage reads {dpi} DPI, outside the sensor's {min}–{max}"),
        );
        return;
    }
    let mut notes = Vec::new();
    if let Some(dpi) = stages.iter().find(|dpi| !info.dpi_values.contains(*dpi)) {
        notes.push(format!("{dpi} DPI is not a step the sensor lists"));
    }
    for (which, index) in [
        ("default", profile.default_dpi_index),
        ("shift", profile.shift_dpi_index),
    ] {
        let set = usize::from(index) < DPI_STAGE_COUNT
            && profile.dpi_stages[usize::from(index)].is_some();
        if !set {
            notes.push(format!(
                "the {which} DPI points at stage {index}, which is not set"
            ));
        }
    }
    let stages_text: Vec<String> = stages.iter().map(u16::to_string).collect();
    if notes.is_empty() {
        findings.push(
            "dpi",
            number,
            Outcome::Pass,
            format!(
                "stages {} DPI, within the sensor's range",
                stages_text.join(", ")
            ),
        );
    } else {
        findings.push("dpi", number, Outcome::Warn, notes.join("; "));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::onboard::format::{BUTTON_OFFSET, DPI_OFFSET, with_crc};

    const FIXTURE: &str = include_str!("../tests/fixtures/g502x-c099.json");

    fn fixture_hex(pointer: &str) -> Vec<u8> {
        let fixture: serde_json::Value = serde_json::from_str(FIXTURE).expect("fixture is JSON");
        let hex = fixture
            .pointer(pointer)
            .and_then(serde_json::Value::as_str)
            .unwrap_or_else(|| panic!("fixture has no string at {pointer}"));
        (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("fixture hex"))
            .collect()
    }

    fn description() -> Description {
        Description::parse(&fixture_hex("/onboard/description")).expect("description")
    }

    /// The wired G502 X as `omalogi info` reads it: 100–25600 DPI in steps of 50, at
    /// 1600 DPI and 1000 Hz.
    fn info() -> Info {
        Info {
            name: "G502 X",
            vendor_id: 0x046D,
            product_id: 0xC099,
            path: "/dev/hidraw0".to_owned(),
            firmware: Vec::new(),
            dpi: 1600,
            dpi_values: (2..=512).map(|step| step * 50).collect(),
            report_rate_hz: Some(1000),
            report_rates_hz: vec![125, 250, 500, 1000],
            onboard_mode: Mode::Onboard,
        }
    }

    fn outcomes(sector: &[u8], info: &Info, active: bool) -> Vec<(&'static str, Outcome)> {
        let profile = Profile::parse(sector, &description()).expect("profile");
        let mut findings = Findings::default();
        check_profile(
            1,
            sector,
            &profile,
            &description(),
            info,
            active,
            &mut findings,
        );
        findings
            .0
            .iter()
            .map(|finding| (finding.check, finding.outcome))
            .collect()
    }

    fn outcome_of(sector: &[u8], check: &str) -> Outcome {
        outcomes(sector, &info(), true)
            .into_iter()
            .find(|(name, _)| *name == check)
            .unwrap_or_else(|| panic!("no {check} finding"))
            .1
    }

    #[test]
    fn every_profile_of_the_g502x_passes() {
        for sector in ["0001", "0002", "0003", "0004", "0005"] {
            let data = fixture_hex(&format!("/onboard/sectors/{sector}"));
            let found = outcomes(&data, &info(), true);
            assert!(
                found.iter().all(|(_, outcome)| *outcome == Outcome::Pass),
                "sector {sector}: {found:?}"
            );
            assert_eq!(found.len(), 6, "sector {sector}: {found:?}");
        }
    }

    #[test]
    fn only_the_profile_in_use_is_compared_with_the_mouse() {
        let data = fixture_hex("/onboard/sectors/0003");
        let checks: Vec<_> = outcomes(&data, &info(), false)
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(checks, ["round_trip", "bindings", "dpi", "report_rate"]);
    }

    #[test]
    fn a_stage_outside_the_sensor_range_fails() {
        let mut data = fixture_hex("/onboard/sectors/0003");
        data[DPI_OFFSET..DPI_OFFSET + 2].copy_from_slice(&30_000u16.to_le_bytes());
        assert_eq!(outcome_of(&with_crc(data), "dpi"), Outcome::Fail);
    }

    #[test]
    fn a_stage_between_the_sensor_steps_is_only_a_warning() {
        let mut data = fixture_hex("/onboard/sectors/0003");
        data[DPI_OFFSET..DPI_OFFSET + 2].copy_from_slice(&825u16.to_le_bytes());
        assert_eq!(outcome_of(&with_crc(data), "dpi"), Outcome::Warn);
    }

    #[test]
    fn a_default_dpi_on_an_unset_stage_is_a_warning() {
        let mut data = fixture_hex("/onboard/sectors/0003");
        let last = DPI_OFFSET + 4 * 2;
        data[last..last + 2].copy_from_slice(&[0, 0]);
        data[1] = 4;
        assert_eq!(outcome_of(&with_crc(data), "dpi"), Outcome::Warn);
    }

    #[test]
    fn a_report_interval_that_is_no_rate_fails() {
        let mut data = fixture_hex("/onboard/sectors/0003");
        data[0] = 0;
        assert_eq!(outcome_of(&with_crc(data), "report_rate"), Outcome::Fail);
    }

    #[test]
    fn a_binding_of_an_unknown_kind_is_a_warning() {
        let mut data = fixture_hex("/onboard/sectors/0003");
        data[BUTTON_OFFSET..BUTTON_OFFSET + BINDING_LEN].copy_from_slice(&[0x42, 1, 2, 3]);
        assert_eq!(outcome_of(&with_crc(data), "bindings"), Outcome::Warn);
    }

    #[test]
    fn a_stale_checksum_fails_the_round_trip() {
        let mut data = fixture_hex("/onboard/sectors/0003");
        data[0] = 2;
        assert_eq!(outcome_of(&data, "round_trip"), Outcome::Fail);
    }

    #[test]
    fn the_mouse_running_other_settings_is_a_warning() {
        let data = fixture_hex("/onboard/sectors/0003");
        let mut elsewhere = info();
        elsewhere.dpi = 1000;
        elsewhere.report_rate_hz = Some(500);
        let found = outcomes(&data, &elsewhere, true);
        assert!(found.contains(&("live_dpi", Outcome::Warn)), "{found:?}");
        assert!(
            found.contains(&("live_report_rate", Outcome::Warn)),
            "{found:?}"
        );
    }
}
