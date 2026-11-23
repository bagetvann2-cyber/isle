//! Bluetooth (Classic + LE) battery via the PnP property
//! `DEVPKEY_Bluetooth_Battery` {104EA319-6EE2-4701-BD47-8DDBF425BBE5},2 (u8).
//! Windows stores it on a child service node (e.g. HFP), so nodes are grouped
//! by Bluetooth MAC. Windows also keeps the last value of a powered-off device,
//! so a group is reported only while a node says "connected".

use super::{device_ids, wide};
use crate::model::{Device, DeviceKind};
use std::collections::BTreeMap;
use windows::core::GUID;
use windows::Win32::Devices::DeviceAndDriverInstallation::*;
use windows::Win32::Devices::Properties::{DEVPROPTYPE, DEVPROP_TYPE_BOOLEAN, DEVPROP_TYPE_BYTE, DEVPROP_TYPE_STRING, DEVPROP_TYPE_UINT32};
use windows::Win32::Foundation::DEVPROPKEY;

const BT_GUID: GUID = GUID::from_u128(0x2BD67D8B_8BEB_48D5_87E0_6CDA3428040A);
const DEV_GUID: GUID = GUID::from_u128(0xA45C254E_DF1C_4EFD_8020_67D146A850E0);
const BATTERY: DEVPROPKEY = DEVPROPKEY { fmtid: GUID::from_u128(0x104EA319_6EE2_4701_BD47_8DDBF425BBE5), pid: 2 };
const IS_CONNECTED: DEVPROPKEY = DEVPROPKEY { fmtid: GUID::from_u128(0x83DA6326_97A6_4088_9453_A1923F573B29), pid: 15 };
const BT_ADDRESS: DEVPROPKEY = DEVPROPKEY { fmtid: BT_GUID, pid: 1 };
const BT_CLASS_OF_DEVICE: DEVPROPKEY = DEVPROPKEY { fmtid: BT_GUID, pid: 10 };
const FRIENDLY_NAME: DEVPROPKEY = DEVPROPKEY { fmtid: DEV_GUID, pid: 14 };
const DEVICE_CLASS: DEVPROPKEY = DEVPROPKEY { fmtid: DEV_GUID, pid: 9 };

const ENUMERATORS: [&str; 3] = ["BTHENUM", "BTHLE", "BTHLEDEVICE"];
const MAX_PROP: u32 = 4096;
/// Suffixes of child-node names ("Redmi Buds 6 Hands-Free AG") to strip.
const NAME_SUFFIXES: [&str; 6] = [" Hands-Free AG", " Hands-Free HF", " Hands-Free", " Avrcp Transport", " Stereo", " Headset"];

/// Everything we learn about one Bluetooth device across its PnP nodes.
#[derive(Default)]
struct Group {
    battery: Option<u8>,
    connected: bool,
    class_of_device: Option<u32>,
    root_name: Option<String>,
    other_names: Vec<String>,
    classes: Vec<String>,
}

pub fn poll() -> Vec<Device> {
    let mut groups: BTreeMap<String, Group> = BTreeMap::new();
    for e in ENUMERATORS {
        for id in device_ids(e) {
            read_node(&id, &mut groups);
        }
    }
    groups.into_iter().filter_map(|(mac, g)| to_device(mac, g)).collect()
}

fn to_device(mac: String, g: Group) -> Option<Device> {
    let percent = g.battery.filter(|&b| g.connected && b <= 100)?;
    // The root node carries the real product name; only child-node names get suffixes stripped.
    let name = g
        .root_name
        .or_else(|| {
            let n = g.other_names.into_iter().min_by_key(String::len)?;
            Some(clean_name(&n))
        })
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| format!("Bluetooth {mac}"));
    let kind = g.class_of_device.and_then(kind_from_cod).unwrap_or_else(|| kind_fallback(&g.classes, &name));
    Some(Device { id: mac, name, kind, percent, charging: None, parts: Vec::new() })
}

