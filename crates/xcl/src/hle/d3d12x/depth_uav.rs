// Experimental changes: Speedstu, 2026-11-23/24. See LOADING-TRIAL.md for provenance.
//! Xbox depth/UAV textures need two PC resources. Copy whole subresources at
//! logical UAV transitions; never bind a PC depth resource as a UAV.
use std::{collections::HashMap, ffi::c_void, mem::transmute_copy, sync::{Mutex, OnceLock}};
use windows::{core::Interface, Win32::Graphics::{Direct3D12::*, Dxgi::Common::*}};

struct Entry {
    states: Vec<u32>,
    depth_subresources: usize,
    shadows: [Option<ID3D12Resource>; 2],
    conversion: Option<Conversion>,
}
static ENTRIES: OnceLock<Mutex<HashMap<usize, Entry>>> = OnceLock::new();
fn entries() -> &'static Mutex<HashMap<usize, Entry>> { ENTRIES.get_or_init(Default::default) }

pub(super) unsafe fn note(resource: *mut c_void, initial: u32) {
    super::resource_states::note(resource, initial);
    let Some(r) = ID3D12Resource::from_raw_borrowed(&resource) else { return };
    let d = r.GetDesc();
    if d.Flags.0 & D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL.0 == 0 { return; }
    let count = d.MipLevels as usize * d.DepthOrArraySize as usize;
    let planes = if matches!(d.Format.0, 19 | 20 | 44 | 45) { 2 } else { 1 };
    entries().lock().unwrap().insert(resource as usize, Entry { states: vec![initial; count * planes], depth_subresources: count, shadows: [None, None], conversion: None });
}

pub(super) fn forget(resource: usize) {
    let old = entries().lock().unwrap().remove(&resource);
    drop(old); // COM release may re-enter resource cleanup.
}

unsafe fn native<T: Copy>(object: *mut c_void, slot: usize) -> T {
    let p = if super::real::is_adopted(object) { super::real::orig_fn(object, slot) }
        else { *(*(object as *const *const usize)).add(slot) };
    transmute_copy(&p)
}

pub(super) unsafe fn view(device: *mut c_void, resource: *mut c_void, format: Option<i32>) -> Result<Option<ID3D12Resource>, String> {
    let Some(r) = ID3D12Resource::from_raw_borrowed(&resource) else { return Ok(None) };
    let mut d = r.GetDesc();
    if d.Flags.0 & D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL.0 == 0 { return Ok(None); }
    let plane = usize::from(matches!(format, Some(22 | 47 | 60 | 62 | 120)));
    let mut all = entries().lock().unwrap();
    let e = all.get_mut(&(resource as usize)).ok_or("untracked depth resource initial state")?;
    if plane == 1 && e.states.len() == e.depth_subresources { return Err("stencil view of a resource without stencil".into()); }
    if let Some(s) = &e.shadows[plane] { return Ok(Some(s.clone())); }
    if plane == 0 && matches!(d.Format.0, 44 | 45) {
        if format != Some(56) { return Err(format!("D24 depth conversion needs the Xbox R16_UNORM view, got {format:?}")); }
        e.conversion = Some(Conversion::new(device, &d)?);
    }
    d.Format = if e.conversion.is_some() && plane == 0 { DXGI_FORMAT_R16_TYPELESS } else if plane == 1 { DXGI_FORMAT_R8_TYPELESS } else { match d.Format.0 { 53 | 55 => DXGI_FORMAT_R16_TYPELESS, 19 | 20 | 39 | 40 => DXGI_FORMAT_R32_TYPELESS,
        f => return Err(format!("depth/UAV format {f} requires a separate conversion path")) } };
    if d.SampleDesc.Count != 1 { return Err("multisample depth/UAV is unsupported".into()); }
    d.Alignment = 0;
    d.Layout = D3D12_TEXTURE_LAYOUT_UNKNOWN;
    d.Flags = D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS;
    type Create = unsafe extern "system" fn(*mut c_void, *const D3D12_HEAP_PROPERTIES, D3D12_HEAP_FLAGS, *const D3D12_RESOURCE_DESC, D3D12_RESOURCE_STATES, *const D3D12_CLEAR_VALUE, *const windows::core::GUID, *mut *mut c_void) -> i32;
    let create: Create = native(device, 27);
    let mut raw = std::ptr::null_mut();
    let hr = create(device, &D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() }, D3D12_HEAP_FLAG_NONE, &d, D3D12_RESOURCE_STATE_UNORDERED_ACCESS, std::ptr::null(), &ID3D12Resource::IID, &mut raw);
    if hr < 0 || raw.is_null() { return Err(format!("depth/UAV companion allocation failed: {hr:#x}")); }
    let shadow = ID3D12Resource::from_raw(raw);
    crate::xlog_info!("D3D12.X: depth/UAV companion {resource:p} -> {raw:p}, {}x{}x{} format {}", d.Width, d.Height, d.DepthOrArraySize, d.Format.0);
    e.shadows[plane] = Some(shadow.clone());
    Ok(Some(shadow))
}

