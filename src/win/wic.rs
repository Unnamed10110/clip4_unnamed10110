//! Image decode / encode through WIC (no image crates; spec 4). Worker threads only.
use crate::model::*;
use windows::core::{Interface, GUID, PCWSTR};
use windows::Win32::Foundation::GENERIC_WRITE;
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::*;

/// Initialises COM (MTA) for the calling worker thread.
pub fn com_init() {
    // SAFETY: balanced only by process exit on a long-lived worker; harmless if already initialised.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
}

/// Wraps a packed DIB in a BITMAPFILEHEADER so the WIC BMP decoder accepts it.
pub fn dib_to_bmp_file(dib: &[u8]) -> Option<Vec<u8>> {
    if dib.len() < 40 {
        return None;
    }
    let hs = u32::from_le_bytes(dib[0..4].try_into().ok()?) as usize;
    if !(12..=124).contains(&hs) || dib.len() < hs {
        return None;
    }
    let bpp = u16::from_le_bytes(dib[14..16].try_into().ok()?) as usize;
    let comp = u32::from_le_bytes(dib[16..20].try_into().ok()?);
    let clr_used = u32::from_le_bytes(dib[32..36].try_into().ok()?) as usize;
    let mut table = if bpp <= 8 { (if clr_used > 0 { clr_used } else { 1 << bpp }) * 4 } else { clr_used * 4 };
    if hs == 40 && comp == 3 {
        table += 12;
    } else if hs == 40 && comp == 6 {
        table += 16;
    }
    let off = 14 + hs + table;
    let mut out = Vec::with_capacity(14 + dib.len());
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&((14 + dib.len()) as u32).to_le_bytes());
    out.extend_from_slice(&[0, 0, 0, 0]);
    out.extend_from_slice(&(off as u32).to_le_bytes());
    out.extend_from_slice(dib);
    Some(out)
}

fn factory() -> Option<IWICImagingFactory> {
    // SAFETY: COM object creation.
    unsafe { CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).ok() }
}

fn decode_frame(f: &IWICImagingFactory, file: &[u8]) -> Option<IWICBitmapSource> {
    // SAFETY: `file` outlives the stream use inside this function and its callers' scope; we
    // convert to an owned pixel buffer before returning anything that references it.
    unsafe {
        let stream = f.CreateStream().ok()?;
        stream.InitializeFromMemory(file).ok()?;
        let dec = f.CreateDecoderFromStream(&stream, std::ptr::null::<GUID>(), WICDecodeMetadataCacheOnLoad).ok()?;
        let frame = dec.GetFrame(0).ok()?;
        frame.cast::<IWICBitmapSource>().ok()
    }
}

fn file_bytes_for(key: &FormatKey, bytes: &[u8]) -> Option<Vec<u8>> {
    if key.is_named(FMT_PNG) {
        Some(bytes.to_vec())
    } else if key.is_std(CF_DIB) || key.is_std(CF_DIBV5) {
        dib_to_bmp_file(bytes)
    } else {
        None
    }
}

/// Decodes to a premultiplied BGRA thumbnail whose longer side is at most `max_px`.
pub fn thumbnail(key: &FormatKey, bytes: &[u8], max_px: u32) -> Option<(u32, u32, Vec<u8>)> {
    let file = file_bytes_for(key, bytes)?;
    let f = factory()?;
    // SAFETY: WIC pipeline; every interface is dropped before `file`.
    unsafe {
        let src = decode_frame(&f, &file)?;
        let (mut w, mut h) = (0u32, 0u32);
        src.GetSize(&mut w, &mut h).ok()?;
        if w == 0 || h == 0 {
            return None;
        }
        let scale = (max_px as f32 / w.max(h) as f32).min(1.0);
        let (tw, th) = (((w as f32 * scale) as u32).max(1), ((h as f32 * scale) as u32).max(1));
        let scaler = f.CreateBitmapScaler().ok()?;
        scaler.Initialize(&src, tw, th, WICBitmapInterpolationModeFant).ok()?;
        let conv = f.CreateFormatConverter().ok()?;
        conv.Initialize(&scaler, &GUID_WICPixelFormat32bppPBGRA, WICBitmapDitherTypeNone, None, 0.0, WICBitmapPaletteTypeCustom).ok()?;
        let mut buf = vec![0u8; (tw * th * 4) as usize];
        conv.CopyPixels(std::ptr::null(), tw * 4, &mut buf).ok()?;
        Some((tw, th, buf))
    }
}

/// Writes the image as a PNG file. A PNG payload is written as is.
pub fn save_png(key: &FormatKey, bytes: &[u8], path: &str) -> bool {
    if key.is_named(FMT_PNG) {
        return std::fs::write(path, bytes).is_ok();
    }
    let Some(file) = file_bytes_for(key, bytes) else { return false };
    let Some(f) = factory() else { return false };
    let w: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: WIC encode pipeline.
    let r: Option<()> = (|| unsafe {
        let src = decode_frame(&f, &file)?;
        let out = f.CreateStream().ok()?;
        out.InitializeFromFilename(PCWSTR(w.as_ptr()), GENERIC_WRITE.0).ok()?;
        let enc = f.CreateEncoder(&GUID_ContainerFormatPng, std::ptr::null()).ok()?;
        enc.Initialize(&out, WICBitmapEncoderNoCache).ok()?;
        let mut frame: Option<IWICBitmapFrameEncode> = None;
        enc.CreateNewFrame(&mut frame, std::ptr::null_mut()).ok()?;
        let frame = frame?;
        frame.Initialize(None).ok()?;
        let (mut iw, mut ih) = (0u32, 0u32);
        src.GetSize(&mut iw, &mut ih).ok()?;
        frame.SetSize(iw, ih).ok()?;
        let mut pf = GUID_WICPixelFormat32bppBGRA;
        frame.SetPixelFormat(&mut pf).ok()?;
        let conv = f.CreateFormatConverter().ok()?;
        conv.Initialize(&src, &GUID_WICPixelFormat32bppBGRA, WICBitmapDitherTypeNone, None, 0.0, WICBitmapPaletteTypeCustom).ok()?;
        frame.WriteSource(&conv, std::ptr::null()).ok()?;
        frame.Commit().ok()?;
        enc.Commit().ok()?;
        Some(())
    })();
    r.is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiny_dib() -> Vec<u8> {
        // 2x2, 32bpp BI_RGB, bottom-up.
        let mut v = Vec::new();
        v.extend_from_slice(&40u32.to_le_bytes());
        v.extend_from_slice(&2i32.to_le_bytes());
        v.extend_from_slice(&2i32.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&32u16.to_le_bytes());
        v.extend_from_slice(&[0; 24]);
        v.extend_from_slice(&[0, 0, 255, 255, 0, 255, 0, 255, 255, 0, 0, 255, 255, 255, 255, 255]);
        v
    }

    #[test]
    fn thumbnail_from_dib() {
        com_init();
        let (w, h, px) = thumbnail(&FormatKey::Standard(CF_DIB), &tiny_dib(), 64).expect("thumb");
        assert_eq!((w, h, px.len()), (2, 2, 16));
    }
}