fn read_node(id: &str, groups: &mut BTreeMap<String, Group>) {
    let id_w = wide(id);
    let mut dev = 0u32;
    let located = unsafe { CM_Locate_DevNodeW(&mut dev, windows::core::PCWSTR(id_w.as_ptr()), CM_LOCATE_DEVNODE_NORMAL) };
    if located != CR_SUCCESS {
        return;
    }
    let Some(mac) = prop_str(dev, &BT_ADDRESS).as_deref().and_then(normalize_mac).or_else(|| mac_from_id(id)) else {
        return;
    };
    let g = groups.entry(mac).or_default();
    g.battery = g.battery.or_else(|| prop(dev, &BATTERY, DEVPROP_TYPE_BYTE).and_then(|v| v.first().copied()));
    g.connected |= prop(dev, &IS_CONNECTED, DEVPROP_TYPE_BOOLEAN).is_some_and(|v| v.first().is_some_and(|&b| b != 0));
    g.class_of_device = g.class_of_device.or_else(|| prop_u32(dev, &BT_CLASS_OF_DEVICE));
    if let Some(c) = prop_str(dev, &DEVICE_CLASS) {
        g.classes.push(c);
    }
    if let Some(n) = prop_str(dev, &FRIENDLY_NAME) {
        if is_root(id) {
            g.root_name = Some(n);
        } else {
            g.other_names.push(n);
        }
    }
}

fn prop(dev: u32, key: &DEVPROPKEY, want: DEVPROPTYPE) -> Option<Vec<u8>> {
    let (mut ty, mut size) = (DEVPROPTYPE(0), 0u32);
    let r = unsafe { CM_Get_DevNode_PropertyW(dev, key, &mut ty, None, &mut size, 0) };
    if r != CR_BUFFER_SMALL || ty != want || size == 0 || size > MAX_PROP {
        return None;
    }
    let mut buf = vec![0u8; size as usize];
    let r = unsafe { CM_Get_DevNode_PropertyW(dev, key, &mut ty, Some(buf.as_mut_ptr()), &mut size, 0) };
    if r != CR_SUCCESS || ty != want {
        return None;
    }
    buf.truncate(size as usize);
    Some(buf)
}

fn prop_u32(dev: u32, key: &DEVPROPKEY) -> Option<u32> {
    let v = prop(dev, key, DEVPROP_TYPE_UINT32)?;
    Some(u32::from_le_bytes(v.get(..4)?.try_into().ok()?))
}

fn prop_str(dev: u32, key: &DEVPROPKEY) -> Option<String> {
    let v = prop(dev, key, DEVPROP_TYPE_STRING)?;
    let w: Vec<u16> = v.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).take_while(|&c| c != 0).collect();
    Some(String::from_utf16_lossy(&w))
}

/// Root node of a device: `BTHENUM\DEV_<mac>\...` / `BTHLE\DEV_<mac>\...`.
fn is_root(id: &str) -> bool {
    id.to_ascii_uppercase().contains("\\DEV_")
}

/// 12 hex digits, uppercase, no separators.
fn normalize_mac(s: &str) -> Option<String> {
    let m: String = s.chars().filter(char::is_ascii_hexdigit).collect::<String>().to_ascii_uppercase();
    (m.len() == 12 && m != "000000000000").then_some(m)
}

/// Fallback when the address property is missing: the last 12-hex token of the
/// instance id that is not the Bluetooth base UUID tail.
fn mac_from_id(id: &str) -> Option<String> {
    id.split(|c: char| !c.is_ascii_alphanumeric())
        .rfind(|t| t.len() == 12 && t.chars().all(|c| c.is_ascii_hexdigit()) && !t.eq_ignore_ascii_case("00805F9B34FB"))
        .and_then(normalize_mac)
}

fn clean_name(n: &str) -> String {
    let mut s = n.trim();
    loop {
        let before = s;
        for suf in NAME_SUFFIXES {
            let cut = s.len().saturating_sub(suf.len());
            if s.len() > suf.len() && s.is_char_boundary(cut) && s[cut..].eq_ignore_ascii_case(suf) {
                s = s[..cut].trim_end();
            }
        }
        if s == before {
            return s.to_string();
        }
    }
}