unsafe fn barrier(list: *mut c_void, resource: usize, sub: u32, before: u32, after: u32) {
    if before == after { return; }
    let b = [0, 0, resource as u32, (resource >> 32) as u32, sub, before, after, 0];
    let f: unsafe extern "system" fn(*mut c_void, u32, *const [u32; 8]) = native(list, 26);
    f(list, 1, &b);
}

unsafe fn copy_sub(list: *mut c_void, dst: usize, dst_sub: u32, src: usize, src_sub: u32) {
    let loc = |r: usize, sub: u32| D3D12_TEXTURE_COPY_LOCATION {
        pResource: transmute_copy(&r), Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: sub },
    };
    let f: unsafe extern "system" fn(*mut c_void, *const D3D12_TEXTURE_COPY_LOCATION, u32, u32, u32, *const D3D12_TEXTURE_COPY_LOCATION, *const D3D12_BOX) = native(list, 16);
    f(list, &loc(dst, dst_sub), 0, 0, 0, &loc(src, src_sub), std::ptr::null());
}

/// Consumes a depth transition, including those recorded before a UAV exists.
/// Caller must flush earlier barriers first to preserve command ordering.
pub(super) unsafe fn transition(list: *mut c_void, b: &[u32; 8]) -> Result<bool, String> {
    let resource = b[2] as usize | (b[3] as usize) << 32;
    let mut all = entries().lock().unwrap();
    let Some(e) = all.get_mut(&resource) else { return Ok(false) };
    if b[1] != 0 { return Err(format!("split depth/UAV transition flags {:#x} are unsupported", b[1])); }
    let subs: Vec<usize> = if b[4] == u32::MAX { (0..e.states.len()).collect() } else { vec![b[4] as usize] };
    let (before, after) = (b[5], b[6]);
    for sub in subs {
        let state = e.states.get_mut(sub).ok_or("depth/UAV subresource index outside allocation")?;
        let plane = sub / e.depth_subresources;
        let shadow_sub = (sub % e.depth_subresources) as u32;
        if let Some(shadow) = &e.shadows[plane] {
            let s = shadow.as_raw() as usize;
            if before & 8 != 0 && after & 8 == 0 {
                barrier(list, s, shadow_sub, 8, 0x800);
                barrier(list, resource, sub as u32, *state, 0x400);
                if plane == 0 && e.conversion.is_some() {
                    e.conversion.as_ref().unwrap().transfer(list, resource, sub as u32, s, shadow_sub, true)?;
                } else { copy_sub(list, resource, sub as u32, s, shadow_sub); }
                barrier(list, resource, sub as u32, 0x400, after);
                barrier(list, s, shadow_sub, 0x800, 8);
                *state = after;
            } else if before & 8 == 0 && after & 8 != 0 {
                barrier(list, resource, sub as u32, *state, 0x800);
                barrier(list, s, shadow_sub, 8, 0x400);
                if plane == 0 && e.conversion.is_some() {
                    e.conversion.as_ref().unwrap().transfer(list, resource, sub as u32, s, shadow_sub, false)?;
                } else { copy_sub(list, s, shadow_sub, resource, sub as u32); }
                barrier(list, s, shadow_sub, 0x400, 8);
                barrier(list, resource, sub as u32, 0x800, *state);
            } else if after & 8 == 0 {
                barrier(list, resource, sub as u32, *state, after);
                *state = after;
            } else {
                let uav = [2, 0, s as u32, (s >> 32) as u32, 0, 0, 0, 0];
                let f: unsafe extern "system" fn(*mut c_void, u32, *const [u32; 8]) = native(list, 26);
                f(list, 1, &uav);
            }
        } else {
            let next = if after & 8 != 0 { 16 } else { after };
            barrier(list, resource, sub as u32, *state, next);
            *state = next;
        }
    }
    Ok(true)
}


