//! Mice behind LIGHTSPEED, Bolt and Unifying receivers.
//!
//! Receivers, their paired devices, and the route to each one come from OpenLogi's device
//! layer (`openlogi-hid`, MIT OR Apache-2.0), which already speaks the receiver protocols.
//! Omalogi picks the mice it knows from that inventory and opens a HID++ channel at the
//! mouse's receiver slot; everything above the channel is the same as for a wired mouse.

use std::sync::Arc;

use hidpp::channel::HidppChannel;
use openlogi_core::device::{DeviceInventory, DeviceKind};
use openlogi_hid::DeviceRoute;
use thiserror::Error;

use crate::hidraw::{SupportedDevice, WIRELESS_DEVICES, intern, unlisted};

#[derive(Debug, Error)]
#[error("could not look for wireless mice: {0}")]
pub struct WirelessError(String);

/// A known wireless mouse, online behind a receiver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WirelessMouse {
    pub model: SupportedDevice,
    pub route: DeviceRoute,
    /// Where it was found, e.g. `Lightspeed Receiver, slot 1`.
    pub path: String,
}

/// The online wireless mice Omalogi knows, from an OpenLogi inventory. Mice attached
/// directly are left to the wired path, which finds them faster.
/// Online mice behind receivers that Omalogi lists, by their wireless id.
#[must_use]
pub fn known_mice(inventories: &[DeviceInventory]) -> Vec<WirelessMouse> {
    mice(inventories, true)
}

/// Online mice behind receivers that Omalogi does not list, for a session to ask whether
/// each is a mouse it can edit. Named by the receiver's codename for them until the
/// mouse says its own name.
#[must_use]
pub fn unlisted_mice(inventories: &[DeviceInventory]) -> Vec<WirelessMouse> {
    mice(inventories, false)
}

fn mice(inventories: &[DeviceInventory], listed: bool) -> Vec<WirelessMouse> {
    inventories
        .iter()
        .flat_map(|inventory| {
            inventory.paired.iter().filter_map(move |device| {
                if device.kind != DeviceKind::Mouse || !device.online {
                    return None;
                }
                let mut ids = device.wpid.into_iter().chain(
                    device
                        .model_info
                        .iter()
                        .flat_map(|info| info.model_ids.iter().copied()),
                );
                let known = ids.find_map(|id| {
                    WIRELESS_DEVICES
                        .iter()
                        .find(|model| model.product_id == id)
                        .copied()
                });
                let model = match (known, listed) {
                    (Some(model), true) => model,
                    (None, false) => {
                        let id = device.wpid?;
                        let name = device
                            .codename
                            .as_deref()
                            .map(str::trim)
                            .filter(|name| !name.is_empty())
                            .map_or_else(|| format!("mouse {id:04x}"), str::to_owned);
                        unlisted(id, intern(&name))
                    }
                    _ => return None,
                };
                let route = DeviceRoute::for_slot(inventory, device.slot)?;
                if matches!(route, DeviceRoute::Direct { .. }) {
                    return None;
                }
                Some(WirelessMouse {
                    model,
                    route,
                    path: format!("{}, slot {}", inventory.receiver.name, device.slot),
                })
            })
        })
        .collect()
}

async fn inventories() -> Result<Vec<DeviceInventory>, WirelessError> {
    openlogi_hid::enumerate()
        .await
        .map_err(|error| WirelessError(error.to_string()))
}

pub async fn find() -> Result<Vec<WirelessMouse>, WirelessError> {
    Ok(known_mice(&inventories().await?))
}

/// Opens the channel to `mouse`'s receiver slot.
pub async fn channel(mouse: &WirelessMouse) -> Result<Arc<HidppChannel>, WirelessError> {
    openlogi_hid::channel_pool()
        .open(&mouse.route)
        .await
        .map_err(|error| WirelessError(error.to_string()))?
        .ok_or_else(|| WirelessError(format!("the receiver of the {} is gone", mouse.model.name)))
}