/// Kind from the Bluetooth class-of-device (major/minor class bits).
fn kind_from_cod(cod: u32) -> Option<DeviceKind> {
    let (major, minor) = ((cod >> 8) & 0x1F, (cod >> 2) & 0x3F);
    match major {
        // audio/video: 1 headset, 2 hands-free, 6 headphones; mic/speaker/car/etc are not battery "buds"
        4 => Some(match minor {
            1 | 2 => DeviceKind::Headset,
            4 | 5 | 8..=10 | 12.. => DeviceKind::Other,
            _ => DeviceKind::Headphones,
        }),
        // peripheral: bit 6 keyboard, bit 7 pointing device, low nibble 1/2 joystick/gamepad
        5 if cod & 0x40 != 0 => Some(DeviceKind::Keyboard),
        5 if cod & 0x80 != 0 => Some(DeviceKind::Mouse),
        5 if matches!(minor & 0xF, 1 | 2) => Some(DeviceKind::Gamepad),
        _ => None,
    }
}

/// Kind from PnP classes of the child nodes, then the name.
fn kind_fallback(classes: &[String], name: &str) -> DeviceKind {
    let has = |c: &str| classes.iter().any(|x| x.eq_ignore_ascii_case(c));
    let n = name.to_lowercase();
    if has("Mouse") || n.contains("mouse") {
        DeviceKind::Mouse
    } else if has("Keyboard") || n.contains("keyboard") {
        DeviceKind::Keyboard
    } else if ["gamepad", "controller", "joystick"].iter().any(|k| n.contains(k)) {
        DeviceKind::Gamepad
    } else if has("AudioEndpoint") || has("MEDIA") {
        DeviceKind::Headphones
    } else {
        DeviceKind::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cod_kinds() {
        assert_eq!(kind_from_cod(0x240404), Some(DeviceKind::Headset)); // Redmi Buds 6
        assert_eq!(kind_from_cod(0x240418), Some(DeviceKind::Headphones));
        assert_eq!(kind_from_cod(0x002580), Some(DeviceKind::Mouse));
        assert_eq!(kind_from_cod(0x002540), Some(DeviceKind::Keyboard));
        assert_eq!(kind_from_cod(0x0025C0), Some(DeviceKind::Keyboard)); // combo
        assert_eq!(kind_from_cod(0x000508), Some(DeviceKind::Gamepad));
        assert_eq!(kind_from_cod(0x5A020C), None); // phone
    }

    #[test]
    fn names_and_macs() {
        assert_eq!(clean_name("Redmi Buds 6 Hands-Free AG"), "Redmi Buds 6");
        assert_eq!(clean_name("MX Keys Avrcp Transport"), "MX Keys");
        assert_eq!(clean_name("Redmi Buds 6"), "Redmi Buds 6");
        assert_eq!(clean_name("Stereo"), "Stereo");
        assert_eq!(normalize_mac("12:34:56:ab:cd:ef").as_deref(), Some("123456ABCDEF"));
        assert_eq!(normalize_mac("bad"), None);
        let id = r"BTHENUM\{0000111E-0000-1000-8000-00805F9B34FB}_VID&000102B0_PID&0000\7&2A1B3C4D&0&123456ABCDEF_C00000000";
        assert_eq!(mac_from_id(id).as_deref(), Some("123456ABCDEF"));
        let root = r"BTHENUM\DEV_123456ABCDEF\7&2A1B3C4D&0&BLUETOOTHDEVICE_123456ABCDEF";
        assert_eq!(mac_from_id(root).as_deref(), Some("123456ABCDEF"));
        assert!(is_root(root));
        assert!(!is_root(id));
    }

    #[test]
    fn stale_values_are_hidden() {
        let g = |connected| Group {
            battery: Some(100),
            connected,
            root_name: Some("Redmi Buds 6".into()),
            class_of_device: Some(0x240404),
            ..Group::default()
        };
        assert!(to_device("AA".into(), g(false)).is_none());
        let d = to_device("AA".into(), g(true));
        assert_eq!(d.map(|d| (d.name, d.kind, d.percent)), Some(("Redmi Buds 6".into(), DeviceKind::Headset, 100)));
    }
}
