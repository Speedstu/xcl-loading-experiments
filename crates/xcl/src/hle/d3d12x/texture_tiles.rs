// Experimental changes: Speedstu, 2026-11-23/24. See LOADING-TRIAL.md for provenance.
//! GPU-owned textures share 64 KiB physical tiles by title address. Reserved
//! resources avoid replacing growing heaps and retaining every earlier heap.
use std::{collections::{BTreeMap, HashMap}, ffi::c_void, sync::{Mutex, OnceLock}};
use windows::{core::Interface, Win32::Graphics::Direct3D12::*};

const TILE: usize = 65536;
const POOL_TILES: u32 = 1024;
struct Page { pool: usize, tile: u32, refs: u32 }
struct Pool { heap: ID3D12Heap, free: Vec<u32> }
struct Tiles {
    device: ID3D12Device,
    queue: ID3D12CommandQueue,
    fence: ID3D12Fence,
    serial: u64,
    pools: Vec<Pool>,
    pages: BTreeMap<usize, Page>,
    resources: HashMap<usize, Vec<usize>>,
}
unsafe impl Send for Tiles {}
static STATE: OnceLock<Mutex<Option<Tiles>>> = OnceLock::new();
fn state() -> &'static Mutex<Option<Tiles>> { STATE.get_or_init(Default::default) }

impl Tiles {
    unsafe fn new(device: &ID3D12Device) -> windows::core::Result<Self> {
        Ok(Self { device: device.clone(), queue: device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC::default())?,
            fence: device.CreateFence(0, D3D12_FENCE_FLAG_NONE)?, serial: 0,
            pools: Vec::new(), pages: BTreeMap::new(), resources: HashMap::new() })
    }
    fn release_pages(&mut self, keys: &[usize]) {
        for key in keys {
            if let Some(page) = self.pages.get_mut(key) {
                page.refs -= 1;
                if page.refs == 0 {
                    let page = self.pages.remove(key).unwrap();
                    self.pools[page.pool].free.push(page.tile);
                }
            }
        }
    }
    unsafe fn acquire(&mut self, key: usize) -> Result<(usize, u32), String> {
        if let Some(page) = self.pages.get_mut(&key) { page.refs += 1; return Ok((page.pool, page.tile)); }
        let pool = if let Some(i) = self.pools.iter().position(|p| !p.free.is_empty()) { i } else {
            let mut heap: Option<ID3D12Heap> = None;
            self.device.CreateHeap(&D3D12_HEAP_DESC { SizeInBytes: POOL_TILES as u64 * TILE as u64,
                Properties: D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() },
                Alignment: D3D12_DEFAULT_MSAA_RESOURCE_PLACEMENT_ALIGNMENT as u64,
                Flags: D3D12_HEAP_FLAG_NONE }, &mut heap).map_err(|e| format!("texture tile heap: {e}"))?;
            self.pools.push(Pool { heap: heap.ok_or("no tile heap")?, free: (0..POOL_TILES).rev().collect() });
            self.pools.len()-1
        };
        let tile = self.pools[pool].free.pop().unwrap();
        self.pages.insert(key, Page { pool, tile, refs: 1 });
        Ok((pool, tile))
    }
    unsafe fn map(&mut self, resource: &ID3D12Resource, va: usize) -> Result<(), String> {
        let mut total = 0;
        self.device.GetResourceTiling(resource, Some(&mut total), None, None, None, 0, std::ptr::null_mut());
        if total == 0 { return Err("resource has no tiles".into()); }
        let mut keys = Vec::with_capacity(total as usize);
        let mut tiles = Vec::with_capacity(total as usize);
        for i in 0..total as usize {
            let key = va.checked_add(i*TILE).ok_or("texture address overflow")?;
            match self.acquire(key) {
                Ok(tile) => { keys.push(key); tiles.push(tile); },
                Err(e) => { self.release_pages(&keys); return Err(e); }
            }
        }
        // A NULL coordinate and region specify the entire resource, including
        // packed mips and array slices. SKIP retains mappings made for other heaps.
        let mut used: Vec<usize> = tiles.iter().map(|t| t.0).collect(); used.sort_unstable(); used.dedup();
        for pool in used {
            let mut flags = Vec::new(); let mut offsets = Vec::new(); let mut counts = Vec::<u32>::new();
            for &(p, tile) in &tiles {
                let flag = if p == pool { D3D12_TILE_RANGE_FLAG_NONE } else { D3D12_TILE_RANGE_FLAG_SKIP };
                if flags.last() == Some(&flag) && (flag == D3D12_TILE_RANGE_FLAG_SKIP || offsets.last().copied().unwrap_or(0)+counts.last().copied().unwrap_or(0) == tile) {
                    *counts.last_mut().unwrap() += 1;
                } else { flags.push(flag); offsets.push(tile); counts.push(1); }
            }
            self.queue.UpdateTileMappings(resource, 1, None, None, &self.pools[pool].heap, flags.len() as u32,
                Some(flags.as_ptr()), Some(offsets.as_ptr()), Some(counts.as_ptr()), D3D12_TILE_MAPPING_FLAG_NONE);
        }
        self.serial += 1;
        self.queue.Signal(&self.fence, self.serial).map_err(|e| format!("tile mapping fence: {e}"))?;
        let started = std::time::Instant::now();
        while self.fence.GetCompletedValue() < self.serial {
            if started.elapsed().as_secs() >= 10 { return Err("tile mapping fence timeout".into()); }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        self.device.GetDeviceRemovedReason().map_err(|e| format!("tile mapping removed device: {e}"))?;
        self.resources.insert(resource.as_raw() as usize, keys);
        let n = self.resources.len();
        if n == 1 || n % 250 == 0 { crate::xlog_info!("D3D12.X: tiled textures: {n} resources, {} MiB unique pages, {} MiB heap capacity", self.pages.len()*TILE/(1<<20), self.pools.len()*POOL_TILES as usize*TILE/(1<<20)); }
        Ok(())
    }
}