pub async fn open() -> Result<Option<(WirelessMouse, Arc<HidppChannel>)>, WirelessError> {
    let Some(mouse) = find().await?.into_iter().next() else {
        return Ok(None);
    };
    let channel = channel(&mouse).await?;
    Ok(Some((mouse, channel)))
}

/// The unlisted mice behind receivers, without opening them.
pub async fn find_unlisted() -> Result<Vec<WirelessMouse>, WirelessError> {
    Ok(unlisted_mice(&inventories().await?))
}

#[cfg(test)]
mod tests {
    use openlogi_core::device::{PairedDevice, ReceiverInfo};

    use super::*;

    fn paired(slot: u8, wpid: Option<u16>, kind: DeviceKind, online: bool) -> PairedDevice {
        PairedDevice {
            slot,
            codename: None,
            wpid,
            kind,
            online,
            battery: None,
            model_info: None,
            capabilities: None,
            settings_revision: 0,
        }
    }

    fn receiver(
        product_id: u16,
        unique_id: Option<&str>,
        paired: Vec<PairedDevice>,
    ) -> DeviceInventory {
        DeviceInventory {
            receiver: ReceiverInfo {
                name: "Lightspeed Receiver".to_owned(),
                vendor_id: 0x046D,
                product_id,
                unique_id: unique_id.map(str::to_owned),
            },
            paired,
        }
    }

    #[test]
    fn picks_online_known_mice_behind_receivers() {
        let inventories = [receiver(
            0xC547,
            Some("receiver-1"),
            vec![
                paired(1, Some(0x409F), DeviceKind::Mouse, true),
                // Asleep, a keyboard, and a mouse Omalogi does not know.
                paired(2, Some(0x4093), DeviceKind::Mouse, false),
                paired(3, Some(0x407C), DeviceKind::Keyboard, true),
                paired(4, Some(0xB023), DeviceKind::Mouse, true),
            ],
        )];
        let mice = known_mice(&inventories);
        assert_eq!(mice.len(), 1);
        assert_eq!(mice[0].model.name, "G502 X Lightspeed");
        assert_eq!(mice[0].route.device_index(), 1);
        assert!(matches!(mice[0].route, DeviceRoute::Unifying { .. }));
        assert_eq!(mice[0].path, "Lightspeed Receiver, slot 1");
    }

    #[test]
    fn unlisted_mice_are_the_online_mice_omalogi_does_not_list() {
        let mut named = paired(4, Some(0x40A9), DeviceKind::Mouse, true);
        named.codename = Some("PRO X 2".to_owned());
        let inventories = [receiver(
            0xC547,
            Some("receiver-1"),
            vec![
                // Listed, asleep, a keyboard, then two that are not listed.
                paired(1, Some(0x409F), DeviceKind::Mouse, true),
                paired(2, Some(0x40AA), DeviceKind::Mouse, false),
                paired(3, Some(0x407C), DeviceKind::Keyboard, true),
                named,
                paired(5, Some(0x40AB), DeviceKind::Mouse, true),
            ],
        )];
        let mice = unlisted_mice(&inventories);
        let found: Vec<_> = mice
            .iter()
            .map(|mouse| (mouse.model.product_id, mouse.model.name, mouse.model.listed))
            .collect();
        assert_eq!(
            found,
            [(0x40A9, "PRO X 2", false), (0x40AB, "mouse 40ab", false)]
        );
        assert!(mice.iter().all(|mouse| !mouse.model.verified));
        assert_eq!(known_mice(&inventories).len(), 1, "listed ones stay apart");
    }

    #[test]
    fn leaves_direct_devices_and_unknown_receivers_alone() {
        let direct = receiver(
            0xC099,
            None,
            vec![paired(0xFF, Some(0x409F), DeviceKind::Mouse, true)],
        );
        // Without its unique id a receiver cannot be told apart, so nothing is routed to it.
        let anonymous = receiver(
            0xC547,
            None,
            vec![paired(1, Some(0x409F), DeviceKind::Mouse, true)],
        );
        assert!(known_mice(&[direct, anonymous]).is_empty());
    }
}
