// Experimental changes: Speedstu, 2026-11-23/24. See LOADING-TRIAL.md for provenance.
//! Read-only streamed textures: reserve their GPU address space first and back it
//! with video memory when data arrives. Tier-2 null tiles read zero. Never discard
//! a later zero upload to a texture that already has backing memory.
use std::{collections::HashMap, ffi::c_void, sync::{Mutex, OnceLock}};
use windows::{core::Interface, Win32::Graphics::Direct3D12::*};

struct Entry { device: usize, heap: Option<ID3D12Heap>, bytes: u64 }
static ENTRIES: OnceLock<Mutex<HashMap<usize, Entry>>> = OnceLock::new();
fn entries() -> &'static Mutex<HashMap<usize, Entry>> { ENTRIES.get_or_init(Default::default) }

pub(super) unsafe fn create(device: *mut c_void, desc: &D3D12_RESOURCE_DESC, initial: u32, out: *mut *mut c_void) -> Option<i32> {
    if std::env::var("XCL_SPARSE_TEXTURES").ok().as_deref() != Some("1")
        || desc.Dimension != D3D12_RESOURCE_DIMENSION_TEXTURE2D || desc.SampleDesc.Count != 1 || desc.Flags.0 != 0 { return None; }
    let dev = ID3D12Device::from_raw_borrowed(&device)?;
    let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS::default();
    let mut format = D3D12_FEATURE_DATA_FORMAT_SUPPORT { Format: desc.Format, ..Default::default() };
    if dev.CheckFeatureSupport(D3D12_FEATURE_D3D12_OPTIONS, &mut options as *mut _ as _, std::mem::size_of_val(&options) as u32).is_err()
        || options.TiledResourcesTier.0 < 2 || options.ResourceHeapTier.0 < 2
        || dev.CheckFeatureSupport(D3D12_FEATURE_FORMAT_SUPPORT, &mut format as *mut _ as _, std::mem::size_of_val(&format) as u32).is_err()
        || !format.Support2.contains(D3D12_FORMAT_SUPPORT2_TILED) { return None; }
    let mut d = *desc;
    d.Alignment = 0;
    d.Layout = D3D12_TEXTURE_LAYOUT_64KB_UNDEFINED_SWIZZLE;
    let mut res: Option<ID3D12Resource> = None;
    if let Err(e) = dev.CreateReservedResource(&d, D3D12_RESOURCE_STATES(initial as i32), None, &mut res) { return Some(e.code().0); }
    let res = res?;
    let raw = res.into_raw();
    super::real::adopt(raw, super::real::Kind::Resource);
    let mut entries = entries().lock().unwrap();
    entries.insert(raw as usize, Entry { device: device as usize, heap: None, bytes: 0 });
    if entries.len() == 1 || entries.len() % 1024 == 0 {
        let bytes: u64 = entries.values().map(|e| e.bytes).sum();
        crate::xlog_info!("streamed textures: {} virtual textures, {} MiB backing", entries.len(), bytes >> 20);
    }
    *out = raw;
    Some(0)
}

pub(super) fn unmapped(resource: usize) -> bool {
    entries().lock().unwrap().get(&resource).is_some_and(|e| e.heap.is_none())
}

pub(super) fn forget(resource: usize) { entries().lock().unwrap().remove(&resource); }

pub(super) unsafe fn ensure(resource: usize) -> Result<(), String> {
    let mut entries = entries().lock().unwrap();
    let Some(e) = entries.get_mut(&resource) else { return Ok(()) };
    if e.heap.is_some() { return Ok(()); }
    let device = e.device as *mut c_void;
    let dev = ID3D12Device::from_raw_borrowed(&device).ok_or("texture device")?;
    let raw = resource as *mut c_void;
    let res = ID3D12Resource::from_raw_borrowed(&raw).ok_or("texture")?;
    let mut tiles = 0;
    dev.GetResourceTiling(res, Some(&mut tiles), None, None, None, 0, std::ptr::null_mut());
    if tiles == 0 { return Err("streamed texture has no tiles".into()); }
    let mut heap: Option<ID3D12Heap> = None;
    dev.CreateHeap(&D3D12_HEAP_DESC {
        SizeInBytes: tiles as u64 * 65536,
        Properties: D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() },
        Flags: D3D12_HEAP_FLAG_ALLOW_ONLY_NON_RT_DS_TEXTURES, ..Default::default()
    }, &mut heap).map_err(|e| format!("streamed texture backing: {e}"))?;
    let heap = heap.ok_or("texture heap")?;
    let queue: ID3D12CommandQueue = dev.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC::default()).map_err(|e| e.to_string())?;
    let fence: ID3D12Fence = dev.CreateFence(0, D3D12_FENCE_FLAG_NONE).map_err(|e| e.to_string())?;
    super::queue_order::update(dev, &queue, || {
        queue.UpdateTileMappings(res, 1, None, None, &heap, 1, Some(&D3D12_TILE_RANGE_FLAG_NONE), Some(&0), Some(&tiles), D3D12_TILE_MAPPING_FLAG_NONE);
    });
    super::queue_order::signal(&queue, &fence, 1).map_err(|e| e.to_string())?;
    let start = std::time::Instant::now();
    while fence.GetCompletedValue() < 1 {
        if start.elapsed().as_secs() >= 10 { return Err("streamed texture mapping timeout".into()); }
        dev.GetDeviceRemovedReason().map_err(|e| e.to_string())?;
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    e.heap = Some(heap);
    e.bytes = tiles as u64 * 65536;
    let resident = entries.values().filter(|e| e.heap.is_some()).count();
    if resident.is_power_of_two() {
        let bytes: u64 = entries.values().map(|e| e.bytes).sum();
        crate::xlog_info!("streamed textures: {resident} resident textures, {} MiB backing", bytes >> 20);
    }
    Ok(())
}

