// Experimental changes: Speedstu, 2026-11-23/24. See LOADING-TRIAL.md for provenance.
//! CPU-only COM identities for pipelines not used yet. The first bind builds
//! the actual pipeline synchronously; these identities never reach the driver.
use std::{collections::{HashMap, HashSet}, ffi::c_void, sync::{Mutex, OnceLock, atomic::{AtomicU32, Ordering}}};
use windows::{core::{GUID, IUnknown, Interface}, Win32::Graphics::Direct3D12::*};

enum Data { Bytes(Vec<u8>), Object(IUnknown) }
#[repr(C)]
struct Deferred {
    vtable: *const usize,
    refs: AtomicU32,
    device: ID3D12Device,
    private: Mutex<HashMap<GUID, Data>>,
    name: Mutex<Vec<u16>>,
}
static LIVE: OnceLock<Mutex<HashSet<usize>>> = OnceLock::new();
fn live() -> &'static Mutex<HashSet<usize>> { LIVE.get_or_init(Default::default) }
pub(super) fn contains(p: usize) -> bool { live().lock().unwrap().contains(&p) }
pub(super) fn enabled() -> bool { std::env::var("XCL_DEFERRED_PSO").is_ok_and(|s| s == "1") }

unsafe extern "system" fn qi(this: *mut c_void, iid: *const GUID, out: *mut *mut c_void) -> i32 {
    if out.is_null() || iid.is_null() { return 0x80070057u32 as i32; }
    *out = std::ptr::null_mut();
    if [IUnknown::IID, ID3D12Object::IID, ID3D12DeviceChild::IID, ID3D12Pageable::IID, ID3D12PipelineState::IID].contains(&*iid) {
        *out = this; addref(this); 0
    } else { 0x80004002u32 as i32 }
}
unsafe extern "system" fn addref(this: *mut c_void) -> u32 { (*(this as *mut Deferred)).refs.fetch_add(1, Ordering::AcqRel) + 1 }
unsafe extern "system" fn release(this: *mut c_void) -> u32 {
    let p = this as *mut Deferred;
    let n = (*p).refs.fetch_sub(1, Ordering::AcqRel) - 1;
    if n == 0 {
        live().lock().unwrap().remove(&(this as usize));
        super::pso_async::release(super::pso_async::released(this as usize));
        drop(Box::from_raw(p));
    }
    n
}
unsafe extern "system" fn get_private(this: *mut c_void, guid: *const GUID, size: *mut u32, out: *mut c_void) -> i32 {
    if guid.is_null() || size.is_null() { return 0x80070057u32 as i32; }
    let g = (*(this as *mut Deferred)).private.lock().unwrap();
    let Some(data) = g.get(&*guid) else { return 0x887a0002u32 as i32 };
    let (src, bytes) = match data { Data::Bytes(v) => (v.as_ptr(), v.len()), Data::Object(o) => (o as *const IUnknown as *const u8, 8) };
    let available = *size as usize;
    *size = bytes as u32;
    if out.is_null() { return 0; }
    if available < bytes { return 0x887a0003u32 as i32; }
    std::ptr::copy_nonoverlapping(src, out.cast(), bytes);
    if let Data::Object(o) = data { std::mem::forget(o.clone()); }
    0
}
unsafe extern "system" fn set_private(this: *mut c_void, guid: *const GUID, size: u32, data: *const c_void) -> i32 {
    if guid.is_null() || (size != 0 && data.is_null()) { return 0x80070057u32 as i32; }
    let value = (size != 0).then(|| Data::Bytes(std::slice::from_raw_parts(data.cast(), size as usize).to_vec()));
    let old = { let mut g = (*(this as *mut Deferred)).private.lock().unwrap(); match value { Some(v) => g.insert(*guid, v), None => g.remove(&*guid) } };
    drop(old); 0
}
unsafe extern "system" fn set_interface(this: *mut c_void, guid: *const GUID, object: *mut c_void) -> i32 {
    if guid.is_null() { return 0x80070057u32 as i32; }
    let value = IUnknown::from_raw_borrowed(&object).map(|o| Data::Object(o.clone()));
    let old = { let mut g = (*(this as *mut Deferred)).private.lock().unwrap(); match value { Some(v) => g.insert(*guid, v), None => g.remove(&*guid) } };
    drop(old); 0
}
unsafe extern "system" fn set_name(this: *mut c_void, name: *const u16) -> i32 {
    if name.is_null() { return 0x80070057u32 as i32; }
    let mut n = 0; while *name.add(n) != 0 { n += 1; }
    *(*(this as *mut Deferred)).name.lock().unwrap() = std::slice::from_raw_parts(name, n).to_vec(); 0
}
unsafe extern "system" fn get_device(this: *mut c_void, iid: *const GUID, out: *mut *mut c_void) -> i32 {
    if iid.is_null() || out.is_null() { return 0x80070057u32 as i32; }
    let device = &(*(this as *mut Deferred)).device;
    let v = *(device.as_raw() as *const *const usize);
    let f: unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> i32 = std::mem::transmute(*v);
    f(device.as_raw(), iid, out)
}
unsafe extern "system" fn get_blob(this: *mut c_void, out: *mut *mut c_void) -> i32 {
    let real = super::pso_async::resolve_wait(this as usize) as *mut c_void;
    let v = *(real as *const *const usize);
    let f: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> i32 = std::mem::transmute(*v.add(8));
    f(real, out)
}