/// D16S8 is represented by D24S8 on Windows. Stage the depth plane through raw
/// GPU buffers, quantizing normalized depth while the stencil plane stays put.
struct Conversion {
    dev: ID3D12Device,
    root: ID3D12RootSignature,
    to16: ID3D12PipelineState,
    to24: ID3D12PipelineState,
    packed24: ID3D12Resource,
    packed16: ID3D12Resource,
}
impl Conversion {
    unsafe fn new(device: *mut c_void, d: &D3D12_RESOURCE_DESC) -> Result<Self, String> {
        let dev = ID3D12Device::from_raw_borrowed(&device).ok_or("conversion device")?.clone();
        let params = [
            D3D12_ROOT_PARAMETER { ParameterType: D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS, Anonymous: D3D12_ROOT_PARAMETER_0 { Constants: D3D12_ROOT_CONSTANTS { ShaderRegister: 0, RegisterSpace: 0, Num32BitValues: 4 } }, ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL },
            D3D12_ROOT_PARAMETER { ParameterType: D3D12_ROOT_PARAMETER_TYPE_SRV, Anonymous: D3D12_ROOT_PARAMETER_0 { Descriptor: D3D12_ROOT_DESCRIPTOR { ShaderRegister: 0, RegisterSpace: 0 } }, ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL },
            D3D12_ROOT_PARAMETER { ParameterType: D3D12_ROOT_PARAMETER_TYPE_UAV, Anonymous: D3D12_ROOT_PARAMETER_0 { Descriptor: D3D12_ROOT_DESCRIPTOR { ShaderRegister: 0, RegisterSpace: 0 } }, ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL },
        ];
        let desc = D3D12_ROOT_SIGNATURE_DESC { NumParameters: 3, pParameters: params.as_ptr(), ..Default::default() };
        let mut blob = None;
        D3D12SerializeRootSignature(&desc, D3D_ROOT_SIGNATURE_VERSION_1, &mut blob, None).map_err(|e|e.to_string())?;
        let blob = blob.unwrap();
        let f: unsafe extern "system" fn(*mut c_void, u32, *const c_void, usize, *const windows::core::GUID, *mut *mut c_void) -> i32 = native(device, 16);
        let mut raw = std::ptr::null_mut();
        let hr = f(device, 0, blob.GetBufferPointer(), blob.GetBufferSize(), &ID3D12RootSignature::IID, &mut raw);
        if hr < 0 { return Err(format!("depth conversion root signature {hr:#x}")); }
        let root = ID3D12RootSignature::from_raw(raw);
        let shader = r#"
cbuffer Shape : register(b0) { uint width, height, pitch24, pitch16; }
ByteAddressBuffer src : register(t0);
RWByteAddressBuffer dst : register(u0);
[numthreads(8,8,1)] void to16(uint3 id : SV_DispatchThreadID) {
 uint x=id.x*2, y=id.y; if(x>=width || y>=height) return;
 uint a=src.Load(y*pitch24+x*4)&0xffffff;
 uint b=x+1<width ? src.Load(y*pitch24+(x+1)*4)&0xffffff : 0;
 uint lo=(uint)round(float(a)*(65535.0/16777215.0));
 uint hi=(uint)round(float(b)*(65535.0/16777215.0));
 dst.Store(y*pitch16+x*2,lo|(hi<<16));
}
[numthreads(8,8,1)] void to24(uint3 id : SV_DispatchThreadID) {
 uint x=id.x,y=id.y; if(x>=width || y>=height) return;
 uint v=(src.Load(y*pitch16+(x/2)*4)>>((x&1)*16))&65535;
 dst.Store(y*pitch24+x*4,v*256+(v+128)/257);
}
"#;
        let build = |entry: &str| -> Result<ID3D12PipelineState,String> {
            let bytes = crate::gpu::recomp::dxc::urgent(|| crate::gpu::recomp::dxc::compile(shader, entry, "cs_6_0", &[]))?;
            let desc = D3D12_COMPUTE_PIPELINE_STATE_DESC { pRootSignature: transmute_copy(&root), CS: D3D12_SHADER_BYTECODE { pShaderBytecode: bytes.as_ptr().cast(), BytecodeLength: bytes.len() }, ..Default::default() };
            let f: unsafe extern "system" fn(*mut c_void, *const D3D12_COMPUTE_PIPELINE_STATE_DESC, *const windows::core::GUID, *mut *mut c_void) -> i32 = native(device, 11);
            let mut raw = std::ptr::null_mut();
            let hr = f(device, &desc, &ID3D12PipelineState::IID, &mut raw);
            if hr < 0 { return Err(format!("depth conversion pipeline {entry} {hr:#x}")); }
            Ok(ID3D12PipelineState::from_raw(raw))
        };
        let (to16, to24) = (build("to16")?, build("to24")?);
        let make_buffer = |format: DXGI_FORMAT| -> Result<ID3D12Resource,String> {
            let mut shape=*d; shape.Format=format; shape.Flags=D3D12_RESOURCE_FLAG_NONE;
            let mut size=0;
            dev.GetCopyableFootprints(&shape, 0, 1, 0, None, None, None, Some(&mut size));
            let desc=D3D12_RESOURCE_DESC { Dimension:D3D12_RESOURCE_DIMENSION_BUFFER, Width:size.next_multiple_of(256), Height:1, DepthOrArraySize:1, MipLevels:1, SampleDesc:DXGI_SAMPLE_DESC{Count:1,Quality:0}, Layout:D3D12_TEXTURE_LAYOUT_ROW_MAJOR, Flags:D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS, ..Default::default() };
            let f: unsafe extern "system" fn(*mut c_void,*const D3D12_HEAP_PROPERTIES,D3D12_HEAP_FLAGS,*const D3D12_RESOURCE_DESC,D3D12_RESOURCE_STATES,*const D3D12_CLEAR_VALUE,*const windows::core::GUID,*mut *mut c_void)->i32=native(device,27);
            let mut raw=std::ptr::null_mut();
            let hr=f(device,&D3D12_HEAP_PROPERTIES{Type:D3D12_HEAP_TYPE_DEFAULT,..Default::default()},D3D12_HEAP_FLAG_NONE,&desc,D3D12_RESOURCE_STATE_COMMON,std::ptr::null(),&ID3D12Resource::IID,&mut raw);
            if hr<0 { return Err(format!("depth conversion buffer {hr:#x}")); }
            Ok(ID3D12Resource::from_raw(raw))
        };
        // D24's copy plane has R32_TYPELESS storage and a 512-byte-aligned pitch.
        let packed24=make_buffer(d.Format)?;
        let packed16=make_buffer(DXGI_FORMAT_R16_TYPELESS)?;
        Ok(Self {dev, root, to16, to24, packed24, packed16})
    }
    unsafe fn transfer(&self, list:*mut c_void, depth:usize, sub:u32, shadow:usize, shadow_sub:u32, to_depth:bool)->Result<(),String> {
        let desc=|r:usize| ID3D12Resource::from_raw_borrowed(&(r as *mut c_void)).unwrap().GetDesc();
        let mut fp24=D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
        let mut fp16=D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
        self.dev.GetCopyableFootprints(&desc(depth),sub,1,0,Some(&mut fp24),None,None,None);
        self.dev.GetCopyableFootprints(&desc(shadow),shadow_sub,1,0,Some(&mut fp16),None,None,None);
        let (b24,b16)=(self.packed24.as_raw() as usize,self.packed16.as_raw() as usize);
        let fp=|r:usize,footprint| D3D12_TEXTURE_COPY_LOCATION{pResource:transmute_copy(&r),Type:D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,Anonymous:D3D12_TEXTURE_COPY_LOCATION_0{PlacedFootprint:footprint}};
        let tex=|r:usize,s| D3D12_TEXTURE_COPY_LOCATION{pResource:transmute_copy(&r),Type:D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,Anonymous:D3D12_TEXTURE_COPY_LOCATION_0{SubresourceIndex:s}};
        let cp:unsafe extern "system" fn(*mut c_void,*const D3D12_TEXTURE_COPY_LOCATION,u32,u32,u32,*const D3D12_TEXTURE_COPY_LOCATION,*const D3D12_BOX)=native(list,16);
        let (source,dest)=if to_depth {(b16,b24)}else{(b24,b16)};
        barrier(list,source,0,0,0x400);
        if to_depth {cp(list,&fp(b16,fp16),0,0,0,&tex(shadow,shadow_sub),std::ptr::null());}
        else {cp(list,&fp(b24,fp24),0,0,0,&tex(depth,sub),std::ptr::null());}
        barrier(list,source,0,0x400,0x40);
        barrier(list,dest,0,0,8);
        let set_rs:unsafe extern "system" fn(*mut c_void,*mut c_void)=native(list,29);
        let set_pso:unsafe extern "system" fn(*mut c_void,*mut c_void)=native(list,25);
        let constants:unsafe extern "system" fn(*mut c_void,u32,u32,*const u32,u32)=native(list,35);
        let srv:unsafe extern "system" fn(*mut c_void,u32,u64)=native(list,39);
        let uav:unsafe extern "system" fn(*mut c_void,u32,u64)=native(list,41);
        let dispatch:unsafe extern "system" fn(*mut c_void,u32,u32,u32)=native(list,14);
        set_rs(list,self.root.as_raw());
        set_pso(list,if to_depth{self.to24.as_raw()}else{self.to16.as_raw()});
        constants(list,0,4,[fp16.Footprint.Width,fp16.Footprint.Height,fp24.Footprint.RowPitch,fp16.Footprint.RowPitch].as_ptr(),0);
        srv(list,1,if to_depth{self.packed16.GetGPUVirtualAddress()}else{self.packed24.GetGPUVirtualAddress()});
        uav(list,2,if to_depth{self.packed24.GetGPUVirtualAddress()}else{self.packed16.GetGPUVirtualAddress()});
        dispatch(list,if to_depth{fp16.Footprint.Width.div_ceil(8)}else{fp16.Footprint.Width.div_ceil(16)},fp16.Footprint.Height.div_ceil(8),1);
        barrier(list,dest,0,8,0x800);
        if to_depth {cp(list,&tex(depth,sub),0,0,0,&fp(b24,fp24),std::ptr::null());}
        else {cp(list,&tex(shadow,shadow_sub),0,0,0,&fp(b16,fp16),std::ptr::null());}
        barrier(list,source,0,0x40,0);
        barrier(list,dest,0,0x800,0);
        // Graphics roots/heaps never changed. Console compute calls bind all their
        // saved root arguments afresh; restore the prior PSO for native graphics.
        if super::real::is_adopted(list) {
            let prior=super::bind::tracked_pso(list);
            if prior!=0 {super::real::bind_pipeline(list,prior as *mut c_void);}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0;

    #[test]
    fn depth_uav_gpu_roundtrip_preserves_all_array_slices() {
        roundtrip(0);
    }

    #[test]
    fn d32s8_depth_plane_uav_gpu_roundtrip() {
        roundtrip(1);
    }

    #[test]
    fn d24s8_stencil_plane_uav_gpu_roundtrip() { roundtrip(2); }

    #[test]
    fn d16s8_depth_normalization_gpu_roundtrip() { roundtrip(3); }

    fn roundtrip(kind: u8) {
        let wide = kind == 1;
        let normalize = kind == 3;
        let stencil = kind == 2;
        unsafe {
            let mut debug: Option<ID3D12Debug> = None;
            D3D12GetDebugInterface(&mut debug).unwrap();
            debug.unwrap().EnableDebugLayer();
            let mut device: Option<ID3D12Device> = None;
            D3D12CreateDevice(None, D3D_FEATURE_LEVEL_11_0, &mut device).unwrap();
            let dev = device.unwrap();
            let info: ID3D12InfoQueue = dev.cast().unwrap();
            for array in [1u16, 6] {
                info.ClearStoredMessages();
                let d = D3D12_RESOURCE_DESC {
                    Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D, Width: 4, Height: 4,
                    DepthOrArraySize: array, MipLevels: 1, Format: if stencil || normalize { DXGI_FORMAT_R24G8_TYPELESS } else if wide { DXGI_FORMAT_R32G8X24_TYPELESS } else { DXGI_FORMAT_R16_TYPELESS },
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Flags: D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL, ..Default::default()
                };
                let mut depth: Option<ID3D12Resource> = None;
                dev.CreateCommittedResource(&D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() }, D3D12_HEAP_FLAG_NONE, &d, D3D12_RESOURCE_STATE_DEPTH_WRITE, None, &mut depth).unwrap();
                let depth = depth.unwrap();
                if wide {
                    let heap: ID3D12DescriptorHeap = dev.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC { Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, NumDescriptors: 2, ..Default::default() }).unwrap();
                    let start = heap.GetCPUDescriptorHandleForHeapStart().ptr;
                    let step = dev.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV) as usize;
                    for (i, requested) in [41, 62].into_iter().enumerate() {
                        let (format, plane) = super::super::real::pc_depth_srv(19, requested).unwrap();
                        let srv = D3D12_SHADER_RESOURCE_VIEW_DESC { Format: DXGI_FORMAT(format), ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2DARRAY,
                            Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                            Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 { Texture2DArray: D3D12_TEX2D_ARRAY_SRV { MostDetailedMip: 0, MipLevels: 1, FirstArraySlice: 0, ArraySize: array as u32, PlaneSlice: plane, ResourceMinLODClamp: 0.0 } } };
                        dev.CreateShaderResourceView(&depth, Some(&srv), D3D12_CPU_DESCRIPTOR_HANDLE { ptr: start + i * step });
                    }
                }
                let raw = depth.as_raw();
                note(raw, 16);
                let shadow = view(dev.as_raw(), raw, if stencil {Some(62)}else if normalize {Some(56)}else{None}).unwrap().unwrap();
                let dsv_heap: ID3D12DescriptorHeap = dev.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC { Type: D3D12_DESCRIPTOR_HEAP_TYPE_DSV, NumDescriptors: 1, ..Default::default() }).unwrap();
                let dsv = dsv_heap.GetCPUDescriptorHandleForHeapStart();
                dev.CreateDepthStencilView(&depth, Some(&D3D12_DEPTH_STENCIL_VIEW_DESC {
                    Format: if stencil || normalize { DXGI_FORMAT_D24_UNORM_S8_UINT } else if wide { DXGI_FORMAT_D32_FLOAT_S8X24_UINT } else { DXGI_FORMAT_D16_UNORM }, ViewDimension: D3D12_DSV_DIMENSION_TEXTURE2DARRAY,
                    Anonymous: D3D12_DEPTH_STENCIL_VIEW_DESC_0 { Texture2DArray: D3D12_TEX2D_ARRAY_DSV { MipSlice: 0, FirstArraySlice: 0, ArraySize: array as u32 } }, ..Default::default()
                }), dsv);
                let gpu_heap: ID3D12DescriptorHeap = dev.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC { Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, NumDescriptors: 1, Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE, ..Default::default() }).unwrap();
                let cpu_heap: ID3D12DescriptorHeap = dev.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC { Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, NumDescriptors: 1, ..Default::default() }).unwrap();
                let uav = D3D12_UNORDERED_ACCESS_VIEW_DESC { Format: if stencil { DXGI_FORMAT_R8_UINT } else if wide { DXGI_FORMAT_R32_FLOAT } else { DXGI_FORMAT_R16_UNORM }, ViewDimension: D3D12_UAV_DIMENSION_TEXTURE2DARRAY,
                    Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 { Texture2DArray: D3D12_TEX2D_ARRAY_UAV { MipSlice: 0, FirstArraySlice: 0, ArraySize: array as u32, PlaneSlice: 0 } } };
                dev.CreateUnorderedAccessView(&shadow, None, Some(&uav), gpu_heap.GetCPUDescriptorHandleForHeapStart());
                dev.CreateUnorderedAccessView(&shadow, None, Some(&uav), cpu_heap.GetCPUDescriptorHandleForHeapStart());
                let mut readback: Option<ID3D12Resource> = None;
                dev.CreateCommittedResource(&D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_READBACK, ..Default::default() }, D3D12_HEAP_FLAG_NONE,
                    &D3D12_RESOURCE_DESC { Dimension: D3D12_RESOURCE_DIMENSION_BUFFER, Width: array as u64 * 12288, Height: 1, DepthOrArraySize: 1, MipLevels: 1, SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 }, Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR, ..Default::default() },
                    D3D12_RESOURCE_STATE_COPY_DEST, None, &mut readback).unwrap();
                let readback = readback.unwrap();
                let queue: ID3D12CommandQueue = dev.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC::default()).unwrap();
                let alloc: ID3D12CommandAllocator = dev.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).unwrap();
                let list: ID3D12GraphicsCommandList = dev.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &alloc, None).unwrap();
                let lr = list.as_raw();
                let logical = |before, after| [0, 0, raw as usize as u32, ((raw as usize) >> 32) as u32, u32::MAX, before, after, 0];
                let snapshot = |resource: &ID3D12Resource, pass: u64| {
                    for sub in 0..array as u32 {
                        let dst = D3D12_TEXTURE_COPY_LOCATION { pResource: transmute_copy(&(readback.as_raw() as usize)), Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT { Offset: pass * array as u64 * 4096 + sub as u64 * 4096,
                                Footprint: D3D12_SUBRESOURCE_FOOTPRINT { Format: if stencil { DXGI_FORMAT_R8_UINT } else if wide { DXGI_FORMAT_R32_FLOAT } else { DXGI_FORMAT_R16_UNORM }, Width: 4, Height: 4, Depth: 1, RowPitch: 256 } } } };
                        let src = D3D12_TEXTURE_COPY_LOCATION { pResource: transmute_copy(&(resource.as_raw() as usize)), Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: sub } };
                        list.CopyTextureRegion(&dst, 0, 0, 0, &src, None);
                    }
                };
                list.ClearDepthStencilView(dsv, if wide || stencil || normalize { D3D12_CLEAR_FLAG_DEPTH | D3D12_CLEAR_FLAG_STENCIL } else { D3D12_CLEAR_FLAG_DEPTH }, 0.25, 0x5a, None);
                assert!(transition(lr, &logical(16, 8)).unwrap());
                barrier(lr, shadow.as_raw() as usize, u32::MAX, 8, 0x800);
                snapshot(&shadow, 0);
                barrier(lr, shadow.as_raw() as usize, u32::MAX, 0x800, 8);
                list.SetDescriptorHeaps(&[Some(gpu_heap.clone())]);
                if stencil { list.ClearUnorderedAccessViewUint(gpu_heap.GetGPUDescriptorHandleForHeapStart(), cpu_heap.GetCPUDescriptorHandleForHeapStart(), &shadow, &[0xa5, 0, 0, 0], &[]); } else { list.ClearUnorderedAccessViewFloat(gpu_heap.GetGPUDescriptorHandleForHeapStart(), cpu_heap.GetCPUDescriptorHandleForHeapStart(), &shadow, &[0.75, 0.0, 0.0, 0.0], &[]); }
                assert!(transition(lr, &logical(8, 16)).unwrap());
                barrier(lr, raw as usize, u32::MAX, 16, 0x800);
                if wide || stencil {
                    barrier(lr, shadow.as_raw() as usize, u32::MAX, 8, 0x400);
                    for sub in 0..array as u32 { copy_sub(lr, shadow.as_raw() as usize, sub, raw as usize, sub + if stencil {array as u32}else{0}); }
                    barrier(lr, shadow.as_raw() as usize, u32::MAX, 0x400, 0x800);
                    snapshot(&shadow, 1);
                } else if normalize {
                    // Normalize the final D24 plane back into the R16 companion.
                    barrier(lr, shadow.as_raw() as usize, u32::MAX, 8, 0x400);
                    let all=entries().lock().unwrap();
                    let conversion=all[&(raw as usize)].conversion.as_ref().unwrap();
                    for sub in 0..array as u32 { conversion.transfer(lr,raw as usize,sub,shadow.as_raw() as usize,sub,false).unwrap(); }
                    barrier(lr, shadow.as_raw() as usize, u32::MAX, 0x400, 0x800);
                    snapshot(&shadow,1);
                } else { snapshot(&depth, 1); }
                let mut stencil_footprints = Vec::new();
                if wide || stencil || normalize {
                    for sub in 0..array as u32 {
                        let mut fp = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
                        dev.GetCopyableFootprints(&d, if stencil {sub}else{array as u32 + sub}, 1, (2 * array as u64 + sub as u64) * 4096, Some(&mut fp), None, None, None);
                        let dst = D3D12_TEXTURE_COPY_LOCATION { pResource: transmute_copy(&(readback.as_raw() as usize)), Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT, Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: fp } };
                        let src = D3D12_TEXTURE_COPY_LOCATION { pResource: transmute_copy(&(raw as usize)), Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX, Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: if stencil {sub}else{array as u32 + sub} } };
                        list.CopyTextureRegion(&dst, 0, 0, 0, &src, None);
                        stencil_footprints.push(fp);
                    }
                }
                list.Close().unwrap();
                queue.ExecuteCommandLists(&[Some(list.cast().unwrap())]);
                let fence: ID3D12Fence = dev.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
                queue.Signal(&fence, 1).unwrap();
                let start = std::time::Instant::now();
                while fence.GetCompletedValue() < 1 { assert!(start.elapsed().as_secs() < 10, "GPU fence timeout"); std::thread::sleep(std::time::Duration::from_millis(1)); }
                dev.GetDeviceRemovedReason().unwrap();
                let mut mapped = std::ptr::null_mut();
                readback.Map(0, None, Some(&mut mapped)).unwrap();
                for (pass, expected) in [(0usize, 16384i32), (1, 49151)] {
                    for sub in 0..array as usize { for y in 0..4 { for x in 0..4 {
                        let offset = pass * array as usize * 4096 + sub * 4096 + y * 256 + x * if stencil {1} else if wide {4} else {2};
                        if stencil {
                            assert_eq!(*mapped.cast::<u8>().add(offset), if pass == 0 {0x5a}else{0xa5}, "stencil pass {pass} slice {sub}");
                        } else if wide {
                            let actual = mapped.cast::<u8>().add(offset).cast::<f32>().read_unaligned();
                            assert_eq!(actual, if pass==0 {0.25}else{0.75}, "D32S8 pass {pass} slice {sub} ({x},{y})");
                        } else {
                            let actual = (mapped.cast::<u8>().add(offset).cast::<u16>()).read_unaligned() as i32;
                            assert!((actual - expected).abs() <= 1, "pass {pass} slice {sub} ({x},{y}): {actual}, expected {expected}");
                        }
                    } } }
                }
                for (sub, fp) in stencil_footprints.iter().enumerate() {
                    if stencil {
                        println!("preserved depth footprint {:?}", fp.Footprint);
                        for y in 0..4 { for x in 0..4 {
                            let at = fp.Offset as usize + y * fp.Footprint.RowPitch as usize + x * 4;
                            let value = mapped.cast::<u8>().add(at).cast::<u32>().read_unaligned() & 0xffffff;
                            assert!((value as i64 - 4194304).abs() <= 1, "depth changed: {value:#x}");
                        }}
                        continue;
                    }
                    let (stride, byte) = match fp.Footprint.Format.0 { 60 | 62 => (1,0), 22 => (8,4), f => panic!("unexpected stencil footprint format {f}") };
                    for y in 0..4 { for x in 0..4 {
                        let at = fp.Offset as usize + y * fp.Footprint.RowPitch as usize + x * stride + byte;
                        assert_eq!(*mapped.cast::<u8>().add(at), 0x5a, "stencil changed: slice {sub}, {x},{y}");
                    } }
                }
                readback.Unmap(0, None);
                let errors = info.GetNumStoredMessagesAllowedByRetrievalFilter();
                for i in 0..errors {
                    let mut bytes = 0;
                    info.GetMessage(i, None, &mut bytes).unwrap();
                    let mut buf = vec![0u64; bytes.div_ceil(8)];
                    info.GetMessage(i, Some(buf.as_mut_ptr().cast()), &mut bytes).unwrap();
                    let message = &*(buf.as_ptr().cast::<D3D12_MESSAGE>());
                    assert!(message.Severity != D3D12_MESSAGE_SEVERITY_ERROR && message.Severity != D3D12_MESSAGE_SEVERITY_CORRUPTION,
                        "D3D validation: {}", std::ffi::CStr::from_ptr(message.pDescription.cast()).to_string_lossy());
                }
                forget(raw as usize);
                println!("GPU plane UAV round trip: kind {kind}, all {array} slices; companion changes and untouched plane verified");
            }
        }
    }
}