pub(super) fn ensure_or_stop(resource: usize) {
    if let Err(e) = unsafe { ensure(resource) } {
        let reason = format!("streamed texture residency failed: {e}");
        crate::xlog_error!("{reason}");
        crate::report::write_final(Some(reason));
        super::real::end_title(0x7c1000b);
    }
}

/// Same holes-as-zero convention as title_textures::read_committed, without
/// allocating or detiling the whole texture just to find out that it is empty.
pub(super) unsafe fn has_data(at: usize, len: usize) -> bool {
    use windows_sys::Win32::System::Memory::{VirtualQuery, MEMORY_BASIC_INFORMATION, MEM_COMMIT};
    let Some(limit) = at.checked_add(len) else { return true };
    let mut p = at;
    while p < limit {
        let mut m: MEMORY_BASIC_INFORMATION = std::mem::zeroed();
        if VirtualQuery(p as *const c_void, &mut m, std::mem::size_of_val(&m)) == 0 { return true; }
        let end = (m.BaseAddress as usize).saturating_add(m.RegionSize).min(limit);
        if end <= p { return true; }
        if m.State == MEM_COMMIT && m.Protect & 0xEE != 0 && m.Protect & 0x100 == 0 {
            let bytes = std::slice::from_raw_parts(p as *const u8, end - p);
            if bytes.iter().any(|&b| b != 0) { return true; }
        }
        p = end;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{transmute_copy, ManuallyDrop};
    use windows::Win32::Graphics::{Direct3D::D3D_FEATURE_LEVEL_11_0, Dxgi::Common::*};

    #[test]
    fn zero_scan_checks_late_data_and_skips_uncommitted_holes() { unsafe {
        use windows_sys::Win32::System::Memory::*;
        let p = VirtualAlloc(std::ptr::null(), 65536, MEM_RESERVE, PAGE_READWRITE) as *mut u8;
        assert!(!p.is_null());
        assert!(!has_data(p as usize, 65536));
        assert!(!VirtualAlloc(p.add(32768) as _,4096,MEM_COMMIT,PAGE_READWRITE).is_null());
        assert!(!has_data(p as usize,65536));
        *p.add(32768+4095)=7;
        assert!(has_data(p as usize,65536),"data is not restricted to the first sampled page");
        *p.add(32768+4095)=0;
        assert!(!has_data(p as usize,65536));
        VirtualFree(p as _,0,MEM_RELEASE);
    }}

    unsafe fn read(dev:&ID3D12Device,q:&ID3D12CommandQueue,r:&ID3D12Resource) -> Vec<u8> {
        let desc=r.GetDesc();let mut fp=D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
        let (mut rows,mut row_bytes,mut total)=(0,0,0);
        dev.GetCopyableFootprints(&desc,0,1,0,Some(&mut fp),Some(&mut rows),Some(&mut row_bytes),Some(&mut total));
        let mut rb=None;dev.CreateCommittedResource(&D3D12_HEAP_PROPERTIES{Type:D3D12_HEAP_TYPE_READBACK,..Default::default()},D3D12_HEAP_FLAG_NONE,
            &D3D12_RESOURCE_DESC{Dimension:D3D12_RESOURCE_DIMENSION_BUFFER,Width:total,Height:1,DepthOrArraySize:1,MipLevels:1,SampleDesc:DXGI_SAMPLE_DESC{Count:1,Quality:0},Layout:D3D12_TEXTURE_LAYOUT_ROW_MAJOR,..Default::default()},D3D12_RESOURCE_STATE_COPY_DEST,None,&mut rb).unwrap();
        let rb:ID3D12Resource=rb.unwrap();let a:ID3D12CommandAllocator=dev.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).unwrap();
        let l:ID3D12GraphicsCommandList=dev.CreateCommandList(0,D3D12_COMMAND_LIST_TYPE_DIRECT,&a,None).unwrap();
        let barrier=|before,after|D3D12_RESOURCE_BARRIER{Type:D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,Anonymous:D3D12_RESOURCE_BARRIER_0{Transition:ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER{pResource:transmute_copy(r),Subresource:0,StateBefore:before,StateAfter:after})},..Default::default()};
        l.ResourceBarrier(&[barrier(crate::gpu::texture::SHADER_READ,D3D12_RESOURCE_STATE_COPY_SOURCE)]);
        let dst=D3D12_TEXTURE_COPY_LOCATION{pResource:transmute_copy(&rb),Type:D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,Anonymous:D3D12_TEXTURE_COPY_LOCATION_0{PlacedFootprint:fp}};
        let src=D3D12_TEXTURE_COPY_LOCATION{pResource:transmute_copy(r),Type:D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,Anonymous:D3D12_TEXTURE_COPY_LOCATION_0{SubresourceIndex:0}};
        l.CopyTextureRegion(&dst,0,0,0,&src,None);l.ResourceBarrier(&[barrier(D3D12_RESOURCE_STATE_COPY_SOURCE,crate::gpu::texture::SHADER_READ)]);
        l.Close().unwrap();q.ExecuteCommandLists(&[Some(l.cast().unwrap())]);crate::gpu::texture::wait(dev,q).unwrap();
        let mut p=std::ptr::null_mut();rb.Map(0,None,Some(&mut p)).unwrap();let mut bytes=Vec::new();
        for y in 0..rows as usize { bytes.extend_from_slice(std::slice::from_raw_parts((p as *const u8).add(y*fp.Footprint.RowPitch as usize),row_bytes as usize)); }
        rb.Unmap(0,None);bytes
    }

    #[test]
    fn gpu_streamed_textures_start_zero_then_preserve_data_and_zero_updates() { unsafe {
        std::env::set_var("XCL_SPARSE_TEXTURES","1");
        let mut debug:Option<ID3D12Debug>=None;D3D12GetDebugInterface(&mut debug).unwrap();debug.unwrap().EnableDebugLayer();
        let mut dev=None;D3D12CreateDevice(None,D3D_FEATURE_LEVEL_11_0,&mut dev).unwrap();let dev:ID3D12Device=dev.unwrap();
        let info:ID3D12InfoQueue=dev.cast().unwrap();let q=dev.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC::default()).unwrap();
        for (format,width,height,rows,row_bytes) in [(28,64,64,64,256),(71,64,64,16,128),(98,64,64,16,256),(78,1024,1024,256,4096)] {
            let desc=D3D12_RESOURCE_DESC{Dimension:D3D12_RESOURCE_DIMENSION_TEXTURE2D,Width:width,Height:height,DepthOrArraySize:1,MipLevels:1,Format:DXGI_FORMAT(format),SampleDesc:DXGI_SAMPLE_DESC{Count:1,Quality:0},..Default::default()};
            println!("Testing reserved texture {width}x{height}, format {format}");
            let mut raw=std::ptr::null_mut();assert_eq!(create(dev.as_raw(),&desc,crate::gpu::texture::SHADER_READ.0 as u32,&mut raw),Some(0));
            let r=ID3D12Resource::from_raw(raw);assert!(unmapped(raw as usize));
            // Console Map must return its title address without making the invalid
            // native Map call on a reserved D3D12 resource (validation id 824).
            let cpu=windows_sys::Win32::System::Memory::VirtualAlloc(std::ptr::null(),65536,0x3000,4);
            assert!(!cpu.is_null());*(cpu as *mut u8)=0x53;
            super::super::real::note_title_address(raw as usize,cpu as usize);
            let mut mapped=std::ptr::null_mut();r.Map(0,None,Some(&mut mapped)).unwrap();
            assert_eq!(mapped,cpu);assert_eq!(*(mapped as *const u8),0x53);r.Unmap(0,None);
            windows_sys::Win32::System::Memory::VirtualFree(cpu,0,0x8000);
            assert_eq!(read(&dev,&q,&r),vec![0;rows*row_bytes],"NULL tiles must read zero");
            ensure(raw as usize).unwrap();assert!(!unmapped(raw as usize));
            for value in [0x6bu8,0] {
                let sub=crate::gpu::texture::Subresource{depth:1,rows,row_bytes,data:vec![value;rows*row_bytes]};
                let failures=crate::gpu::texture::upload_batch(&dev,&q,&[(r.clone(),vec![sub],crate::gpu::texture::SHADER_READ,crate::gpu::texture::SHADER_READ)]).unwrap();assert!(failures.is_empty());
                assert_eq!(read(&dev,&q,&r),vec![value;rows*row_bytes],"later zero writes cannot be skipped");
            }
            drop(r);assert!(!entries().lock().unwrap().contains_key(&(raw as usize)));
        }
        let mut errors=Vec::new();
        for i in 0..info.GetNumStoredMessagesAllowedByRetrievalFilter(){let mut n=0;info.GetMessage(i,None,&mut n).unwrap();let mut bytes=vec![0u8;n];let m=bytes.as_mut_ptr() as *mut D3D12_MESSAGE;info.GetMessage(i,Some(m),&mut n).unwrap();if (*m).Severity.0<=D3D12_MESSAGE_SEVERITY_ERROR.0{errors.push((*m).ID.0);}}
        assert!(errors.is_empty(),"D3D12 validation errors: {errors:?}");
        std::env::remove_var("XCL_SPARSE_TEXTURES");
    }}
}