pub(super) unsafe fn create(device: *mut c_void) -> *mut c_void {
    static VT: OnceLock<usize> = OnceLock::new();
    let vt = *VT.get_or_init(|| {
        let mut slots: Vec<usize> = (0..576).map(|i| crate::stubs::make_stub_info("d3d12_xs.dll", &format!("ID3D12PipelineState(deferred)::#{i}"), 0).0).collect();
        slots[..9].copy_from_slice(&[qi as usize, addref as usize, release as usize, get_private as usize, set_private as usize, set_interface as usize, set_name as usize, get_device as usize, get_blob as usize]);
        Box::leak(slots.into_boxed_slice()).as_ptr() as usize
    });
    let device = ID3D12Device::from_raw_borrowed(&device).unwrap().clone();
    let p = Box::into_raw(Box::new(Deferred { vtable: vt as *const usize, refs: AtomicU32::new(1), device, private: Default::default(), name: Default::default() }));
    live().lock().unwrap().insert(p as usize);
    p.cast()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn deferred_pipeline_com_metadata_is_independent_and_releases() {
        unsafe {
            let mut device: Option<ID3D12Device> = None;
            D3D12CreateDevice(None, windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0, &mut device).unwrap();
            let device = device.unwrap();
            let a = create(device.as_raw()); let b = create(device.as_raw());
            let guid = GUID::from_u128(0x12345678_1234_5678_9000_000000000001);
            let bytes = [3u8, 1, 4, 1];
            assert_eq!(set_private(a, &guid, 4, bytes.as_ptr().cast()), 0);
            let mut n = 4; let mut result = [0u8; 4];
            assert_eq!(get_private(a, &guid, &mut n, result.as_mut_ptr().cast()), 0); assert_eq!(result, bytes);
            assert_eq!(get_private(b, &guid, &mut n, result.as_mut_ptr().cast()), 0x887a0002u32 as i32);
            let mut out = std::ptr::null_mut(); assert_eq!(qi(a, &ID3D12PipelineState::IID, &mut out), 0); assert_eq!(out,a); assert_eq!(release(out),1);
            assert_eq!(get_device(a,&ID3D12Device::IID,&mut out),0); drop(ID3D12Device::from_raw(out));
            assert!(contains(a as usize)); assert_eq!(release(a),0); assert!(!contains(a as usize)); assert_eq!(release(b),0);
        }
    }
}
