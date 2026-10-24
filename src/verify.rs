//! `omalogi verify`: the hardware self-test, built into the helper.
//!
//! It drives an [`omalogi serve`](crate::serve) server in process with the overlay's own
//! requests, so every edit the overlay can make is checked against what the mouse reads
//! back. It runs on a spare profile slot, which it turns on and names "Omalogi Test", and
//! bindings only change while that slot is not in use; while it is, only the DPI, the
//! report rate and one button change, so the mouse stays usable. All profile memory is
//! backed up first. At the end, every other sector must be as it was, and restoring the
//! backup must put all of it back byte for byte.
//!
//! The caller holds the device lock for the whole run, so the daemon cannot switch
//! profiles in the middle of it.

use std::{
    collections::BTreeMap,
    error::Error,
    path::PathBuf,
    time::{Duration, Instant},
};

use serde::Serialize;
use serde_json::{Value, json};
use tokio::io::{
    AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, Lines, duplex, split,
};

use crate::{
    check::Outcome,
    consent::Support,
    device::{Firmware, OnboardState, Session},
    editing::{BackupFile, EditError, save_backup},
    hidraw::WIRELESS_DEVICES,
    onboard::action,
    serve::{BackupPath, Server},
};

/// The name the test profile is given while the test runs.
pub const TEST_PROFILE_NAME: &str = "Omalogi Test";

const KEYS: &[&str] = &[
    "key:a",
    "key:z",
    "key:1",
    "key:enter",
    "key:esc",
    "key:tab",
    "key:space",
    "key:minus",
    "key:f13",
    "key:f24",
    "key:up",
    "key:ctrl+t",
    "key:ctrl+shift+t",
    "key:alt+tab",
    "key:super+enter",
    "key:ctrl+shift+alt+super+f13",
];
const BUTTONS: &[&str] = &["button:6", "button:16"];

#[derive(Debug, Clone, Default)]
pub struct VerifyOptions {
    /// The profile slot to test on (1-based). By default the first one that is turned off.
    pub profile: Option<usize>,
}

/// Requests of one kind the self-test sent, and how long they took.
#[derive(Debug, Clone, Serialize)]
pub struct Timing {
    pub request: String,
    pub count: usize,
    pub median_ms: u64,
    pub max_ms: u64,
}

/// The result of `omalogi verify`. Like [`crate::check::CheckReport`] it holds no unit ID
/// and no profile memory.
#[derive(Debug, Clone, Serialize)]
pub struct VerifyReport {
    pub device: &'static str,
    pub vendor_id: u16,
    pub product_id: u16,
    pub wireless: bool,
    pub firmware: Vec<Firmware>,
    pub support: Support,
    /// The profile slot the test ran on (1-based).
    pub profile: usize,
    pub outcome: Outcome,
    pub passed: usize,
    pub failures: Vec<String>,
    pub notes: Vec<String>,
    pub seconds: u64,
    pub timings: Vec<Timing>,
    /// The backup made before anything was written.
    pub backup: PathBuf,
}

impl VerifyReport {
    /// The active firmware version, as `omalogi info` shows it.
    #[must_use]
    pub fn active_firmware(&self) -> Option<&str> {
        self.firmware
            .iter()
            .find(|firmware| firmware.active)
            .map(|firmware| firmware.version.as_str())
    }

    /// The Mouse report form, prefilled with this report. Like the overlay's link, it holds
    /// the model, its product id and firmware version only.
    #[must_use]
    pub fn issue_url(&self) -> String {
        let mut results = format!(
            "`omalogi verify` {}: {} checks passed, {} failed in {} s, on profile {}.",
            env!("CARGO_PKG_VERSION"),
            self.passed,
            self.failures.len(),
            self.seconds,
            self.profile
        );
        for note in &self.notes {
            results.push_str(&format!("\nnote: {note}"));
        }
        // A link has room for a summary, not every failure; the terminal shows them all.
        for failure in self.failures.iter().take(10) {
            results.push_str(&format!("\nFAIL {failure}"));
        }
        if self.failures.len() > 10 {
            results.push_str(&format!("\n… and {} more", self.failures.len() - 10));
        }
        let mut fields = vec![
            ("template", "mouse.yml".to_owned()),
            ("title", format!("[Mouse]: {}", self.device)),
            (
                "goal",
                "Verify it (I will run the self-test, `omalogi verify`)".to_owned(),
            ),
            ("model", format!("Logitech {}", self.device)),
            (
                "device_id",
                format!("{:04x}:{:04x}", self.vendor_id, self.product_id),
            ),
            (
                "connection",
                if self.wireless {
                    "Through a receiver (LIGHTSPEED, Bolt or Unifying)"
                } else {
                    "Wired"
                }
                .to_owned(),
            ),
            ("results", results),
        ];
        if let Some(firmware) = self.active_firmware() {
            fields.push(("firmware", firmware.to_owned()));
        }
        let query: Vec<String> = fields
            .iter()
            .map(|(key, value)| format!("{key}={}", url_encode(value)))
            .collect();
        format!(
            "https://github.com/elberacasa/omalogi/issues/new?{}",
            query.join("&")
        )
    }
}

