// Experimental changes: Speedstu, 2026-11-23/24. See LOADING-TRIAL.md for provenance.
//! Order sparse mappings against earlier and later title submissions on other queues.
use std::{collections::HashMap, ffi::c_void, sync::{Mutex, OnceLock}};
use windows::{core::Interface, Win32::Graphics::Direct3D12::*};
struct Timeline { fence: ID3D12Fence, value: u64 }
#[derive(Default)]
struct Order { users: HashMap<usize, Timeline>, maps: HashMap<usize, Timeline>, active: HashMap<usize, ID3D12CommandQueue> }
static ORDER: OnceLock<Mutex<Order>> = OnceLock::new();
fn order() -> &'static Mutex<Order> { ORDER.get_or_init(Default::default) }
unsafe fn queue_call(q:&ID3D12CommandQueue, slot:usize, f:&ID3D12Fence,v:u64)->windows::core::Result<()> {
    let raw=q.as_raw();
    let p=if super::real::is_adopted(raw) {super::real::orig_fn(raw,slot)}else{*(*(raw as *const *const usize)).add(slot)};
    let call:unsafe extern "system" fn(*mut c_void,*mut c_void,u64)->i32=std::mem::transmute(p);
    let hr=call(raw,f.as_raw(),v);
    windows::core::HRESULT(hr).ok()
}
pub(crate) unsafe fn signal(q:&ID3D12CommandQueue,f:&ID3D12Fence,v:u64)->windows::core::Result<()> { queue_call(q,14,f,v) }
pub(crate) unsafe fn wait(q:&ID3D12CommandQueue,f:&ID3D12Fence,v:u64)->windows::core::Result<()> { queue_call(q,15,f,v) }
unsafe fn timeline(dev:&ID3D12Device)->Result<Timeline,String> {
    let raw=dev.as_raw();
    let p=if super::real::is_adopted(raw){super::real::orig_fn(raw,36)}else{*(*(raw as *const *const usize)).add(36)};
    let create:unsafe extern "system" fn(*mut c_void,u64,D3D12_FENCE_FLAGS,*const windows::core::GUID,*mut *mut c_void)->i32=std::mem::transmute(p);
    let mut f=std::ptr::null_mut();
    let hr=create(raw,0,D3D12_FENCE_FLAG_NONE,&ID3D12Fence::IID,&mut f);
    if hr<0 {return Err(format!("queue ordering fence: {hr:#x}"));}
    Ok(Timeline{fence:ID3D12Fence::from_raw(f),value:0})
}
/// Only the actual ExecuteCommandLists call is gated: refresh/upload code takes
/// the sparse-memory lock and must run outside this lock.
#[cfg(test)]
pub(super) fn submit<T>(f:impl FnOnce()->T)->T {let _g=order().lock().unwrap(); f()}
/// A newly seen queue must wait for mappings made before its first submission.
/// Registration and Execute stay under the mapping lock, so a concurrent mapper
/// cannot miss the new queue between taking its user snapshot and changing tiles.
pub(super) unsafe fn submit_on<T>(queue: *mut c_void, f: impl FnOnce()->T)->T {
    let mut g=order().lock().unwrap();
    let q=ID3D12CommandQueue::from_raw_borrowed(&queue).expect("live submission queue");
    let key=queue as usize;
    if !g.active.contains_key(&key) {
        for (&mapping,t) in &g.maps {
            if mapping!=key {
                if let Err(e)=wait(q,&t.fence,t.value) {
                    let reason=format!("new queue could not wait for prior tile mappings: {e}");
                    crate::xlog_error!("{reason}");crate::log::flush();super::real::end_title(0x7c1000a);
                }
            }
        }
        g.active.insert(key,q.clone());
        crate::xlog_info!("queue order: registered queue {key:#x}, adopted {}, waited for {} prior mapping queues",super::real::is_adopted(queue),g.maps.len());
    }
    f()
}
/// XCL-owned uploads/readbacks may use queues whose vtables were never adopted.
/// Route their actual native submit through the same mapping order as title queues.
pub(crate) unsafe fn execute_native(queue:&ID3D12CommandQueue,lists:&[Option<ID3D12CommandList>]) {
    let raw=queue.as_raw();
    let p=if super::real::is_adopted(raw){super::real::orig_fn(raw,10)}else{*(*(raw as *const *const usize)).add(10)};
    let call:unsafe extern "system" fn(*mut c_void,u32,*const *mut c_void)=std::mem::transmute(p);
    let pointers:Vec<_>=lists.iter().map(|l|l.as_ref().map_or(std::ptr::null_mut(),Interface::as_raw)).collect();
    submit_on(raw,||call(raw,pointers.len() as u32,pointers.as_ptr()));
}
unsafe fn update_with<T>(dev:&ID3D12Device,mapping:&ID3D12CommandQueue,users:&[ID3D12CommandQueue],f:impl FnOnce()->T)->Result<T,String> {
    let mut g=order().lock().unwrap();
    let mut all=users.to_vec();
    for q in g.active.values() {if !all.iter().any(|u|u.as_raw()==q.as_raw()){all.push(q.clone());}}
    let users:Vec<_>=all.iter().filter(|q|q.as_raw()!=mapping.as_raw()).collect();
    static TRACES:std::sync::atomic::AtomicU64=std::sync::atomic::AtomicU64::new(0);
    let trace=TRACES.fetch_add(1,std::sync::atomic::Ordering::Relaxed);
    if trace<4 {crate::xlog_info!("queue order: mapping queue {:p}, users {:x?}",mapping.as_raw(),users.iter().map(|q|q.as_raw() as usize).collect::<Vec<_>>());}
    for q in &users {
        let key=q.as_raw() as usize;
        if !g.users.contains_key(&key){g.users.insert(key,timeline(dev)?);}
        let t=g.users.get_mut(&key).unwrap(); t.value+=1;
        queue_call(q,14,&t.fence,t.value).map_err(|e|e.to_string())?;
        queue_call(mapping,15,&t.fence,t.value).map_err(|e|e.to_string())?;
    }
    let result=f();
    {
        let key=mapping.as_raw() as usize;
        if !g.maps.contains_key(&key){g.maps.insert(key,timeline(dev)?);}
        let t=g.maps.get_mut(&key).unwrap();t.value+=1;
        queue_call(mapping,14,&t.fence,t.value).map_err(|e|e.to_string())?;
        for q in users {queue_call(q,15,&t.fence,t.value).map_err(|e|e.to_string())?;}
    }
    Ok(result)
}
pub(super) unsafe fn update<T>(dev:&ID3D12Device,mapping:&ID3D12CommandQueue,f:impl FnOnce()->T)->T {
    let users:Vec<_>=super::present::known_queues().into_iter().filter_map(|q|{
        ID3D12CommandQueue::from_raw_borrowed(&(q as *mut c_void)).cloned()
    }).collect();
    match update_with(dev,mapping,&users,f){Ok(v)=>v,Err(e)=>{
        let reason=format!("sparse tile mapping ordering failed: {e}");
        crate::xlog_error!("{reason}");crate::report::write_final(Some(reason));super::real::end_title(0x7c1000a)
    }}
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{transmute_copy,ManuallyDrop};
    use windows::Win32::Graphics::{Direct3D::D3D_FEATURE_LEVEL_11_0,Dxgi::Common::*};
    #[test]
    fn remap_waits_for_prior_reads_and_precedes_later_reads(){unsafe{
        let mut debug:Option<ID3D12Debug>=None;D3D12GetDebugInterface(&mut debug).unwrap();debug.unwrap().EnableDebugLayer();
        let mut dev=None;D3D12CreateDevice(None,D3D_FEATURE_LEVEL_11_0,&mut dev).unwrap();let dev:ID3D12Device=dev.unwrap();
        let info:ID3D12InfoQueue=dev.cast().unwrap();
        let desc=|size,flags|D3D12_RESOURCE_DESC{Dimension:D3D12_RESOURCE_DIMENSION_BUFFER,Width:size,Height:1,DepthOrArraySize:1,MipLevels:1,SampleDesc:DXGI_SAMPLE_DESC{Count:1,Quality:0},Layout:D3D12_TEXTURE_LAYOUT_ROW_MAJOR,Flags:flags,..Default::default()};
        let create=|ty,size,state|{let mut r=None;dev.CreateCommittedResource(&D3D12_HEAP_PROPERTIES{Type:ty,..Default::default()},D3D12_HEAP_FLAG_NONE,&desc(size,D3D12_RESOURCE_FLAG_NONE),state,None,&mut r).unwrap();r.unwrap()};
        let upload:ID3D12Resource=create(D3D12_HEAP_TYPE_UPLOAD,131072,D3D12_RESOURCE_STATE_GENERIC_READ);
        let mut p=std::ptr::null_mut();upload.Map(0,None,Some(&mut p)).unwrap();std::ptr::write_bytes(p,0x11,65536);std::ptr::write_bytes(p.cast::<u8>().add(65536),0x22,65536);upload.Unmap(0,None);
        let mut heap=None;dev.CreateHeap(&D3D12_HEAP_DESC{SizeInBytes:131072,Properties:D3D12_HEAP_PROPERTIES{Type:D3D12_HEAP_TYPE_DEFAULT,..Default::default()},Flags:D3D12_HEAP_FLAG_ALLOW_ONLY_BUFFERS,..Default::default()},&mut heap).unwrap();let heap:ID3D12Heap=heap.unwrap();
        let mut placed=None;dev.CreatePlacedResource(&heap,0,&desc(131072,D3D12_RESOURCE_FLAG_NONE),D3D12_RESOURCE_STATE_COPY_DEST,None,&mut placed).unwrap();let placed:ID3D12Resource=placed.unwrap();
        let mut reserved=None;dev.CreateReservedResource(&desc(65536,D3D12_RESOURCE_FLAG_NONE),D3D12_RESOURCE_STATE_COMMON,None,&mut reserved).unwrap();let reserved:ID3D12Resource=reserved.unwrap();
        let readback:ID3D12Resource=create(D3D12_HEAP_TYPE_READBACK,131072,D3D12_RESOURCE_STATE_COPY_DEST);
        let user:ID3D12CommandQueue=dev.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC::default()).unwrap();let maps:ID3D12CommandQueue=dev.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC::default()).unwrap();
        let a:ID3D12CommandAllocator=dev.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).unwrap();let b:ID3D12CommandAllocator=dev.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).unwrap();let c:ID3D12CommandAllocator=dev.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).unwrap();
        let prep:ID3D12GraphicsCommandList=dev.CreateCommandList(0,D3D12_COMMAND_LIST_TYPE_DIRECT,&a,None).unwrap();prep.CopyBufferRegion(&placed,0,&upload,0,131072);
        prep.ResourceBarrier(&[D3D12_RESOURCE_BARRIER{Type:D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,Anonymous:D3D12_RESOURCE_BARRIER_0{Transition:ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER{pResource:transmute_copy(&placed),Subresource:0,StateBefore:D3D12_RESOURCE_STATE_COPY_DEST,StateAfter:D3D12_RESOURCE_STATE_COMMON})},..Default::default()}]);prep.Close().unwrap();execute_native(&user,&[Some(prep.cast().unwrap())]);
        super::super::real::adopt(user.as_raw(),super::super::real::Kind::CommandQueue);
        super::super::real::QUEUE_SHIFTED.store(true,std::sync::atomic::Ordering::Release);
        let execute = |list:&ID3D12GraphicsCommandList| {
            let cl:ID3D12CommandList=list.cast().unwrap();
            execute_native(&user,&[Some(cl)]);
        };
        let remap=|tile:u32|{update_with(&dev,&maps,&[user.clone()],||maps.UpdateTileMappings(&reserved,1,Some(&D3D12_TILED_RESOURCE_COORDINATE::default()),Some(&D3D12_TILE_REGION_SIZE{NumTiles:1,UseBox:false.into(),..Default::default()}),&heap,1,Some(&D3D12_TILE_RANGE_FLAG_NONE),Some(&tile),Some(&1),D3D12_TILE_MAPPING_FLAG_NONE)).unwrap();};
        remap(0);
        let gate:ID3D12Fence=dev.CreateFence(0,D3D12_FENCE_FLAG_NONE).unwrap();wait(&user,&gate,1).unwrap();
        let first:ID3D12GraphicsCommandList=dev.CreateCommandList(0,D3D12_COMMAND_LIST_TYPE_DIRECT,&b,None).unwrap();first.CopyBufferRegion(&readback,0,&reserved,0,65536);first.Close().unwrap();execute(&first);
        remap(1); // first read is deliberately blocked, so this must wait on the GPU
        let second:ID3D12GraphicsCommandList=dev.CreateCommandList(0,D3D12_COMMAND_LIST_TYPE_DIRECT,&c,None).unwrap();second.CopyBufferRegion(&readback,65536,&reserved,0,65536);second.Close().unwrap();execute(&second);
        gate.Signal(1).unwrap();let done:ID3D12Fence=dev.CreateFence(0,D3D12_FENCE_FLAG_NONE).unwrap();signal(&user,&done,1).unwrap();let start=std::time::Instant::now();while done.GetCompletedValue()<1{assert!(start.elapsed().as_secs()<10);std::thread::sleep(std::time::Duration::from_millis(1));}dev.GetDeviceRemovedReason().unwrap();
        let mut p=std::ptr::null_mut();readback.Map(0,None,Some(&mut p)).unwrap();let bytes=std::slice::from_raw_parts(p.cast::<u8>(),131072);assert!(bytes[..65536].iter().all(|&v|v==0x11));assert!(bytes[65536..].iter().all(|&v|v==0x22));readback.Unmap(0,None);
        // A queue discovered after a mapping was recorded must still wait for it.
        let map_gate:ID3D12Fence=dev.CreateFence(0,D3D12_FENCE_FLAG_NONE).unwrap();
        wait(&maps,&map_gate,1).unwrap();
        update_with(&dev,&maps,&[],||maps.UpdateTileMappings(&reserved,1,Some(&D3D12_TILED_RESOURCE_COORDINATE::default()),Some(&D3D12_TILE_REGION_SIZE{NumTiles:1,UseBox:false.into(),..Default::default()}),&heap,1,Some(&D3D12_TILE_RANGE_FLAG_NONE),Some(&0),Some(&1),D3D12_TILE_MAPPING_FLAG_NONE)).unwrap();
        let late:ID3D12CommandQueue=dev.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC::default()).unwrap();
        super::super::real::adopt(late.as_raw(),super::super::real::Kind::CommandQueue);
        let alloc:ID3D12CommandAllocator=dev.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).unwrap();
        let list:ID3D12GraphicsCommandList=dev.CreateCommandList(0,D3D12_COMMAND_LIST_TYPE_DIRECT,&alloc,None).unwrap();
        list.CopyBufferRegion(&readback,0,&reserved,0,65536);list.Close().unwrap();
        let cl:ID3D12CommandList=list.cast().unwrap();
        execute_native(&late,&[Some(cl)]);
        let late_done:ID3D12Fence=dev.CreateFence(0,D3D12_FENCE_FLAG_NONE).unwrap();signal(&late,&late_done,1).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(30));
        assert_eq!(late_done.GetCompletedValue(),0,"a new queue must not outrun an earlier pending mapping");
        map_gate.Signal(1).unwrap();let start=std::time::Instant::now();while late_done.GetCompletedValue()<1{assert!(start.elapsed().as_secs()<10);std::thread::sleep(std::time::Duration::from_millis(1));}
        let mut p=std::ptr::null_mut();readback.Map(0,None,Some(&mut p)).unwrap();assert!(std::slice::from_raw_parts(p.cast::<u8>(),65536).iter().all(|&v|v==0x11));readback.Unmap(0,None);
        for i in 0..info.GetNumStoredMessagesAllowedByRetrievalFilter(){let mut n=0;info.GetMessage(i,None,&mut n).unwrap();let mut data=vec![0u64;n.div_ceil(8)];info.GetMessage(i,Some(data.as_mut_ptr().cast()),&mut n).unwrap();let m=&*data.as_ptr().cast::<D3D12_MESSAGE>();assert!(m.Severity.0>D3D12_MESSAGE_SEVERITY_ERROR.0,"{}",std::ffi::CStr::from_ptr(m.pDescription.cast()).to_string_lossy());}
        super::super::real::QUEUE_SHIFTED.store(false,std::sync::atomic::Ordering::Release);
        println!("Adopted newer Xbox queues: in-flight reads preserved across remap, newly discovered queue waited for earlier mappings; no validation errors");
    }}
}