pub(super) fn forget(resource: usize) {
    let mut g = state().lock().unwrap();
    if let Some(s) = g.as_mut() { if let Some(keys) = s.resources.remove(&resource) { s.release_pages(&keys); } }
}

/// None means a resource outside this experimental path's supported shapes.
pub(super) unsafe fn place(device: *mut c_void, va: usize, desc: &D3D12_RESOURCE_DESC, initial: u32, clear: *const c_void, out: *mut *mut c_void) -> Option<i32> {
    if desc.Dimension != D3D12_RESOURCE_DIMENSION_TEXTURE2D || desc.SampleDesc.Count != 1 || !va.is_multiple_of(TILE) { return None; }
    let dev = ID3D12Device::from_raw_borrowed(&device)?;
    let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS::default();
    if dev.CheckFeatureSupport(D3D12_FEATURE_D3D12_OPTIONS, &mut options as *mut _ as _, std::mem::size_of_val(&options) as u32).is_err() || options.TiledResourcesTier.0 < 2 || options.ResourceHeapTier.0 < 2 { return None; }
    let mut format = D3D12_FEATURE_DATA_FORMAT_SUPPORT { Format: desc.Format, ..Default::default() };
    if dev.CheckFeatureSupport(D3D12_FEATURE_FORMAT_SUPPORT, &mut format as *mut _ as _, std::mem::size_of_val(&format) as u32).is_err()
        || !format.Support2.contains(D3D12_FORMAT_SUPPORT2_TILED) {
        static SEEN: OnceLock<Mutex<std::collections::HashSet<i32>>> = OnceLock::new();
        if SEEN.get_or_init(Default::default).lock().unwrap().insert(desc.Format.0) {
            crate::xlog_info!("D3D12.X: format {} has no tiled-resource support; retaining the placed-resource path", desc.Format.0);
        }
        return None;
    }
    let mut d = *desc; d.Alignment = 0; d.Layout = D3D12_TEXTURE_LAYOUT_64KB_UNDEFINED_SWIZZLE;
    type Create = unsafe extern "system" fn(*mut c_void, *const D3D12_RESOURCE_DESC, D3D12_RESOURCE_STATES, *const c_void, *const windows::core::GUID, *mut *mut c_void) -> i32;
    let method = if super::real::is_adopted(device) { super::real::orig_fn(device, 30) } else { *(*(device as *const *const usize)).add(30) };
    let create: Create = std::mem::transmute(method);
    let mut raw = std::ptr::null_mut();
    let hr = create(device, &d, D3D12_RESOURCE_STATES(initial as i32), clear, &ID3D12Resource::IID, &mut raw);
    if hr < 0 || raw.is_null() { crate::xlog_warn!("tiled texture creation failed {hr:#x}: {}x{}x{} fmt {} flags {:#x}", d.Width, d.Height, d.DepthOrArraySize, d.Format.0, d.Flags.0); return Some(hr); }
    let resource = ID3D12Resource::from_raw(raw);
    let mapped = {
        let mut g = state().lock().unwrap();
        if g.is_none() {
            match Tiles::new(dev) { Ok(s) => *g = Some(s), Err(e) => { crate::xlog_error!("texture tile queue: {e}"); return Some(0x80004005u32 as i32); } }
        }
        g.as_mut().unwrap().map(&resource, va)
    };
    if let Err(e) = mapped { crate::xlog_error!("tiled texture mapping: {e}"); return Some(0x80004005u32 as i32); }
    let raw = resource.into_raw();
    super::real::adopt(raw, super::real::Kind::Resource);
    super::depth_uav::note(raw, initial);
    super::real::note_title_address(raw as usize, va);
    *out = raw;
    Some(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{transmute_copy, ManuallyDrop};
    use windows::Win32::Graphics::{Direct3D::D3D_FEATURE_LEVEL_11_0, Dxgi::Common::*};

    unsafe fn alias(a: &ID3D12Resource, b: &ID3D12Resource) -> D3D12_RESOURCE_BARRIER {
        D3D12_RESOURCE_BARRIER { Type: D3D12_RESOURCE_BARRIER_TYPE_ALIASING, Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
            Anonymous: D3D12_RESOURCE_BARRIER_0 { Aliasing: ManuallyDrop::new(D3D12_RESOURCE_ALIASING_BARRIER {
                pResourceBefore: transmute_copy(a), pResourceAfter: transmute_copy(b),
            }) } }
    }

    #[test]
    fn tiled_texture_aliases_preserve_gpu_writes_across_array_slices() {
        unsafe {
            let mut debug: Option<ID3D12Debug> = None; D3D12GetDebugInterface(&mut debug).unwrap(); debug.unwrap().EnableDebugLayer();
            let mut dev: Option<ID3D12Device> = None; D3D12CreateDevice(None,D3D_FEATURE_LEVEL_11_0,&mut dev).unwrap(); let dev=dev.unwrap();
            let info: ID3D12InfoQueue=dev.cast().unwrap();
            for f in [DXGI_FORMAT_R32G8X24_TYPELESS,DXGI_FORMAT_D32_FLOAT_S8X24_UINT,DXGI_FORMAT_R16_TYPELESS,DXGI_FORMAT_R32_UINT] {
                let mut supported=D3D12_FEATURE_DATA_FORMAT_SUPPORT {Format:f,..Default::default()};
                dev.CheckFeatureSupport(D3D12_FEATURE_FORMAT_SUPPORT,&mut supported as *mut _ as _,std::mem::size_of_val(&supported) as u32).unwrap();
                println!("format {} tiled support: {}",f.0,supported.Support2.contains(D3D12_FORMAT_SUPPORT2_TILED));
            }
            let mut tiles=Tiles::new(&dev).unwrap();
            for depth in [false,true] {
                info.ClearStoredMessages();
                let (size,array)=if depth {(256u32,6u16)}else{(1024u32,20u16)};
                let d=D3D12_RESOURCE_DESC { Dimension:D3D12_RESOURCE_DIMENSION_TEXTURE2D,Width:size as u64,Height:size,
                    DepthOrArraySize:array,MipLevels:1,Format:if depth {DXGI_FORMAT_R16_TYPELESS}else{DXGI_FORMAT_R32_UINT},
                    SampleDesc:DXGI_SAMPLE_DESC {Count:1,Quality:0},Layout:D3D12_TEXTURE_LAYOUT_64KB_UNDEFINED_SWIZZLE,
                    Flags:if depth {D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL}else{D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS},..Default::default() };
                let mut a:Option<ID3D12Resource>=None; let mut b:Option<ID3D12Resource>=None;
                dev.CreateReservedResource(&d,if depth {D3D12_RESOURCE_STATE_DEPTH_WRITE}else{D3D12_RESOURCE_STATE_UNORDERED_ACCESS},None,&mut a).unwrap();
                dev.CreateReservedResource(&d,D3D12_RESOURCE_STATE_COPY_SOURCE,None,&mut b).unwrap();
                let (a,b)=(a.unwrap(),b.unwrap()); let va=0x1_0000_0000usize;
                tiles.map(&a,va).unwrap(); let page_count=tiles.pages.len(); let capacity=tiles.pools.len();
                tiles.map(&b,va).unwrap(); assert_eq!(tiles.pages.len(),page_count); assert_eq!(tiles.pools.len(),capacity);
                assert!(tiles.pages.values().all(|p|p.refs==2));
                if !depth { assert!(capacity>=2,"exercise mappings spanning several heaps"); }
                let alloc:ID3D12CommandAllocator=dev.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).unwrap();
                let list:ID3D12GraphicsCommandList=dev.CreateCommandList(0,D3D12_COMMAND_LIST_TYPE_DIRECT,&alloc,None).unwrap();
                let mut heaps=Vec::<ID3D12DescriptorHeap>::new();
                if depth {
                    let heap:ID3D12DescriptorHeap=dev.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {Type:D3D12_DESCRIPTOR_HEAP_TYPE_DSV,NumDescriptors:1,..Default::default()}).unwrap();
                    let handle=heap.GetCPUDescriptorHandleForHeapStart();
                    dev.CreateDepthStencilView(&a,Some(&D3D12_DEPTH_STENCIL_VIEW_DESC {Format:DXGI_FORMAT_D16_UNORM,ViewDimension:D3D12_DSV_DIMENSION_TEXTURE2DARRAY,
                        Anonymous:D3D12_DEPTH_STENCIL_VIEW_DESC_0 {Texture2DArray:D3D12_TEX2D_ARRAY_DSV {MipSlice:0,FirstArraySlice:0,ArraySize:array as u32}},..Default::default()}),handle);
                    list.ClearDepthStencilView(handle,D3D12_CLEAR_FLAG_DEPTH,0.5,0,None); heaps.push(heap);
                } else {
                    let gpu:ID3D12DescriptorHeap=dev.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {Type:D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,NumDescriptors:1,Flags:D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,..Default::default()}).unwrap();
                    let cpu:ID3D12DescriptorHeap=dev.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {Type:D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,NumDescriptors:1,..Default::default()}).unwrap();
                    let view=D3D12_UNORDERED_ACCESS_VIEW_DESC {Format:DXGI_FORMAT_R32_UINT,ViewDimension:D3D12_UAV_DIMENSION_TEXTURE2DARRAY,
                        Anonymous:D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {Texture2DArray:D3D12_TEX2D_ARRAY_UAV {MipSlice:0,FirstArraySlice:0,ArraySize:array as u32,PlaneSlice:0}}};
                    dev.CreateUnorderedAccessView(&a,None,Some(&view),gpu.GetCPUDescriptorHandleForHeapStart());
                    dev.CreateUnorderedAccessView(&a,None,Some(&view),cpu.GetCPUDescriptorHandleForHeapStart());
                    list.SetDescriptorHeaps(&[Some(gpu.clone())]);
                    list.ClearUnorderedAccessViewUint(gpu.GetGPUDescriptorHandleForHeapStart(),cpu.GetCPUDescriptorHandleForHeapStart(),&a,&[0x1234abcd,0,0,0],&[]);
                    heaps.extend([gpu,cpu]);
                }
                if depth {
                    // Depth/stencil alias activation requires initialization of
                    // the newly active resource. Verify clear/readback on A;
                    // cross-resource data inheritance is tested with the UAV.
                    list.ResourceBarrier(&[D3D12_RESOURCE_BARRIER {Type:D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,Flags:D3D12_RESOURCE_BARRIER_FLAG_NONE,
                        Anonymous:D3D12_RESOURCE_BARRIER_0 {Transition:ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {pResource:transmute_copy(&a),Subresource:u32::MAX,StateBefore:D3D12_RESOURCE_STATE_DEPTH_WRITE,StateAfter:D3D12_RESOURCE_STATE_COPY_SOURCE})}}]);
                } else { list.ResourceBarrier(&[alias(&a,&b)]); }
                let mut footprints=vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();array as usize];let mut total=0;
                dev.GetCopyableFootprints(&d,0,array as u32,0,Some(footprints.as_mut_ptr()),None,None,Some(&mut total));
                let mut readback:Option<ID3D12Resource>=None;
                dev.CreateCommittedResource(&D3D12_HEAP_PROPERTIES {Type:D3D12_HEAP_TYPE_READBACK,..Default::default()},D3D12_HEAP_FLAG_NONE,
                    &D3D12_RESOURCE_DESC {Dimension:D3D12_RESOURCE_DIMENSION_BUFFER,Width:total,Height:1,DepthOrArraySize:1,MipLevels:1,SampleDesc:DXGI_SAMPLE_DESC{Count:1,Quality:0},Layout:D3D12_TEXTURE_LAYOUT_ROW_MAJOR,..Default::default()},
                    D3D12_RESOURCE_STATE_COPY_DEST,None,&mut readback).unwrap();let readback=readback.unwrap();
                for (sub,fp) in footprints.iter().enumerate() {
                    let dst=D3D12_TEXTURE_COPY_LOCATION {pResource:transmute_copy(&readback),Type:D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,Anonymous:D3D12_TEXTURE_COPY_LOCATION_0 {PlacedFootprint:*fp}};
                    let src=D3D12_TEXTURE_COPY_LOCATION {pResource:transmute_copy(if depth {&a}else{&b}),Type:D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,Anonymous:D3D12_TEXTURE_COPY_LOCATION_0 {SubresourceIndex:sub as u32}};
                    list.CopyTextureRegion(&dst,0,0,0,&src,None);
                }
                list.Close().unwrap();tiles.queue.ExecuteCommandLists(&[Some(list.cast().unwrap())]);tiles.serial+=1;tiles.queue.Signal(&tiles.fence,tiles.serial).unwrap();
                let start=std::time::Instant::now();while tiles.fence.GetCompletedValue()<tiles.serial {assert!(start.elapsed().as_secs()<10);std::thread::sleep(std::time::Duration::from_millis(1));}
                dev.GetDeviceRemovedReason().unwrap();let mut ptr=std::ptr::null_mut();readback.Map(0,None,Some(&mut ptr)).unwrap();
                for (sub,fp) in footprints.iter().enumerate() {for y in 0..size as usize {for x in 0..size as usize {
                    let at=ptr.cast::<u8>().add(fp.Offset as usize+y*fp.Footprint.RowPitch as usize+x*if depth {2}else{4});
                    if depth {let actual=at.cast::<u16>().read_unaligned();assert!((actual as i32-32768).abs()<=1,"D16 alias slice {sub}, {x},{y}: {actual}");}
                    else {assert_eq!(at.cast::<u32>().read_unaligned(),0x1234abcd,"UAV alias slice {sub}, {x},{y}");}
                }}}
                readback.Unmap(0,None);
                for i in 0..info.GetNumStoredMessagesAllowedByRetrievalFilter() {
                    let mut bytes=0;info.GetMessage(i,None,&mut bytes).unwrap();let mut data=vec![0u64;bytes.div_ceil(8)];info.GetMessage(i,Some(data.as_mut_ptr().cast()),&mut bytes).unwrap();
                    let msg=&*data.as_ptr().cast::<D3D12_MESSAGE>();assert!(msg.Severity!=D3D12_MESSAGE_SEVERITY_ERROR&&msg.Severity!=D3D12_MESSAGE_SEVERITY_CORRUPTION,"{}",std::ffi::CStr::from_ptr(msg.pDescription.cast()).to_string_lossy());
                }
                let ak=tiles.resources.remove(&(a.as_raw() as usize)).unwrap(); tiles.release_pages(&ak); assert_eq!(tiles.pages.len(),page_count);
                let bk=tiles.resources.remove(&(b.as_raw() as usize)).unwrap(); tiles.release_pages(&bk); assert!(tiles.pages.is_empty());
                println!("{}: all pixels of {array} slices preserved, {page_count} unique pages",if depth {"D16 tiled clear/readback"}else{"R32 UAV tiled alias inheritance"});
            }
        }
    }
}