fn url_encode(text: &str) -> String {
    text.bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                char::from(byte).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

/// Picks the profile slot to test on: `requested`, or the first one that is turned off.
/// The profile in use is never picked, since the test switches away from it and back.
pub fn pick_profile(onboard: &OnboardState, requested: Option<usize>) -> Result<usize, String> {
    let count = onboard.profiles.len();
    match requested {
        Some(number) => {
            let slot = number
                .checked_sub(1)
                .and_then(|position| onboard.profiles.get(position))
                .ok_or_else(|| format!("profile {number} does not exist; the mouse has {count}"))?;
            if slot.active {
                return Err(format!(
                    "profile {number} is in use; pick a profile that is not, or switch to \
                     another one first"
                ));
            }
            Ok(number)
        }
        None => onboard
            .profiles
            .iter()
            .find(|slot| !slot.enabled && !slot.active)
            .map(|slot| slot.position + 1)
            .ok_or_else(|| {
                "every profile is turned on, so there is no spare slot to test on: pick one \
                 with --profile N. It is put back byte for byte at the end"
                    .to_owned()
            }),
    }
}

/// Runs the self-test on the connected mouse. `progress` gets a line as each phase starts.
pub async fn verify(
    mut session: Session,
    options: &VerifyOptions,
    backup_path: BackupPath,
    progress: &mut (dyn FnMut(&str) + Send),
) -> Result<VerifyReport, Box<dyn Error>> {
    let check = session.check().await?;
    if check.outcome == Outcome::Fail
        || check
            .findings
            .iter()
            .any(|finding| finding.check == "directory" && finding.outcome != Outcome::Pass)
    {
        return Err(
            "`omalogi check` does not pass, so the self-test will not write to this mouse; \
             run `omalogi check` to see why"
                .into(),
        );
    }
    let info = session.info().await?;
    let mut support = session.support().await?;
    let onboard = session.onboard().await?;
    let number = pick_profile(&onboard, options.profile)?;
    let model = session.model();

    let start = session.backup().await?;
    let backup = backup_path(model.name)?;
    save_backup(&start, &backup)?;
    progress(&format!(
        "Backed up all profile memory to {}. If the test stops part way, \
         `omalogi restore {}` puts everything back.",
        backup.display(),
        backup.display()
    ));

    let mut tally = Tally::default();
    if !support.accepted {
        session.accept_untested().await?;
        tally.note("accepted editing this untested model, as running the test asks");
        support.accepted = true;
    }

    let started = Instant::now();
    let mut server = Server::new(session, None, backup_path);
    let (client_side, server_side) = duplex(1 << 20);
    let (server_read, server_write) = split(server_side);
    let (client_read, client_write) = split(client_side);
    let script = async {
        let mut test = SelfTest::new(client_read, client_write, number, tally, progress);
        test.run_all().await;
        (test.tally, test.client.timings())
        // The client is dropped here, which ends the server.
    };
    let (served, (mut tally, timings)) = tokio::join!(
        server.run(BufReader::new(server_read), server_write),
        script
    );
    served?;
    let mut session = server.into_session();

    // The test only writes the directory and its own slot. Everything else must be as it
    // was, then restoring the backup puts those two back too.
    progress("Putting profile memory back");
    let test_sector = onboard.profiles[number - 1].sector;
    let end = session.backup().await?;
    let stray: Vec<&String> = start
        .sectors
        .keys()
        .chain(end.sectors.keys())
        .filter(|sector| {
            *sector != "0000"
                && **sector != format!("{test_sector:04x}")
                && start.sectors.get(*sector) != end.sectors.get(*sector)
        })
        .collect();
    tally.check(
        "no sector besides the directory and the test profile changed",
        stray.is_empty(),
        format!("sectors {stray:?} differ"),
    );
    match session
        .restore(&BackupFile::load(&backup)?, &backup_path(model.name)?)
        .await
    {
        Ok(_) | Err(EditError::AlreadyRestored) => {}
        Err(error) => tally.check("restoring the backup", false, error.to_string()),
    }
    let restored = session.backup().await?;
    tally.check(
        "profile memory is byte for byte as it was",
        restored.sectors == start.sectors,
        "a fresh backup differs from the one made first",
    );

    Ok(VerifyReport {
        device: model.name,
        vendor_id: info.vendor_id,
        product_id: info.product_id,
        wireless: WIRELESS_DEVICES
            .iter()
            .any(|device| device.product_id == model.product_id),
        firmware: info.firmware,
        support,
        profile: number,
        outcome: if tally.failures.is_empty() {
            Outcome::Pass
        } else {
            Outcome::Fail
        },
        passed: tally.passed,
        failures: tally.failures,
        notes: tally.notes,
        seconds: started.elapsed().as_secs(),
        timings,
        backup,
    })
}

#[derive(Debug, Default)]
struct Tally {
    passed: usize,
    failures: Vec<String>,
    notes: Vec<String>,
}

impl Tally {
    fn check(&mut self, name: impl AsRef<str>, ok: bool, detail: impl std::fmt::Display) {
        if ok {
            self.passed += 1;
        } else {
            self.failures.push(format!("{}: {detail}", name.as_ref()));
        }
    }

    fn note(&mut self, text: impl Into<String>) {
        self.notes.push(text.into());
    }
}

/// The overlay's side of an in-process `omalogi serve`.
struct Client<R, W> {
    lines: Lines<BufReader<R>>,
    writer: W,
    last_id: u64,
    times: BTreeMap<String, Vec<Duration>>,
}

impl<R: AsyncRead + Unpin, W: AsyncWrite + Unpin> Client<R, W> {
    async fn request(&mut self, cmd: &str, fields: Value) -> Result<Value, Box<dyn Error>> {
        self.last_id += 1;
        let mut request = json!({ "id": self.last_id, "cmd": cmd });
        if let (Value::Object(request), Value::Object(fields)) = (&mut request, fields) {
            request.extend(fields);
        }
        let started = Instant::now();
        let mut line = request.to_string();
        line.push('\n');
        self.writer.write_all(line.as_bytes()).await?;
        self.writer.flush().await?;
        let reply = self
            .lines
            .next_line()
            .await?
            .ok_or("the server stopped answering")?;
        self.times
            .entry(cmd.to_owned())
            .or_default()
            .push(started.elapsed());
        let response: Value = serde_json::from_str(&reply)?;
        if response["id"] != json!(self.last_id) {
            return Err(format!("a reply to another request: {response}").into());
        }
        Ok(response)
    }

    async fn result(&mut self, cmd: &str, fields: Value) -> Result<Value, Box<dyn Error>> {
        let response = self.request(cmd, fields).await?;
        if response["ok"] != json!(true) {
            return Err(format!("{cmd} failed: {}", response["error"]).into());
        }
        Ok(response["result"].clone())
    }

    fn timings(&self) -> Vec<Timing> {
        self.times
            .iter()
            .map(|(request, times)| {
                let mut ms: Vec<u64> = times
                    .iter()
                    .map(|time| u64::try_from(time.as_millis()).unwrap_or(u64::MAX))
                    .collect();
                ms.sort_unstable();
                Timing {
                    request: request.clone(),
                    count: ms.len(),
                    median_ms: ms[ms.len() / 2],
                    max_ms: ms[ms.len() - 1],
                }
            })
            .collect()
    }
}

struct SelfTest<'p, R, W> {
    client: Client<R, W>,
    tally: Tally,
    progress: &'p mut (dyn FnMut(&str) + Send),
    number: usize,
    position: usize,
    last_undo: u64,
}

