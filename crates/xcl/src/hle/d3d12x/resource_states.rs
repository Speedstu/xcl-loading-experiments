// Experimental changes: Speedstu, 2026-11-23/24. See LOADING-TRIAL.md for provenance.
//! States of render-target textures, at recording time and in submission order.
use std::{collections::HashMap,ffi::c_void,sync::{Mutex,OnceLock}};
use windows::{core::Interface,Win32::Graphics::Direct3D12::*};
#[derive(Default)]
struct States { live:HashMap<usize,Vec<u32>>, lists:HashMap<usize,HashMap<usize,Vec<u32>>> }
static STATES:OnceLock<Mutex<States>>=OnceLock::new();
fn states()-> &'static Mutex<States>{STATES.get_or_init(Default::default)}
/// Xbox colour targets can stay in a console UAV state while being drawn to.
/// The PC draw needs RT state; restore the title's state after the native draw.
pub(super) unsafe fn with_render_targets<T>(list:*mut c_void,f:impl FnOnce()->T)->T {
    unsafe fn apply<T,F:FnOnce()->T>(list:*mut c_void,resources:&[usize],f:F)->T {
        match resources.split_first() {Some((&r,rest))=>with_state(list,r,4,||apply(list,rest,f)),None=>f()}
    }
    let resources=super::list_x::render_target_resources(list);
    apply(list,&resources,f)
}
pub(super) unsafe fn note(resource:*mut c_void,initial:u32){
    let Some(r)=ID3D12Resource::from_raw_borrowed(&resource)else{return};let d=r.GetDesc();
    if d.Flags.0&D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET.0==0{return;}
    states().lock().unwrap().live.insert(resource as usize,vec![initial;d.MipLevels as usize*d.DepthOrArraySize as usize]);
}
pub(super) fn forget(resource:usize){let mut g=states().lock().unwrap();g.live.remove(&resource);for l in g.lists.values_mut(){l.remove(&resource);}}
pub(super) fn reset(list:usize){states().lock().unwrap().lists.remove(&list);}
pub(super) fn transition(list:usize,b:&[u32;8]){
    if b[0]!=0||b[1]==1{return;}
    let r=b[2] as usize|((b[3] as usize)<<32);let mut g=states().lock().unwrap();
    let Some(mut s)=g.lists.get(&list).and_then(|l|l.get(&r)).or_else(||g.live.get(&r)).cloned()else{return};
    if b[4]==u32::MAX{s.fill(b[6]);}else if let Some(state)=s.get_mut(b[4] as usize){*state=b[6];}
    g.lists.entry(list).or_default().insert(r,s);
}
pub(super) fn submitted(lists:&[*mut c_void]){
    let mut g=states().lock().unwrap();
    for &list in lists {if let Some(updates)=g.lists.get(&(list as usize)).cloned(){for(r,s)in updates{if let Some(old)=g.live.get_mut(&r){*old=s;}}}}
}
/// A temporary native operation leaves the title's recorded states unchanged.
/// Render targets do not decay out of an explicit state between submissions.
pub(super) unsafe fn with_state<T>(list:*mut c_void,resource:usize,target:u32,f:impl FnOnce()->T)->T{
    let before={let g=states().lock().unwrap();g.lists.get(&(list as usize)).and_then(|l|l.get(&resource)).or_else(||g.live.get(&resource)).cloned()};
    let Some(before)=before else{return f()};
    let method=if super::real::is_adopted(list){super::real::orig_fn(list,26)}else{*(*(list as *const *const usize)).add(26)};
    let barrier:unsafe extern "system" fn(*mut c_void,u32,*const [u32;8])=std::mem::transmute(method);
    let forward:Vec<_>=before.iter().enumerate().filter(|(_,s)|**s!=target).map(|(i,&s)|[0,0,resource as u32,(resource>>32)as u32,i as u32,s,target,0]).collect();
    if !forward.is_empty(){barrier(list,forward.len()as u32,forward.as_ptr());}
    let result=f();
    let backward:Vec<_>=forward.into_iter().map(|mut b|{b.swap(5,6);b}).collect();
    if !backward.is_empty(){barrier(list,backward.len()as u32,backward.as_ptr());}
    result
}
#[cfg(test)]
mod tests{
    use super::*;
    use std::mem::transmute_copy;
    use windows::Win32::Graphics::{Direct3D::D3D_FEATURE_LEVEL_11_0,Dxgi::Common::*};
    #[test]
    fn clear_and_capture_a_uav_state_target_preserves_pixels_and_state(){unsafe{
        let mut dbg:Option<ID3D12Debug>=None;D3D12GetDebugInterface(&mut dbg).unwrap();dbg.unwrap().EnableDebugLayer();
        let mut dev=None;D3D12CreateDevice(None,D3D_FEATURE_LEVEL_11_0,&mut dev).unwrap();let dev:ID3D12Device=dev.unwrap();let info:ID3D12InfoQueue=dev.cast().unwrap();
        let d=D3D12_RESOURCE_DESC{Dimension:D3D12_RESOURCE_DIMENSION_TEXTURE2D,Width:4,Height:4,DepthOrArraySize:1,MipLevels:1,Format:DXGI_FORMAT_R8G8B8A8_UNORM,SampleDesc:DXGI_SAMPLE_DESC{Count:1,Quality:0},Flags:D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET|D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,..Default::default()};
        let mut target=None;dev.CreateCommittedResource(&D3D12_HEAP_PROPERTIES{Type:D3D12_HEAP_TYPE_DEFAULT,..Default::default()},D3D12_HEAP_FLAG_NONE,&d,D3D12_RESOURCE_STATE_UNORDERED_ACCESS,None,&mut target).unwrap();let target:ID3D12Resource=target.unwrap();note(target.as_raw(),8);
        let heap:ID3D12DescriptorHeap=dev.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC{Type:D3D12_DESCRIPTOR_HEAP_TYPE_RTV,NumDescriptors:1,..Default::default()}).unwrap();let handle=heap.GetCPUDescriptorHandleForHeapStart();dev.CreateRenderTargetView(&target,None,handle);super::super::present::note_rtv(handle.ptr,target.as_raw());
        let mut rb=None;dev.CreateCommittedResource(&D3D12_HEAP_PROPERTIES{Type:D3D12_HEAP_TYPE_READBACK,..Default::default()},D3D12_HEAP_FLAG_NONE,&D3D12_RESOURCE_DESC{Dimension:D3D12_RESOURCE_DIMENSION_BUFFER,Width:1024,Height:1,DepthOrArraySize:1,MipLevels:1,SampleDesc:DXGI_SAMPLE_DESC{Count:1,Quality:0},Layout:D3D12_TEXTURE_LAYOUT_ROW_MAJOR,..Default::default()},D3D12_RESOURCE_STATE_COPY_DEST,None,&mut rb).unwrap();let rb:ID3D12Resource=rb.unwrap();
        let alloc:ID3D12CommandAllocator=dev.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).unwrap();let list:ID3D12GraphicsCommandList=dev.CreateCommandList(0,D3D12_COMMAND_LIST_TYPE_DIRECT,&alloc,None).unwrap();super::super::real::adopt(list.as_raw(),super::super::real::Kind::CommandList);
        super::super::real::cl_clear_rtv(list.as_raw(),handle.ptr,&[0.25,0.5,0.75,1.0],0,std::ptr::null());
        super::super::list_x::note_render_targets(list.as_raw(),1,&handle.ptr,0,std::ptr::null());
        let clear:unsafe extern "system" fn(*mut c_void,usize,*const [f32;4],u32,*const c_void)=std::mem::transmute(super::super::real::orig_fn(list.as_raw(),48));
        with_render_targets(list.as_raw(),||clear(list.as_raw(),handle.ptr,&[0.25,0.5,0.75,1.0],0,std::ptr::null()));
        with_state(list.as_raw(),target.as_raw()as usize,0x800,||{
            let dst=D3D12_TEXTURE_COPY_LOCATION{pResource:transmute_copy(&rb),Type:D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,Anonymous:D3D12_TEXTURE_COPY_LOCATION_0{PlacedFootprint:D3D12_PLACED_SUBRESOURCE_FOOTPRINT{Offset:0,Footprint:D3D12_SUBRESOURCE_FOOTPRINT{Format:DXGI_FORMAT_R8G8B8A8_UNORM,Width:4,Height:4,Depth:1,RowPitch:256}}}};
            let src=D3D12_TEXTURE_COPY_LOCATION{pResource:transmute_copy(&target),Type:D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,Anonymous:D3D12_TEXTURE_COPY_LOCATION_0{SubresourceIndex:0}};list.CopyTextureRegion(&dst,0,0,0,&src,None);
        });
        list.Close().unwrap();let q:ID3D12CommandQueue=dev.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC::default()).unwrap();q.ExecuteCommandLists(&[Some(list.cast().unwrap())]);let done:ID3D12Fence=dev.CreateFence(0,D3D12_FENCE_FLAG_NONE).unwrap();q.Signal(&done,1).unwrap();let start=std::time::Instant::now();while done.GetCompletedValue()<1{assert!(start.elapsed().as_secs()<10);std::thread::sleep(std::time::Duration::from_millis(1));}
        let mut p=std::ptr::null_mut();rb.Map(0,None,Some(&mut p)).unwrap();for y in 0..4{for x in 0..4{let v=std::slice::from_raw_parts(p.cast::<u8>().add(y*256+x*4),4);assert_eq!(v,&[64,128,191,255]);}}rb.Unmap(0,None);assert_eq!(states().lock().unwrap().live[&(target.as_raw()as usize)],[8]);
        for i in 0..info.GetNumStoredMessagesAllowedByRetrievalFilter(){let mut n=0;info.GetMessage(i,None,&mut n).unwrap();let mut data=vec![0u64;n.div_ceil(8)];info.GetMessage(i,Some(data.as_mut_ptr().cast()),&mut n).unwrap();let m=&*data.as_ptr().cast::<D3D12_MESSAGE>();assert!(m.Severity.0>D3D12_MESSAGE_SEVERITY_ERROR.0,"{}",std::ffi::CStr::from_ptr(m.pDescription.cast()).to_string_lossy());}
        forget(target.as_raw()as usize);println!("Real clear wrapper and capture: all 16 pixels correct; initial UAV state restored; no validation errors");
    }}
}
