// Embeds the application manifest (Common Controls v6, per-monitor DPI v2, UTF-8 code page),
// the app icon and the version information. The manifest goes through linker arguments, and
// the icon and version info are written as a compiled resource (.res) file that link.exe
// takes as an input, so no resource compiler or extra crate is required.

use std::path::{Path, PathBuf};

const ICON: &str = "assets/devdeck.ico";

fn main() {
    let target = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    let dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    println!("cargo:rerun-if-changed=app.manifest");
    println!("cargo:rerun-if-changed={ICON}");
    if target != "msvc" {
        return;
    }
    println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
    println!("cargo:rustc-link-arg-bins=/MANIFESTINPUT:{}", dir.join("app.manifest").display());
    let res = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("devdeck.res");
    std::fs::write(&res, resources(&dir.join(ICON))).expect("writing devdeck.res");
    println!("cargo:rustc-link-arg-bins={}", res.display());
}

const RT_ICON: u16 = 3;
const RT_GROUP_ICON: u16 = 14;
const RT_VERSION: u16 = 16;
const LANG_EN_US: u16 = 0x0409;

/// The .res file: icon images, the icon group (resource id 1) and VS_VERSIONINFO.
fn resources(ico_path: &Path) -> Vec<u8> {
    let ico = std::fs::read(ico_path).expect("reading the app icon");
    let mut out = Vec::new();
    // a .res file starts with an empty entry
    entry(&mut out, 0, 0, 0, &[]);
    // ICONDIR: reserved, type (1 = icon), count; then 16-byte ICONDIRENTRYs
    let count = u16::from_le_bytes([ico[4], ico[5]]) as usize;
    let mut group = Vec::new();
    group.extend_from_slice(&[0, 0, 1, 0]);
    group.extend_from_slice(&(count as u16).to_le_bytes());
    for i in 0..count {
        let e = &ico[6 + 16 * i..6 + 16 * (i + 1)];
        let size = u32::from_le_bytes([e[8], e[9], e[10], e[11]]) as usize;
        let offset = u32::from_le_bytes([e[12], e[13], e[14], e[15]]) as usize;
        let id = (i + 1) as u16;
        entry(&mut out, RT_ICON, id, 0x1010, &ico[offset..offset + size]);
        // GRPICONDIRENTRY: the first 12 bytes of ICONDIRENTRY, then the resource id
        group.extend_from_slice(&e[..12]);
        group.extend_from_slice(&id.to_le_bytes());
    }
    entry(&mut out, RT_GROUP_ICON, 1, 0x1030, &group);
    entry(&mut out, RT_VERSION, 1, 0x0030, &version_info());
    out
}

/// One RESOURCEHEADER (numeric type and name) followed by its data, DWORD aligned.
fn entry(out: &mut Vec<u8>, ty: u16, name: u16, flags: u16, data: &[u8]) {
    let lang = if ty == 0 { 0 } else { LANG_EN_US };
    out.extend_from_slice(&(data.len() as u32).to_le_bytes()); // DataSize
    out.extend_from_slice(&32u32.to_le_bytes()); // HeaderSize
    out.extend_from_slice(&[0xFF, 0xFF]);
    out.extend_from_slice(&ty.to_le_bytes());
    out.extend_from_slice(&[0xFF, 0xFF]);
    out.extend_from_slice(&name.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // DataVersion
    out.extend_from_slice(&flags.to_le_bytes()); // MemoryFlags
    out.extend_from_slice(&lang.to_le_bytes()); // LanguageId
    out.extend_from_slice(&0u32.to_le_bytes()); // Version
    out.extend_from_slice(&0u32.to_le_bytes()); // Characteristics
    out.extend_from_slice(data);
    pad4(out);
}

fn pad4(v: &mut Vec<u8>) {
    while v.len() % 4 != 0 {
        v.push(0);
    }
}

fn utf16z(s: &str) -> Vec<u8> {
    s.encode_utf16().chain(std::iter::once(0)).flat_map(|c| c.to_le_bytes()).collect()
}

/// A version resource block: wLength, wValueLength, wType, key, value, children.
fn block(key: &str, value: &[u8], value_len: u16, text: bool, children: &[Vec<u8>]) -> Vec<u8> {
    let mut b = vec![0, 0];
    b.extend_from_slice(&value_len.to_le_bytes());
    b.extend_from_slice(&(text as u16).to_le_bytes());
    b.extend_from_slice(&utf16z(key));
    pad4(&mut b);
    b.extend_from_slice(value);
    for c in children {
        pad4(&mut b);
        b.extend_from_slice(c);
    }
    let len = b.len() as u16;
    b[..2].copy_from_slice(&len.to_le_bytes());
    b
}

fn version_info() -> Vec<u8> {
    let v = |k: &str| std::env::var(k).ok().and_then(|x| x.parse::<u32>().ok()).unwrap_or(0);
    let (major, minor, patch) = (v("CARGO_PKG_VERSION_MAJOR"), v("CARGO_PKG_VERSION_MINOR"), v("CARGO_PKG_VERSION_PATCH"));
    let version = format!("{major}.{minor}.{patch}.0");
    let ms = (major << 16) | minor;
    let ls = patch << 16;
    // VS_FIXEDFILEINFO
    let mut fixed = Vec::new();
    for d in [0xFEEF04BDu32, 0x0001_0000, ms, ls, ms, ls, 0x3F, 0, 0x0004_0004, 1, 0, 0, 0] {
        fixed.extend_from_slice(&d.to_le_bytes());
    }
    let string = |k: &str, val: &str| {
        let data = utf16z(val);
        block(k, &data, (data.len() / 2) as u16, true, &[])
    };
    let strings = [
        string("CompanyName", "Bit Foundation"),
        string("FileDescription", "DevDeck"),
        string("FileVersion", &version),
        string("InternalName", "devdeck"),
        string("LegalCopyright", "Copyright (c) 2026 Bit Foundation"),
        string("OriginalFilename", "devdeck.exe"),
        string("ProductName", "DevDeck"),
        string("ProductVersion", &version),
    ];
    let table = block("040904b0", &[], 0, true, &strings);
    let sfi = block("StringFileInfo", &[], 0, true, &[table]);
    let translation = [0x09u8, 0x04, 0xB0, 0x04]; // en-US, Unicode code page 1200
    let var = block("Translation", &translation, 4, false, &[]);
    let vfi = block("VarFileInfo", &[], 0, true, &[var]);
    block("VS_VERSION_INFO", &fixed, fixed.len() as u16, false, &[sfi, vfi])
}