impl<'p, R: AsyncRead + Unpin, W: AsyncWrite + Unpin> SelfTest<'p, R, W> {
    fn new(
        read: R,
        writer: W,
        number: usize,
        tally: Tally,
        progress: &'p mut (dyn FnMut(&str) + Send),
    ) -> Self {
        Self {
            client: Client {
                lines: BufReader::new(read).lines(),
                writer,
                last_id: 0,
                times: BTreeMap::new(),
            },
            tally,
            progress,
            number,
            position: number - 1,
            last_undo: 0,
        }
    }

    /// Sets the test profile up, runs every phase, and switches back to the profile that
    /// was in use, whatever happened.
    async fn run_all(&mut self) {
        let home = match self.set_up().await {
            Ok(home) => home,
            Err(error) => {
                self.tally
                    .check("setting up the test profile", false, error.to_string());
                return;
            }
        };
        if let Err(error) = self.run(home).await {
            self.tally
                .check("the self-test ran to the end", false, error.to_string());
        }
        if let Err(error) = self
            .client
            .result("activate", json!({ "profile": home }))
            .await
        {
            self.tally
                .check(format!("activating profile {home} again"), false, error);
        }
    }

    /// Turns the test profile on and names it. Answers the profile in use.
    async fn set_up(&mut self) -> Result<usize, Box<dyn Error>> {
        let state = self.client.result("state", json!({})).await?;
        let onboard = &state["onboard"];
        let home = onboard["active_position"]
            .as_u64()
            .ok_or("the mouse reports no profile in use")?;
        if onboard["profiles"][self.position]["enabled"] != json!(true) {
            self.client
                .result(
                    "set_enabled",
                    json!({ "profile": self.number, "enabled": true }),
                )
                .await?;
        }
        self.client
            .result(
                "apply",
                json!({ "profile": self.number, "name": TEST_PROFILE_NAME }),
            )
            .await?;
        Ok(usize::try_from(home)? + 1)
    }

    fn written(&mut self, response: &Value, name: &str) -> Option<Value> {
        if response["ok"] != json!(true) {
            self.tally.check(name, false, &response["error"]);
            return None;
        }
        let result = response["result"].clone();
        if !result["takes_effect"].is_null() {
            self.last_undo = result["undo"].as_u64().unwrap_or(0);
        }
        Some(result)
    }

    async fn apply(&mut self, fields: Value, name: &str) -> Result<Option<Value>, Box<dyn Error>> {
        let mut fields = fields;
        fields["profile"] = json!(self.number);
        let response = self.client.request("apply", fields).await?;
        Ok(self.written(&response, name))
    }

    async fn slot(&mut self) -> Result<Value, Box<dyn Error>> {
        let state = self.client.result("state", json!({})).await?;
        Ok(state["onboard"]["profiles"][self.position].clone())
    }

    async fn reread_matches(&mut self, result: &Value, name: &str) -> Result<(), Box<dyn Error>> {
        let slot = self.slot().await?;
        self.tally.check(
            format!("{name}: a fresh read matches the write"),
            slot["profile"] == result["slot"]["profile"]
                && slot["actions"] == result["slot"]["actions"],
            "the mouse holds something else",
        );
        Ok(())
    }

    /// The sensor's live DPI. Right after loading a profile the mouse can miss one request
    /// (a USB timeout, seen once on a G502 X), so a read is tried again briefly before it
    /// counts as a failure. Only reads are retried; writes never are.
    async fn live_dpi(&mut self) -> Result<Value, Box<dyn Error>> {
        let mut attempt = 0;
        loop {
            match self.client.result("live", json!({})).await {
                Ok(live) => return Ok(live["dpi"].clone()),
                Err(error) if attempt == 2 => return Err(error),
                Err(_) => {
                    attempt += 1;
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
            }
        }
    }

    async fn live_rate(&mut self) -> Result<Value, Box<dyn Error>> {
        let state = self.client.result("state", json!({})).await?;
        Ok(state["info"]["report_rate_hz"].clone())
    }

    #[allow(clippy::too_many_lines)]
    async fn run(&mut self, home: usize) -> Result<(), Box<dyn Error>> {
        let state = self.client.result("state", json!({})).await?;
        let onboard = state["onboard"].clone();
        let info = state["info"].clone();
        let number = self.number;
        let slot = onboard["profiles"][self.position].clone();
        let count = onboard["description"]["button_count"]
            .as_u64()
            .and_then(|count| usize::try_from(count).ok())
            .ok_or("the mouse reports no button count")?;
        let profile_count = onboard["description"]["profile_count"]
            .as_u64()
            .unwrap_or(5);
        let home_profile = onboard["profiles"][home - 1]["profile"].clone();
        let snapshot = slot["profile"].clone();
        let snapshot_actions = slot["actions"].clone();

        // 1. Every action on every slot of both layers. Each write rotates the whole list
        // one step, so N writes put each of the N actions on each slot once.
        (self.progress)(&format!(
            "[1/6] bindings: every action on each of {count} slots, both layers"
        ));
        let mut actions: Vec<String> = action::catalog()
            .into_iter()
            .map(|action| action.value)
            .filter(|value| value != "key:")
            .collect();
        actions.extend(KEYS.iter().chain(BUTTONS).map(|value| (*value).to_owned()));
        // Slots stored with bytes their own text cannot spell are left as they are, as the
        // overlay leaves them: another action would overwrite those bytes, and their own
        // action writes nothing back. The two layers are tracked apart.
        let kept_buttons =
            verbatim_slots(&snapshot["buttons"], &snapshot_actions["buttons"], count);
        let kept_gshift = verbatim_slots(
            &snapshot["gshift_buttons"],
            &snapshot_actions["gshift_buttons"],
            count,
        );
        for (layer, kept) in [("default", &kept_buttons), ("G-Shift", &kept_gshift)] {
            if !kept.is_empty() {
                self.tally.note(format!(
                    "{layer} slots {kept:?} stay as the mouse stores them"
                ));
            }
        }
        for step in 0..actions.len() {
            let table = |offset: usize, kept: &[usize]| -> serde_json::Map<String, Value> {
                (0..count)
                    .filter(|slot| !kept.contains(slot))
                    .map(|slot| {
                        (
                            slot.to_string(),
                            json!(actions[(step + slot + offset) % actions.len()]),
                        )
                    })
                    .collect()
            };
            let tables = [
                ("buttons", table(0, &kept_buttons)),
                ("gshift_buttons", table(count, &kept_gshift)),
            ];
            let name = format!("bindings write {step}");
            let Some(result) = self
                .apply(
                    json!({ "buttons": tables[0].1, "gshift": tables[1].1 }),
                    &name,
                )
                .await?
            else {
                continue;
            };
            let when = &result["takes_effect"]["state"];
            self.tally.check(
                format!("{name} waits for activation"),
                when == "when_activated",
                when,
            );
            for (layer, table) in &tables {
                for (slot, want) in table {
                    let index: usize = slot.parse()?;
                    let have = &result["slot"]["actions"][layer][index];
                    self.tally.check(
                        format!("{layer}[{slot}] = {want}"),
                        have == want,
                        format!("read back {have}"),
                    );
                    if want != "disabled" {
                        let label = &result["slot"]["labels"][layer][index];
                        self.tally.check(
                            format!("{layer}[{slot}] {want} has a label"),
                            label.as_str().is_some_and(|label| !label.is_empty()),
                            "no label",
                        );
                    }
                }
            }
            if step % 10 == 9 {
                self.reread_matches(&result, &name).await?;
            }
        }
        let probe = self
            .client
            .request(
                "apply",
                json!({ "profile": number, "buttons": { count.to_string(): "key:a" } }),
            )
            .await?;
        if probe["ok"] == json!(true) {
            self.tally.note(format!(
                "slot {count} (past the {count} buttons) accepts a binding"
            ));
            self.client.result("undo", json!({})).await?;
        } else {
            self.tally.note(format!(
                "slot {count} (past the {count} buttons): {}",
                probe["error"]
            ));
        }

        // 2. Refusals write nothing: the next unchanged apply reports the same undo depth.
        (self.progress)("[2/6] refusals");
        let refusals = [
            ("an unknown action", json!({ "buttons": { "0": "bogus" } })),
            ("an empty key", json!({ "buttons": { "0": "key:" } })),
            (
                "an unknown key",
                json!({ "buttons": { "0": "key:nosuchkey" } }),
            ),
            ("button:0", json!({ "buttons": { "0": "button:0" } })),
            ("button:17", json!({ "buttons": { "0": "button:17" } })),
            (
                "an unknown media key",
                json!({ "gshift": { "0": "media:nope" } }),
            ),
            ("slot 16", json!({ "buttons": { "16": "left" } })),
            (
                "a slot that is not a number",
                json!({ "buttons": { "x": "left" } }),
            ),
            ("no DPI stages", json!({ "dpi": [] })),
            ("50 DPI", json!({ "dpi": [50], "default_dpi": 50 })),
            ("30000 DPI", json!({ "dpi": [30000], "default_dpi": 30000 })),
            (
                "an unsupported DPI step",
                json!({ "dpi": [123], "default_dpi": 123 }),
            ),
            (
                "six DPI stages",
                json!({ "dpi": [400, 800, 1600, 2400, 3200, 6400] }),
            ),
            (
                "a default DPI that is not a stage",
                json!({ "default_dpi": 999 }),
            ),
            ("333 Hz", json!({ "rate": 333 })),
            ("a 48 character name", json!({ "name": "x".repeat(48) })),
            ("a name that is not ASCII", json!({ "name": "Ömalogi" })),
        ];
        for (name, mut changes) in refusals {
            changes["profile"] = json!(number);
            let response = self.client.request("apply", changes).await?;
            self.tally.check(
                format!("refuses {name}"),
                response["ok"] != json!(true),
                "was accepted",
            );
            if response["ok"] == json!(true) && !response["result"]["takes_effect"].is_null() {
                self.client.result("undo", json!({})).await?;
            }
        }
        let missing = profile_count + 1;
        for (cmd, fields, name) in [
            ("activate", json!({ "profile": missing }), "activating"),
            (
                "set_enabled",
                json!({ "profile": missing, "enabled": true }),
                "turning on",
            ),
            (
                "apply",
                json!({ "profile": missing, "rate": 1000 }),
                "editing",
            ),
        ] {
            let response = self.client.request(cmd, fields).await?;
            self.tally.check(
                format!("refuses {name} profile {missing}"),
                response["ok"] != json!(true),
                "was accepted",
            );
        }

        let current = self.slot().await?["actions"]["buttons"][0].clone();
        if current.is_string() {
            let unchanged = self
                .client
                .result(
                    "apply",
                    json!({ "profile": number, "buttons": { "0": current } }),
                )
                .await?;
            self.tally.check(
                "an unchanged apply writes nothing",
                unchanged["takes_effect"].is_null(),
                &unchanged,
            );
            self.tally.check(
                "refusals left the undo depth alone",
                unchanged["undo"].as_u64() == Some(self.last_undo),
                format!("{} after {}", unchanged["undo"], self.last_undo),
            );
        } else {
            self.tally
                .note("button 0 has no action text: the unchanged apply check is skipped");
        }
        // A slot stored with a tail its text cannot spell keeps it when its own action is
        // sent back, so this is where such a mouse checks the round trip.
        for (layer, snapshot_layer, kept) in [
            ("buttons", "buttons", &kept_buttons),
            ("gshift", "gshift_buttons", &kept_gshift),
        ] {
            for slot in kept {
                let text = &snapshot_actions[snapshot_layer][*slot];
                if !text.is_string() {
                    continue;
                }
                let again = self
                    .client
                    .result(
                        "apply",
                        json!({ "profile": number, layer: { slot.to_string(): text } }),
                    )
                    .await?;
                self.tally.check(
                    format!("{layer}[{slot}] = {text} writes nothing back"),
                    again["takes_effect"].is_null(),
                    &again,
                );
            }
        }
        if let Some(result) = self
            .apply(
                json!({ "buttons": { "0": "key:shift+ctrl+t" } }),
                "modifier order",
            )
            .await?
        {
            let have = &result["slot"]["actions"]["buttons"][0];
            self.tally.check(
                "modifiers read back in canonical order",
                have == "key:ctrl+shift+t",
                have,
            );
        }

        // 3. DPI stage lists, default and shift stages, and every report rate.
        (self.progress)("[3/6] sensitivity: DPI stages, default and shift stages, report rates");
        let rates: Vec<u64> = info["report_rates_hz"]
            .as_array()
            .map(|rates| rates.iter().filter_map(Value::as_u64).collect())
            .unwrap_or_default();
        let values: Vec<u64> = info["dpi_values"]
            .as_array()
            .map(|values| values.iter().filter_map(Value::as_u64).collect())
            .unwrap_or_default();
        if rates.is_empty() || values.is_empty() {
            return Err("the mouse lists no report rates or DPI values".into());
        }
        let mut case = 0;
        let mut last = None;
        for stages in dpi_cases(&values) {
            let defaults: Vec<u64> = if stages.len() <= 3 {
                stages.clone()
            } else {
                vec![stages[0], stages[stages.len() - 1]]
            };
            for default in defaults {
                let shift = stages
                    .iter()
                    .copied()
                    .find(|dpi| *dpi != default)
                    .unwrap_or(default);
                let rate = rates[case % rates.len()];
                case += 1;
                let name = format!("{stages:?} default {default} shift {shift} at {rate} Hz");
                let Some(result) = self
                    .apply(
                        json!({ "dpi": stages, "default_dpi": default, "shift_dpi": shift,
                                "rate": rate }),
                        &name,
                    )
                    .await?
                else {
                    continue;
                };
                let profile = &result["slot"]["profile"];
                let got = sensitivity(profile);
                self.tally.check(
                    &name,
                    got == Some((stages.clone(), default, shift, rate)),
                    format!("read back {got:?}"),
                );
                last = Some(result);
            }
        }
        if let Some(result) = last {
            self.reread_matches(&result, "sensitivity").await?;
        }
        let unsorted = self
            .client
            .request(
                "apply",
                json!({ "profile": number, "dpi": [1600, 800], "default_dpi": 800,
                        "shift_dpi": 1600 }),
            )
            .await?;
        if unsorted["ok"] == json!(true) {
            self.tally.note(format!(
                "unsorted stages read back as {}",
                unsorted["result"]["slot"]["profile"]["dpi_stages"]
            ));
        } else {
            self.tally
                .note(format!("unsorted stages: {}", unsorted["error"]));
        }

        // 4. Names.
        (self.progress)("[4/6] names");
        for name in ["Selftest 0123 ~!", &"x".repeat(47), ""] {
            if let Some(result) = self
                .apply(json!({ "name": name }), &format!("name {name:?}"))
                .await?
            {
                let have = &result["slot"]["profile"]["name"];
                let want = if name.is_empty() {
                    Value::Null
                } else {
                    json!(name)
                };
                self.tally.check(
                    format!("name {name:?}"),
                    *have == want,
                    format!("read back {have}"),
                );
            }
        }

        // 5. Undo walks back through each write, and the snapshot comes back in one write.
        (self.progress)("[5/6] undo and restore");
        let before = self.slot().await?;
        let mut history = vec![(before["profile"].clone(), before["actions"].clone())];
        for changes in [
            json!({ "dpi": [800, 1600], "default_dpi": 800, "shift_dpi": 1600 }),
            json!({ "buttons": { "3": "key:f13" } }),
            json!({ "gshift": { "2": "media:mute" } }),
        ] {
            let name = format!("undo setup {changes}");
            if let Some(result) = self.apply(changes, &name).await? {
                history.push((
                    result["slot"]["profile"].clone(),
                    result["slot"]["actions"].clone(),
                ));
            }
        }
        for index in (0..history.len() - 1).rev() {
            let result = self.client.result("undo", json!({})).await?;
            let have = (
                result["slot"]["profile"].clone(),
                result["slot"]["actions"].clone(),
            );
            self.tally.check(
                format!("undo back to step {index}"),
                have == history[index],
                "did not match",
            );
        }
        let result = self
            .apply(
                changes_for(&snapshot, &snapshot_actions, count),
                "restore the snapshot",
            )
            .await?;
        let restored = result.as_ref().is_some_and(|result| {
            result["slot"]["profile"] == snapshot && result["slot"]["actions"] == snapshot_actions
        });
        self.tally.check(
            "the snapshot comes back in one write",
            restored,
            "did not match",
        );
        if !restored {
            self.tally
                .note("skipped the in-use checks: the test profile is not back to its snapshot");
            return Ok(());
        }

        // 6. In use: every change is loaded right away, and undo loads the old settings back.
        (self.progress)("[6/6] in use: live reload, undo, turning profiles on and off");
        let stages = stage_values(&snapshot);
        let default = &snapshot["dpi_stages"][index_of(&snapshot["default_dpi_index"])];
        self.client
            .result("activate", json!({ "profile": number }))
            .await?;
        let active =
            self.client.result("state", json!({})).await?["onboard"]["active_position"].clone();
        self.tally
            .check("activation", active == json!(self.position), &active);
        let live = self.live_dpi().await?;
        self.tally
            .check("live DPI after activating", &live == default, &live);

        let snapshot_rate = rate_hz(&snapshot);
        let other_rate = if snapshot_rate == Some(500) {
            1000
        } else {
            500
        };
        let other_dpi = stages.iter().copied().find(|dpi| json!(dpi) != *default);
        let mut in_use: Vec<(Value, String, Live)> = Vec::new();
        if let Some(other) = other_dpi {
            in_use.push((
                json!({ "default_dpi": other }),
                format!("default DPI {other}"),
                Live::Dpi(other),
            ));
        } else {
            self.tally
                .note("single DPI stage: the default DPI switch live check is skipped");
        }
        in_use.push((
            json!({ "rate": other_rate }),
            format!("report rate {other_rate} Hz"),
            Live::Rate(other_rate),
        ));
        let harmless = count.min(11) - 1;
        in_use.push((
            json!({ "buttons": { harmless.to_string(): "key:f13" } }),
            format!("button {harmless} as F13"),
            Live::Nothing,
        ));
        let mut undone = 0;
        for (changes, name, live) in in_use {
            if let Some(result) = self.apply(changes, &name).await? {
                let when = &result["takes_effect"]["state"];
                self.tally
                    .check(format!("{name} is loaded right away"), when == "now", when);
                let (ok, read) = match live {
                    Live::Dpi(dpi) => {
                        let read = self.live_dpi().await?;
                        (read == json!(dpi), read)
                    }
                    Live::Rate(hz) => {
                        let read = self.live_rate().await?;
                        (read == json!(hz), read)
                    }
                    Live::Nothing => (true, Value::Null),
                };
                self.tally.check(
                    format!("{name} is live"),
                    ok,
                    format!("the mouse still reads {read}"),
                );
                undone += 1;
            }
        }
        for _ in 0..undone {
            let result = self.client.result("undo", json!({})).await?;
            let when = &result["takes_effect"]["state"];
            self.tally
                .check("undo in use is loaded right away", when == "now", when);
        }
        let live = self.live_dpi().await?;
        self.tally
            .check("live DPI after undo", &live == default, &live);
        let rate = self.live_rate().await?;
        self.tally.check(
            "report rate after undo",
            rate.as_u64() == snapshot_rate,
            &rate,
        );

        let response = self
            .client
            .request(
                "set_enabled",
                json!({ "profile": number, "enabled": false }),
            )
            .await?;
        self.tally.check(
            "refuses turning off the profile in use",
            response["ok"] != json!(true),
            "was accepted",
        );
        let off = onboard["profiles"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|profile| profile["enabled"] == json!(false))
            .and_then(|profile| profile["position"].as_u64())
            .map(|position| position + 1);
        match off {
            None => self
                .tally
                .note("every profile is turned on: the off-profile toggle checks are skipped"),
            Some(off) => {
                let response = self
                    .client
                    .request("set_enabled", json!({ "profile": off, "enabled": false }))
                    .await?;
                self.tally.check(
                    "refuses turning off a profile that is off",
                    response["ok"] != json!(true),
                    "was accepted",
                );
                for enabled in [true, false] {
                    let result = self
                        .client
                        .result("set_enabled", json!({ "profile": off, "enabled": enabled }))
                        .await?;
                    let have = &result["onboard"]["profiles"][index_of(&json!(off - 1))]["enabled"];
                    self.tally.check(
                        format!(
                            "profile {off} turned {}",
                            if enabled { "on" } else { "off" }
                        ),
                        *have == json!(enabled),
                        have,
                    );
                }
            }
        }

        self.client
            .result("activate", json!({ "profile": home }))
            .await?;
        let home_default =
            &home_profile["dpi_stages"][index_of(&home_profile["default_dpi_index"])];
        let live = self.live_dpi().await?;
        self.tally.check(
            "live DPI back on the home profile",
            &live == home_default,
            &live,
        );
        Ok(())
    }
}

/// What to read from the mouse to see that a change in use is live.
enum Live {
    Dpi(u64),
    Rate(u64),
    Nothing,
}

fn index_of(value: &Value) -> usize {
    value
        .as_u64()
        .and_then(|index| usize::try_from(index).ok())
        .unwrap_or(usize::MAX)
}

fn rate_hz(profile: &Value) -> Option<u64> {
    profile["report_rate_ms"]
        .as_u64()
        .filter(|ms| *ms > 0)
        .map(|ms| (1000 + ms / 2) / ms)
}

fn stage_values(profile: &Value) -> Vec<u64> {
    profile["dpi_stages"]
        .as_array()
        .map(|stages| stages.iter().filter_map(Value::as_u64).collect())
        .unwrap_or_default()
}

/// A profile's stages, default and shift DPI, and report rate, as the test compares them.
fn sensitivity(profile: &Value) -> Option<(Vec<u64>, u64, u64, u64)> {
    let stages = &profile["dpi_stages"];
    Some((
        stage_values(profile),
        stages[index_of(&profile["default_dpi_index"])].as_u64()?,
        stages[index_of(&profile["shift_dpi_index"])].as_u64()?,
        rate_hz(profile)?,
    ))
}

/// Whether a binding is stored with bytes its own text cannot spell: a firmware action
/// whose reserved or profile byte is nonzero.
fn opaque_tail(binding: &Value) -> bool {
    binding["type"] == "special"
        && (binding["reserved"].as_u64().unwrap_or(0) != 0
            || binding["profile"].as_u64().unwrap_or(0) != 0)
}

/// The slots the test must not rotate: bindings with no text at all, and firmware actions
/// stored with a tail their text cannot reproduce.
fn verbatim_slots(bindings: &Value, actions: &Value, count: usize) -> Vec<usize> {
    (0..count)
        .filter(|slot| actions[*slot].is_null() || opaque_tail(&bindings[*slot]))
        .collect()
}

/// The changes that write `profile` back as it was.
fn changes_for(profile: &Value, actions: &Value, count: usize) -> Value {
    let stages = &profile["dpi_stages"];
    let layer = |name: &str| -> serde_json::Map<String, Value> {
        (0..count)
            .filter(|slot| actions[name][*slot].is_string())
            .map(|slot| (slot.to_string(), actions[name][slot].clone()))
            .collect()
    };
    let mut changes = json!({
        "dpi": stage_values(profile),
        "default_dpi": stages[index_of(&profile["default_dpi_index"])],
        "rate": rate_hz(profile),
        "name": profile["name"].as_str().unwrap_or(""),
        "buttons": layer("buttons"),
        "gshift": layer("gshift_buttons"),
    });
    let shift = &stages[index_of(&profile["shift_dpi_index"])];
    if !shift.is_null() {
        changes["shift_dpi"] = shift.clone();
    }
    changes
}

/// Stage lists this sensor supports: one to four common levels, its whole range, and an
/// in-between step with its maximum. On a G502 X (100–25600 in steps of 50) these are the
/// lists the self-test always used.
fn dpi_cases(values: &[u64]) -> Vec<Vec<u64>> {
    let mut values = values.to_vec();
    values.sort_unstable();
    values.dedup();
    let near = |target: u64| {
        *values
            .iter()
            .min_by_key(|dpi| dpi.abs_diff(target))
            .expect("values are not empty")
    };
    let sorted_set = |items: Vec<u64>| {
        let mut items = items;
        items.sort_unstable();
        items.dedup();
        items
    };
    let common = sorted_set([400, 800, 1200, 1600, 3200, 6400].map(near).to_vec());
    let (lo, hi) = (values[0], values[values.len() - 1]);
    let mut whole = sorted_set(vec![lo, near(400), near(1600), near(6400), hi]);
    whole.truncate(5);
    let between = sorted_set(vec![values[(values.len() - 1).min(15)], hi]);
    [
        common[..1.min(common.len())].to_vec(),
        common[..2.min(common.len())].to_vec(),
        common[..3.min(common.len())].to_vec(),
        common[..4.min(common.len())].to_vec(),
        whole,
        between,
    ]
    .into_iter()
    .filter(|case| !case.is_empty())
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dpi_cases_match_the_python_self_test_on_a_g502x() {
        let values: Vec<u64> = (2..=512).map(|step| step * 50).collect();
        assert_eq!(
            dpi_cases(&values),
            [
                vec![400],
                vec![400, 800],
                vec![400, 800, 1200],
                vec![400, 800, 1200, 1600],
                vec![100, 400, 1600, 6400, 25600],
                vec![850, 25600],
            ]
        );
    }

    #[test]
    fn the_issue_link_is_prefilled_and_encoded() {
        let report = VerifyReport {
            device: "G502 X",
            vendor_id: 0x046D,
            product_id: 0xC099,
            wireless: false,
            firmware: vec![Firmware {
                kind: "MainApplication".to_owned(),
                version: "U1 60.00.B0009".to_owned(),
                active: true,
            }],
            support: Support {
                name: "G502 X",
                verified: false,
                editable: true,
                accepted: true,
                memory_model: 1,
                profile_format: 4,
            },
            profile: 4,
            outcome: Outcome::Pass,
            passed: 1234,
            failures: Vec::new(),
            notes: Vec::new(),
            seconds: 600,
            timings: Vec::new(),
            backup: PathBuf::from("/tmp/backup.json"),
        };
        let url = report.issue_url();
        assert!(
            url.starts_with("https://github.com/elberacasa/omalogi/issues/new?template=mouse.yml&")
        );
        assert!(url.contains("&device_id=046d%3Ac099&"));
        assert!(url.contains("&connection=Wired&"));
        assert!(url.contains("&firmware=U1%2060.00.B0009"));
        assert!(url.contains("1234%20checks%20passed%2C%200%20failed"));
        assert!(!url.contains("backup"), "the backup path stays local");
    }
}
