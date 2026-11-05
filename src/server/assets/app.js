// ImageCropper web client (minimal Canvas2D implementation over the WS
// protocol). The WebGPU/wasm client replaces this; the protocol is the same.
"use strict";

const EV_EVENT=0x01, EV_THUMB=0x02, EV_THUMB_BATCH=0x03, EV_PONG=0x06;
const CL_HELLO=0x10, CL_WANT=0x11, CL_VIEWPORT=0x12, CL_FOCUS=0x13,
      CL_ACTION=0x14, CL_CREDIT=0x15, CL_PAUSE=0x16, CL_RESUME=0x17,
      CL_PING=0x18, CL_BAD_PREVIEW=0x1a;
const BK_MANIFEST=0x20, BK_FULL_HEAD=0x22, BK_FULL_CHUNK=0x21,
      BK_GET_MANIFEST=0x28, BK_GET_FULL=0x29;

function frame(type, reqId, payload, flags=0) {
  const b=new Uint8Array(13+payload.length), v=new DataView(b.buffer);
  b[0]=type; v.setUint32(1,reqId,true); v.setUint32(5,flags,true);
  v.setUint32(9,payload.length,true); b.set(payload,13); return b;
}
function unframe(buf) {
  const v=new DataView(buf), b=new Uint8Array(buf);
  return {type:b[0], reqId:v.getUint32(1,true), flags:v.getUint32(5,true),
          len:v.getUint32(9,true), payload:b.subarray(13)};
}
const u32s=(...n)=>{const b=new Uint8Array(4*n.length),v=new DataView(b.buffer);
  n.forEach((x,i)=>v.setUint32(4*i,x,true)); return b};
const str=(s)=>new TextEncoder().encode(s);
const token=new URLSearchParams(location.search).get("token")||"";

const canvas=document.getElementById("c"), ctx=canvas.getContext("2d");
const topEl=document.getElementById("top"), statusEl=document.getElementById("status"),
      helpEl=document.getElementById("help");
helpEl.textContent="scroll: wheel · zoom: +/- · width: w/W · select: click/ctrl/shift · delete: Del · move: m · fullscreen: f (toggle, ←/→) · crop: c";

let manifest=null, items=[], thumbs=new Map(), pending=new Map();
let scanning=false, scanErrs=[], reloading=false;
let tileH=180, zoom=1.0, scrollY=0, selected=new Set(), focus=0, anchor=0;
let creditBudget=8*1024*1024;

function connect(path){ return new Promise((res,rej)=>{
  const ws=new WebSocket(`ws://${location.host}${path}?token=${encodeURIComponent(token)}`);
  ws.binaryType="arraybuffer";
  // A stalled upgrade (server mid-restart, proxy swallowing the response)
  // must surface as a failure, not hang boot() on a silent "0 images" page.
  const to=setTimeout(()=>rej(new Error("connect timeout")),10000);
  ws.onopen=()=>{clearTimeout(to);res(ws)};
  ws.onerror=()=>{clearTimeout(to);rej(new Error("refused"))};
  ws.onclose=()=>{clearTimeout(to);rej(new Error("closed during connect"))};
});
}

// /bulk request router: GET_MANIFEST + GET_FULL replies share the socket,
// correlated by req_id.
let bulkWs=null;
const bulkReqs=new Map(); let bulkReqId=0x100;
const BULK_TIMEOUT=120000, BULK_TRIES=3;
function bulkRequest(type,payload){
  const reqId=bulkReqId++;
  return new Promise((res,rej)=>{
    // A lost/slow response must not hang boot()/refreshManifest forever —
    // but under load a reply can legitimately take a while, so the request
    // is retried before it is allowed to fail.
    const req={parts:[],head:null,res,rej,tries:0,to:0,
      arm(){req.to=setTimeout(()=>{
        if(++req.tries<BULK_TRIES){
          // Drop any partial reply — a late first response must not mix
          // chunks with the retried one.
          req.parts=[];req.head=null;req.arm();
          bulkWs.send(frame(type,reqId,payload))}
        else if(bulkReqs.delete(reqId))rej(new Error("bulk timeout"));
      },BULK_TIMEOUT)}};
    bulkReqs.set(reqId,req);req.arm();
    bulkWs.send(frame(type,reqId,payload));
  });
}
function onBulk(f){
  const req=bulkReqs.get(f.reqId); if(!req)return;
  if(f.type===BK_FULL_HEAD){
    const v=new DataView(f.payload.buffer,f.payload.byteOffset);
    req.head={w:v.getUint32(8,true),h:v.getUint32(12,true),fmt:v.getUint8(16)};
    return;
  }
  req.parts.push(f.payload);
  if(f.flags&1){
    clearTimeout(req.to);
    bulkReqs.delete(f.reqId);
    const all=new Uint8Array(req.parts.reduce((s,p)=>s+p.length,0));let o=0;
    for(const p of req.parts){all.set(p,o);o+=p.length}
    req.res({head:req.head,body:all});
  }
}

// Thumbs are keyed by positional id — a resync renumbers everything.
// Key them by content (dir+name+sig) so unchanged images survive a
// resync instead of being refetched.
function snapshotThumbs(){
  const m=new Map(), loose=new Map();
  if(manifest&&manifest.dirs)for(let i=0;i<items.length;i++){
    const it=items[i],b=thumbs.get(i);
    if(it&&b){m.set(`${manifest.dirs[it.dir_index]}|${it.name}|${it.sig}`,b);
      loose.set(`${manifest.dirs[it.dir_index]}|${it.name}`,b);}
  }
  return {m,loose};
}
function restoreThumbs(snap){
  for(let i=0;i<items.length;i++){
    const it=items[i];
    if(!it||it.removed)continue;
    const dk=`${manifest.dirs[it.dir_index]}|${it.name}`;
    const b=snap.m.get(`${dk}|${it.sig}`)||snap.loose.get(dk);
    if(b){thumbs.set(i,b);
      // Loose (sig-changed) hit: keep the pixels, refresh the bytes.
      if(!snap.m.has(`${dk}|${it.sig}`))pending.set(i,Date.now());}
  }
}

// Selection is user intent — it must survive every manifest update,
// including resyncs that renumber all ids. Track it by content key
// (dir+name+sig), the same identity that preserves thumbnails.
const itemKey=i=>{
  const it=items[i];
  return (it&&manifest&&manifest.dirs)?`${manifest.dirs[it.dir_index]}|${it.name}|${it.sig}`:null;
};
function snapshotSelection(){
  const s=new Set();
  for(const i of selected){const k=itemKey(i);if(k)s.add(k)}
  return s;
}
function restoreSelection(snap){
  selected=new Set();
  if(!snap||!snap.size)return;
  for(let i=0;i<items.length;i++){
    const it=items[i];
    if(!it||it.removed)continue;
    const it2=items[i],dk=`${manifest.dirs[it2.dir_index]}|${it2.name}`;
    if(snap.has(itemKey(i))||snap.has(dk))selected.add(i);
  }
}
// Focus/anchor follow their content across renumbering; fall back to the
// nearest live index when the item is gone.
function restoreIndex(key,fallback){
  if(key)for(let i=0;i<items.length;i++){
    const it=items[i];
    if(!it||it.removed)continue;
    const dk=`${manifest.dirs[it.dir_index]}|${it.name}`;
    if(itemKey(i)===key||dk===key)return i;
  }
  const f=Math.min(Math.max(fallback,0),Math.max(items.length-1,0));
  for(let i=f;i<items.length;i++)if(items[i]&&!items[i].removed)return i;
  for(let i=f-1;i>=0;i--)if(items[i]&&!items[i].removed)return i;
  return 0;
}

async function refreshManifest(){
  const snap=snapshotThumbs();
  const sel=snapshotSelection(), focKey=itemKey(focus), ancKey=itemKey(anchor),
        focIdx=focus, ancIdx=anchor;
  const {body}=await bulkRequest(BK_GET_MANIFEST,u32s(1,0));
  manifest=JSON.parse(new TextDecoder().decode(body));
  mouseSelect=manifest.mouse_select||"sequence";
  items=manifest.items;
  thumbs.clear();pending.clear();icStats.thumbClears++;decRetry.clear();
  restoreThumbs(snap);
  // Selection/focus survive the update: ids may have been renumbered.
  restoreSelection(sel);
  focus=restoreIndex(focKey,focIdx);anchor=restoreIndex(ancKey,ancIdx);
  scanning=false;
  // Completions may have been lost on the way — the re-pulled manifest is
  // now authoritative, so stale pending counters must not linger.
  pendingActions.clear();
  screenBmp=null;screenIdx=-1;
  layoutDirty=true;scheduleDraw();
}

// ids are positional (items[id]); tombstoned entries keep their slot.
// Patches arriving while the initial manifest is still in flight are
// queued — applying them against null throws and permanently loses items.
let pendingPatches=[];
function applyPatch(p){
  if(!manifest){pendingPatches.push(p);return}
  if(p.resync){
    icStats.resyncs++;icStats.lastResyncAt=Date.now();
    const snap=snapshotThumbs();
    const sel=snapshotSelection(), focKey=itemKey(focus), ancKey=itemKey(anchor),
          focIdx=focus, ancIdx=anchor;
    items=[];manifest.dirs=[];thumbs.clear();pending.clear();
    icStats.thumbClears++;
    scanning=false;
    screenBmp=null;screenIdx=-1;decRetry.clear();
    manifest.dirs=(p.dirs_added||[]).slice();
    (p.upsert||[]).forEach(it=>{items[it.id]=it});
    (p.remove||[]).forEach(id=>{if(items[id])items[id].removed=true;thumbs.delete(id)});
    manifest.version=p.version;
    restoreThumbs(snap);icStats.restored=thumbs.size;
    // Every id was reassigned — selection/focus are re-anchored by content
    // so the user's chosen images stay chosen.
    restoreSelection(sel);
    focus=restoreIndex(focKey,focIdx);anchor=restoreIndex(ancKey,ancIdx);
    layoutDirty=true;scheduleDraw();
    return;
  }
  (p.dirs_added||[]).forEach(d=>manifest.dirs.push(d));
  (p.upsert||[]).forEach(it=>{items[it.id]=it});
  (p.remove||[]).forEach(id=>{
    if(items[id])items[id].removed=true;
    thumbs.delete(id);
    // A tombstoned item cannot stay selected (ids are stable here, but a
    // removed image must leave the user's set).
    selected.delete(id);
  });
  manifest.version=p.version;
  layoutDirty=true;scheduleDraw();
}

async function boot(){
  statusEl.textContent="connecting…";
  const bulk=await connect("/bulk"), data=await connect("/data");
  bulkWs=bulk;dataWs=data;
  bulk.onmessage=e=>onBulk(unframe(e.data));
  bulk.onclose=()=>{
    // Dead socket → pending bulkRequests would hang forever otherwise.
    for(const r of bulkReqs.values())r.rej(new Error("bulk socket closed"));
    bulkReqs.clear();
    reloading=true;
    statusEl.textContent="bulk connection closed — reloading…";
    setTimeout(()=>location.reload(),1500);};

  // /data: hello then handle thumbs+events. onclose must be armed before
  // the first await below — a mid-boot death is otherwise invisible.
  data.onmessage=e=>onData(unframe(e.data));
  data.onclose=()=>{reloading=true;
    statusEl.textContent="connection closed — reloading…";
    setTimeout(()=>location.reload(),1500);};
  data.send(frame(CL_HELLO,0,str(JSON.stringify({token,manifest_version:0}))));

  // /bulk: manifest (chunked)
  statusEl.textContent="loading manifest…";
  const {body}=await bulkRequest(BK_GET_MANIFEST,u32s(1,0));
  manifest=JSON.parse(new TextDecoder().decode(body));
  mouseSelect=manifest.mouse_select||"sequence";
  items=manifest.items;
  // Patches that raced the manifest pull apply now, in order.
  pendingPatches.forEach(applyPatch);pendingPatches=[];
  statusEl.textContent="";
  layoutDirty=true; draw();

  // credits: keep the push pipeline fed
  const grant=()=>{if(data.readyState===1){
    data.send(frame(CL_CREDIT,0,(()=>{const b=new Uint8Array(8),
      v=new DataView(b.buffer);v.setBigUint64(0,BigInt(creditBudget),true);return b})()));}};
  grant(); setInterval(grant, 500);
}

const thumbReqs=[];
// N virtual want-workers: keep up to WANT_WORKERS chunked CL_WANT requests
// in flight so thumb delivery never stalls on one round-trip. A worker slot
// is freed when its whole chunk arrived, or after WANT_TIMEOUT — a request
// the server could not serve (deleted/moved item) must not retire a slot
// permanently.
const WANT_WORKERS=6, WANT_CHUNK=8, WANT_TIMEOUT=65000;
// A requested thumbnail that never arrives is retried: drop it from
// `pending` so the next draw() renews the request. Preview generation on
// a slow mount legitimately takes tens of seconds — the server answers
// within 60 s — so renew at 45 s, not sooner.
const PENDING_TIMEOUT=45000;
// A dispatched action the server never acknowledges is retried by the user
// (the socket must be dead). Once the server acks the enqueue the op is in
// its queue and is finished even across a restart — so a queued delete is
// never timed out here, however slow the filesystem is.
const ACTION_ACK_TIMEOUT=30000;
let wantQ=[], wantCtr=0;
const wantSlots=new Map();   // reqId -> {left:Set(ids), at:ms}
function pumpWants(){
  while(wantSlots.size<WANT_WORKERS&&wantQ.length&&dataWs&&dataWs.readyState===1){
    const chunk=wantQ.splice(0,WANT_CHUNK);
    icStats.wantSent+=chunk.length;
    const reqId=++wantCtr;
    const b=new Uint8Array(2+4*chunk.length),v=new DataView(b.buffer);
    v.setUint16(0,chunk.length,true);
    chunk.forEach((id,i)=>v.setUint32(2+4*i,id,true));
    dataWs.send(frame(CL_WANT,reqId,b));
    wantSlots.set(reqId,{left:new Set(chunk),at:Date.now()});
  }
}
// Housekeeping: retire stalled want slots, retry lost thumbnails, and fail
// pending actions the server never acknowledged.
setInterval(()=>{
  const now=Date.now();
  let dirty=false;
  for(const [reqId,s] of wantSlots)
    if(now-s.at>WANT_TIMEOUT){wantSlots.delete(reqId);dirty=true}
  for(const [id,at] of pending)
    if(now-at>PENDING_TIMEOUT){pending.delete(id);dirty=true}
  for(const [tag,a] of pendingActions)
    if(!a.queued&&now-a.at>ACTION_ACK_TIMEOUT){
      pendingActions.delete(tag);
      // The dispatch never reached the server: bring its tiles back so the
      // user can retry instead of looking at holes.
      for(const i of a.ids||[])if(items[i]&&items[i].removed)items[i].removed=false;
      statusEl.textContent=`${a.kind} was not acknowledged — try again`;
      dirty=true;
    }
  if(dirty){pumpWants()}
  // Always redraw: draw() re-evaluates which visible tiles still lack a
  // thumbnail and re-queues them. This is the self-healing loop that keeps
  // a blank viewport from staying blank after lost/dropped pushes.
  scheduleDraw();
},1000);
// Decode worker pool: createImageBitmap on the main thread shares
// scheduling with draw()/events — under a batch flood that's the real
// thumb-rate cap. ImageBitmap transfers back at zero cost, so JPEGs
// decode off-thread and only finished bitmaps cross back.
const DEC_WORKERS=4;
const decPool=[];
try{
  const decSrc='onmessage=e=>{const{id,b}=e.data;createImageBitmap(b).then(bmp=>postMessage({id,bmp},[bmp]),err=>postMessage({id,bmp:null,err:String(err)}))}';
  const decUrl=URL.createObjectURL(new Blob([decSrc],{type:"text/javascript"}));
  for(let i=0;i<DEC_WORKERS;i++){
    const w=new Worker(decUrl);
    w.onmessage=e=>{const{id,bmp,err}=e.data;onDecoded(id,bmp,err)};
    w.onerror=()=>{};
    decPool.push(w);
  }
}catch(e){/* Worker/createImageBitmap unavailable → main-thread fallback */}
let decRR=0;
function decodeThumb(id,jpeg){
  if(decPool.length){
    const wi=decRR=(decRR+1)%decPool.length,w=decPool[wi];
    decJobs.set(id,{w:wi,at:Date.now()});
    w.postMessage({id,b:jpeg});   // Blob clones by reference — no copy
  }else{
    decJobs.set(id,{w:-1,at:Date.now()});
    createImageBitmap(jpeg).then(bmp=>onDecoded(id,bmp),
      err=>onDecoded(id,null,String(err)));
  }
}
// Decode-failure backoff: without it a rejected createImageBitmap clears
// `pending`, the next draw() re-wants the id, and the whole visible set
// retries at frame rate — thousands of wasted server hits per second.
// Exponential, capped at 60 s; cleared on success/resync (new bytes may
// decode fine) and removed on the resync path entirely.
const decRetry=new Map();   // id -> {n, next:ms}
function onDecoded(id,bmp,err){
  decJobs.delete(id);
  if(bmp){
    icStats.thumbOk++;thumbs.set(id,bmp);decRetry.delete(id);
    // With no manifest dims, the bitmap's aspect is the fallback — and the
    // layout was computed before this thumb existed. Without a re-layout
    // the tile keeps its square placeholder geometry and the image is
    // drawn stretched into it.
    const it=items[id];
    if(it&&!(it.w>0&&it.h>0))layoutDirty=true;
  }
  else{
    icStats.decFail++;icStats.decErr=String(err||"");
    pending.delete(id);
    const r=decRetry.get(id)||{n:0,next:0};
    r.n++;
    r.next=Date.now()+Math.min(60000,250*(1<<Math.min(r.n,8)));
    decRetry.set(id,r);
    // Tell the server the cached preview is presumed corrupt — it drops
    // the persisted file, regenerates, and pushes fresh bytes. The
    // server-side cooldown absorbs repeat reports for one file.
    const it=items[id];
    if(it&&!it.removed&&dataWs&&dataWs.readyState===1){
      const b=new Uint8Array(12),v=new DataView(b.buffer);
      v.setUint32(0,id,true);
      v.setBigUint64(4,BigInt(it.sig||"0"),true);
      dataWs.send(frame(CL_BAD_PREVIEW,0,b));
      icStats.badSent=(icStats.badSent||0)+1;
    }
  }
  scheduleDraw();
}

function onThumb(id,sig,jpeg,reqId){
  pending.delete(id);
  // Free the want-worker slot that asked for this id (sweep pushes carry
  // req_id 0 and free nothing).
  const slot=wantSlots.get(reqId);
  if(slot){slot.left.delete(id);if(!slot.left.size){wantSlots.delete(reqId);pumpWants();}}
  const it=items[id];
  // Drop pushes for a known item whose content no longer matches —
  // the id was reassigned mid-flight, tombstoned, or the file changed.
  // Unknown ids (manifest still loading) are kept: ids are positional
  // from that same manifest, and resyncs clear thumbs anyway.
  // A dropped frame must still trigger a redraw: the tile is missing and
  // `pending` was just cleared, so the next draw re-requests it. Without
  // this the page can go idle on a viewport full of blank tiles.
  if(it&&it.removed){icStats.thumbDrop++;scheduleDraw();return}
  if(it&&sig!=="0"&&it.sig!==sig){
    // Server and client disagree on this file's signature. The push comes
    // from the authoritative manifest, so adopt its sig and keep the
    // thumbnail. Dropping it instead leaves the tile permanently missing:
    // the client re-wants it forever while the server serves identical
    // bytes. (No manifest re-pull here — that clears thumbnails/screen
    // state and would make unrelated updates destructive.)
    icStats.thumbResig=(icStats.thumbResig||0)+1;
    it.sig=sig;
  }
  if(sig!=="0"&&thumbs.has(id)){icStats.thumbDup++;scheduleDraw();return}   // exact duplicate
  decodeThumb(id,jpeg);
}

function onData(f){
  icStats.frames[f.type]=(icStats.frames[f.type]||0)+1;
  icStats.lastFrameAt=Date.now();icStats.lastFrameType=f.type;
  if(f.type===EV_THUMB){
    icStats.thumbRecv++;
    const v=new DataView(f.payload.buffer,f.payload.byteOffset);
    // Raw payload magic — no Blob involved, so this can't fail the way
    // createImageBitmap does. FF D8 = valid JPEG SOI; anything else means
    // the server pushed corrupt bytes (→ badMagic, CL_BAD_PREVIEW regen).
    const jb=f.payload.subarray(12,20);
    icStats.lastThumbMagic=[...jb];
    if(jb.length<2||jb[0]!==0xFF||jb[1]!==0xD8)icStats.badMagic=(icStats.badMagic||0)+1;
    onThumb(v.getUint32(0,true),v.getBigUint64(4,true).toString(),
      new Blob([f.payload.subarray(12)],{type:"image/jpeg"}),f.reqId);
  } else if(f.type===EV_THUMB_BATCH){
    // One WS message, N thumbnails: [u16 count][u32 id,u64 sig,u32 len,
    // jpeg]×N. One Blob backing the whole frame — each thumb decodes off
    // a lazy slice (zero copies) and all N createImageBitmap calls run in
    // parallel instead of trickling one-per-message.
    const buf=f.payload.buffer,base=f.payload.byteOffset;
    const v=new DataView(buf,base);
    const n=v.getUint16(0,true);
    icStats.thumbRecv+=n;icStats.batchRecv++;
    const blob=new Blob([f.payload],{type:"application/octet-stream"});
    let off=2;
    for(let i=0;i<n;i++){
      const id=v.getUint32(off,true),sig=v.getBigUint64(off+4,true).toString();
      const len=v.getUint32(off+12,true);off+=16;
      const jb=f.payload.subarray(off,off+Math.min(len,8));
      icStats.lastThumbMagic=[...jb];
      if(jb.length<2||jb[0]!==0xFF||jb[1]!==0xD8)icStats.badMagic=(icStats.badMagic||0)+1;
      const slice=blob.slice(off,off+len,"image/jpeg");
      off+=len;
      onThumb(id,sig,slice,f.reqId);
    }
  } else if(f.type===EV_EVENT){
    const ev=JSON.parse(new TextDecoder().decode(f.payload));
    icStats.lastEventAt=Date.now();icStats.lastEventEv=ev.ev;
    if(ev.ev==="action-queued"){
      const act=pendingActions.get(ev.tag);
      // The op is in the server's queue — it will finish in its own time.
      if(act)act.queued=true;
    }
    if(ev.ev==="action-done"){
      const act=pendingActions.get(ev.tag);
      if(act!=null){
        // A stale signature means the manifest entry predated a disk
        // change. The server re-syncs the entry and broadcasts the
        // refreshed item BEFORE this ack, so retry once with the new sig.
        if(!ev.ok&&/stale signature/.test(ev.error||"")&&!act.retried){
          const it=items[act.ids[0]];
          if(it&&!it.removed&&dataWs&&dataWs.readyState===1){
            act.retried=true;act.queued=false;act.at=Date.now();
            it.removed=true;thumbs.delete(it.id);
            dataWs.send(frame(CL_ACTION,0,str(JSON.stringify({
              tag:ev.tag,kind:act.kind,id:it.id,sig:it.sig}))));
            scheduleDraw();
            return;
          }
        }
        pendingActions.delete(ev.tag);
        // Tombstones and restores arrive as manifest patches (ordered
        // ahead of the ack) — the ack only runs the pending counter and
        // surfaces failures.
        if(!ev.ok)statusEl.textContent=`${act.kind} failed: ${ev.error||"?"}`;
        scheduleDraw();
      }
    }
    if(ev.ev==="thumb-ready"){
      // The produced preview knows the source's natural dims — if the
      // manifest entry has none (sniff failed / stale), fill them in.
      const it=items[ev.id];
      if(it&&ev.w>0&&ev.h>0&&!(it.w>0&&it.h>0)){
        it.w=ev.w;it.h=ev.h;layoutDirty=true;scheduleDraw();
      }
    }
    if(ev.ev==="scan-start"){scanning=true;scheduleDraw();}
    if(ev.ev==="scan-end"){scanning=false;
      if(ev.count===0&&!scanErrs.length)statusEl.textContent=
        "no images found — check the path, extensions and filters (use -r for subdirs)";
      else if(scanErrs.length)statusEl.textContent=
        `scan done — ${ev.count} images, ${scanErrs.length} errors (last: ${scanErrs[scanErrs.length-1]})`;
      scheduleDraw();}
    if(ev.ev==="scan-error"){scanErrs.push(ev.msg||"?");
      statusEl.textContent=`scan error: ${ev.msg}`;scheduleDraw();}
    if(ev.ev==="manifest-patch"){applyPatch(ev.patch||{});}
    if(ev.ev==="fs-changed"){refreshManifest().catch(()=>{});}
    if(ev.ev==="progress") statusEl.textContent=
      `previews ${ev.done}/${ev.queued}`;
  }
}

// ---- justified layout (same math as the native album) ----
let rows=[];
// `w`/`W` restrict the viewer: the grid lays out inside
// canvas.width*viewW and renders centered with margins.
let viewW=1.0;
const viewWpx=()=>canvas.width*viewW;
const xOff=()=>(canvas.width-viewWpx())/2;
let layoutDirty=true, layoutW=0;
function layout(){
  // Rows only change when items/tombstones, zoom, viewW or the canvas
  // width change — not on every scroll. Skip the O(N) rebuild otherwise.
  if(!layoutDirty&&canvas.width===layoutW)return;
  layoutDirty=false;layoutW=canvas.width;
  rows=[]; const W=viewWpx();
  const target=tileH*zoom;
  let cur=[], curW=0;
  for(let i=0;i<items.length;i++){
    const it=items[i];
    if(!it||it.removed)continue;   // tombstones/holes keep id positions
    const w=aspect(i)*target;
    if(curW+w>W&&cur.length){rows.push({ids:cur,w:curW});cur=[];curW=0;}
    cur.push(i);curW+=w;
  }
  if(cur.length)rows.push({ids:cur,w:curW,last:true});
}
function rowY(ri){let y=0;for(let r=0;r<ri;r++)y+=rowH(rows[r]);return y}
function rowH(row){
  const target=tileH*zoom, W=viewWpx();
  if(row.last)return Math.min(target,W/row.ids.reduce((s,i)=>s+aspect(i),0));
  return target*(W/row.w)/1||0;
}
// Item dims are authoritative; when the manifest has none (0x0 — sniff
// failed or a stale entry), fall back to the decoded thumbnail's real
// aspect so tiles never render stretched into a wrong box.
const aspect=i=>{const it=items[i];
  if(!it||it.removed)return 1;
  if(it.h>0&&it.w>0)return it.w/it.h;
  const b=thumbs.get(i);
  return b&&b.height>0?b.width/b.height:1};

function tileRects(){
  layout();   // no-op when clean — scrollToIndex may run before a draw
  const out=[];
  let y=0;
  for(const row of rows){
    const h=rowH(row); let x=xOff();
    const scale=row.last?h/(tileH*zoom):viewWpx()/row.w;
    for(const i of row.ids){
      const w=aspect(i)*tileH*zoom*scale;
      out.push({i,x,y,w,h});x+=w;
    }
    y+=h;
  }
  return out;
}

// After a bulk remove, land on the first live tile at/after `i`
// (falls back to the last live tile — the "last non deleted image").
function firstLiveAtOrAfter(i){
  for(let j=i;j<items.length;j++)if(items[j]&&!items[j].removed)return j;
  for(let j=Math.min(i,items.length-1);j>=0;j--)if(items[j]&&!items[j].removed)return j;
  return -1;
}
function scrollToIndex(i){
  const r=tileRects().find(t=>t.i===i);
  if(r)scrollY=Math.max(0,r.y-8*devicePixelRatio);
}

// ---- drag-select state (mode comes from the manifest) ----
let mouseSelect="sequence", drag=null, suppressClick=false;
const tileAt=(x,y)=>{
  for(const t of tileRects())
    if(x>=t.x&&x<t.x+t.w&&y>=t.y&&y<t.y+t.h)return t.i;
  return -1};
const tileNearest=(x,y)=>{
  let b=-1,bd=Infinity;
  for(const t of tileRects()){
    const dx=t.x+t.w/2-x,dy=t.y+t.h/2-y,d=dx*dx+dy*dy;
    if(d<bd){bd=d;b=t.i}}
  return b};

// ---- `f` fullscreen preview (1920-class JPEG from /bulk) ----
let fsMode=false, screenBmp=null, screenIdx=-1;
const fsBar=document.getElementById("fsbar"),
      fsPathEl=document.getElementById("fspath"),
      fsCopyEl=document.getElementById("fscopy");
const fsItemPath=i=>{
  const it=items[i];
  return it&&manifest?`${manifest.dirs[it.dir_index]}/${it.name}`:"";
};
fsCopyEl.onclick=()=>{
  const p=fsItemPath(focus);if(!p)return;
  const done=()=>{fsCopyEl.textContent="copied";
    setTimeout(()=>fsCopyEl.textContent="copy",1200)};
  const legacy=()=>{   // clipboard API needs a secure context
    const ta=document.createElement("textarea");ta.value=p;
    document.body.appendChild(ta);ta.select();
    try{document.execCommand("copy");done()}catch(e){}
    ta.remove();};
  if(navigator.clipboard)navigator.clipboard.writeText(p).then(done,legacy);
  else legacy();
};
// Last mouse position in client coords (-1 = outside). Consulted once
// when `f` mode is entered — the fullscreen preview does not follow
// the pointer; Left/Right arrows navigate instead.
let mouseX=-1, mouseY=-1;
const formatSize=b=>{
  const u=["B","KB","MB","GB","TB"];let i=0,v=b;
  while(v>=1024&&i<u.length-1){v/=1024;i++}
  return i?`${v.toFixed(1)} ${u[i]}`:`${v} ${u[i]}`;
};
const selBytes=()=>{let s=0;for(const i of selected)s+=(items[i]&&items[i].bytes)||0;return s};
const screenFetching=new Set();
async function fetchScreen(i){
  const it=items[i];
  if(!it||it.removed||screenFetching.has(i))return;
  screenFetching.add(i);
  try{
    const b=new Uint8Array(14),v=new DataView(b.buffer);
    v.setUint32(0,it.id,true);v.setBigUint64(4,BigInt(it.sig),true);
    v.setUint8(12,2);v.setUint8(13,80);   // fmt=2: ≤1920 screen preview
    const {body}=await bulkRequest(BK_GET_FULL,b);
    const bmp=await createImageBitmap(new Blob([body],{type:"image/jpeg"}));
    screenBmp=bmp;screenIdx=i;if(fsMode)scheduleDraw();
  }finally{screenFetching.delete(i)}
}

// draw() is O(items) — during delete storms hundreds of patches and
// thumb pushes arrive per second, so calls are coalesced to one redraw
// per frame instead of one per event.
let drawQueued=false;
function scheduleDraw(){
  if(drawQueued)return;
  drawQueued=true;
  requestAnimationFrame(()=>{drawQueued=false;draw()});
}

// Rotating arc for a tile whose preview is still being generated. Driven
// by ensureSpin()'s rAF loop while any want is in flight — without it the
// spinner would only move when something else triggers a redraw.
function drawSpinner(cx,cy,r){
  const ph=performance.now()/1000*Math.PI*2*0.7;   // ~0.7 s per turn
  ctx.lineCap="round";
  ctx.strokeStyle="rgba(255,255,255,.14)";ctx.lineWidth=3;
  ctx.beginPath();ctx.arc(cx,cy,r,0,Math.PI*2);ctx.stroke();
  ctx.strokeStyle="rgba(255,255,255,.85)";
  ctx.beginPath();ctx.arc(cx,cy,r,ph,ph+Math.PI*0.9);ctx.stroke();
  ctx.lineCap="butt";
}
// Frames while previews are outstanding. draw() re-arms this as long as
// pending/wants remain, so it stops by itself when all thumbs arrived.
let spinRAF=false;
function ensureSpin(){
  if(spinRAF)return;
  spinRAF=true;
  requestAnimationFrame(()=>{spinRAF=false;draw()});
}

function draw(){
  const W=canvas.width=innerWidth*devicePixelRatio|0,
        H=canvas.height=innerHeight*devicePixelRatio|0;
  // The canvas is sized here, so the layout (which uses canvas.width) must
  // be (re)computed inside draw() — boot calls draw() before canvas.width
  // is meaningful otherwise.
  layout();
  // Deletions and resyncs shrink the grid — clamp the scroll position so
  // the viewport can't strand past the new content end.
  scrollY=Math.min(scrollY,Math.max(0,rowY(rows.length)-H));
  canvas.style.width=innerWidth+"px";canvas.style.height=innerHeight+"px";
  ctx.fillStyle="#111";ctx.fillRect(0,0,W,H);
  fsBar.style.display=fsMode?"flex":"none";

  if(fsMode){
    ctx.fillStyle="#000";ctx.fillRect(0,0,W,H);
    const fi=focus,it=items[fi];
    if(screenIdx===fi&&screenBmp){
      const s=Math.min(W/screenBmp.width,H/screenBmp.height),
            w=screenBmp.width*s,h=screenBmp.height*s;
      ctx.drawImage(screenBmp,(W-w)/2,(H-h)/2,w,h);
    }else{
      if(it&&!it.removed)fetchScreen(fi);
      ctx.fillStyle="#888";ctx.font=`${16*devicePixelRatio}px monospace`;
      ctx.fillText("loading fullscreen preview…",W/2-160*devicePixelRatio,H/2);
    }
    fsPathEl.textContent=fsItemPath(fi);
    fsPathEl.title=fsPathEl.textContent;
    let liveFs=0;for(const it2 of items)if(it2&&!it2.removed)liveFs++;
    topEl.textContent=`${liveFs} images · fullscreen`;
    return;
  }
  // Missing tiles: everything on screen first, then the prefetch ring (one
  // screen ahead, a quarter behind). Rebuilt every frame so a scroll always
  // outranks whatever is still queued from the previous viewport — an
  // append-only queue lets offscreen ids starve the tiles you are looking at.
  const want=[], ahead=[], nowMs=Date.now();
  for(const t of tileRects()){
    const y=t.y-scrollY;
    const rt=decRetry.get(t.i);
    const missing=!thumbs.has(t.i)&&!pending.has(t.i)&&(!rt||rt.next<=nowMs);
    if(y+t.h>=0&&y<=H){
      if(missing){pending.set(t.i,Date.now());want.push(t.i)}
    }else if(missing&&y+t.h>=-H/4&&y<=H*2){
      pending.set(t.i,Date.now());ahead.push(t.i);
    }
    if(y+t.h<0||y>H)continue;
    const bmp=thumbs.get(t.i);
    if(bmp)ctx.drawImage(bmp,t.x,y,t.w,t.h);
    else{
      ctx.fillStyle="#1c1c1c";ctx.fillRect(t.x,y,t.w,t.h);
      // No bitmap yet: a want is in flight → spinner (the server is
      // generating/serving it); a failed decode shows a static dim mark.
      if(pending.has(t.i))drawSpinner(t.x+t.w/2,y+t.h/2,Math.min(16,t.w/6,t.h/6));
      else if(decRetry.has(t.i)){
        ctx.strokeStyle="rgba(255,90,90,.6)";ctx.lineWidth=2;ctx.lineCap="round";
        const cx=t.x+t.w/2,cy=y+t.h/2,r=Math.min(10,t.w/6,t.h/6);
        ctx.beginPath();
        ctx.moveTo(cx-r,cy-r);ctx.lineTo(cx+r,cy+r);
        ctx.moveTo(cx+r,cy-r);ctx.lineTo(cx-r,cy+r);
        ctx.stroke();ctx.lineCap="butt";
      }
    }
    if(selected.has(t.i)){ctx.strokeStyle="#4ea1ff";ctx.lineWidth=4;
      ctx.strokeRect(t.x+2,y+2,t.w-4,t.h-4);}
    if(t.i===focus){ctx.strokeStyle="#fff";ctx.lineWidth=2;
      ctx.strokeRect(t.x+5,y+5,t.w-10,t.h-10);}
  }
  if(drag&&drag.rect){
    const r=drag.rect,y=r.y-scrollY;
    ctx.fillStyle="rgba(78,161,255,0.22)";ctx.fillRect(r.x,y,r.w,r.h);
    ctx.strokeStyle="#4ea1ff";ctx.lineWidth=2;ctx.strokeRect(r.x,y,r.w,r.h);
  }
  // Keep animating while the server still owes us previews.
  if(pending.size||wantQ.length||decJobs.size)ensureSpin();
  // Replace the queue with the current wish-list; release the `pending`
  // marker of anything dropped so a later frame can request it again.
  const wanted=new Set(want.concat(ahead));
  for(const id of wantQ)if(!wanted.has(id))pending.delete(id);
  wantQ=want.concat(ahead);
  pumpWants();
  let delQ=0,movQ=0,live=0;
  for(const a of pendingActions.values()){if(a.kind==="delete")delQ++;else if(a.kind==="move")movQ++}
  for(const it of items)if(it&&!it.removed)live++;
  topEl.textContent=`${live} images · ${thumbs.size} cached · ${selected.size} selected`+
    `${selected.size?` (${formatSize(selBytes())})`:""}${delQ?` · deleting ${delQ}`:""}`+
    `${movQ?` · moving ${movQ}`:""}${scanning?" · scanning…":""}`;
}

let dataWs=null;

// ---- input ----
// Tags carry a per-client random namespace in the high 12 bits — the
// action-done broadcast reaches every client, so foreign tags must not
// collide with ours. (Multiplication, not << — JS bitwise ops go int32
// and would turn high tags negative.)
let pendingTagBase=Math.floor(Math.random()*0x1000)*0x100000, pendingCtr=0,
    pendingActions=new Map();
const nextTag=()=>pendingTagBase+((pendingCtr++)&0xfffff);
addEventListener("wheel",e=>{if(fsMode)return;scrollY=Math.max(0,scrollY+e.deltaY*devicePixelRatio);scheduleDraw()});
addEventListener("keydown",e=>{
  const cols=()=>Math.max(1,Math.floor(canvas.width/(tileH*zoom)));
  if(e.key==="+"||e.key==="="){zoom*=1.1;layoutDirty=true;scheduleDraw()}
  else if(e.key==="-"){zoom/=1.1;layoutDirty=true;scheduleDraw()}
  else if(e.key.toLowerCase()==="w"&&!e.ctrlKey&&!e.metaKey){viewW=e.shiftKey?Math.min(1,viewW*1.25):Math.max(0.05,viewW/1.25);layoutDirty=true;scheduleDraw()}
  else if(e.key==="Delete"||(e.key.toLowerCase()==="m"&&!e.ctrlKey&&!e.metaKey)){
    const kind=e.key==="Delete"?"delete":"move";
    // No selection → the focused tile is the target (native parity).
    const targets=selected.size?[...selected]:
      (items[focus]&&!items[focus].removed?[focus]:[]);
    const at=targets.length?Math.min(...targets):focus;
    for(const i of targets){
      const it=items[i];if(!it||it.removed)continue;const tag=nextTag();
      pendingActions.set(tag,{kind,at:Date.now(),queued:false,ids:[it.id]});
      // Optimistic: the tile vanishes now, the FS op lands async.
      it.removed=true;thumbs.delete(it.id);
      dataWs.send(frame(CL_ACTION,0,str(JSON.stringify({
        tag,kind,id:it.id,sig:it.sig}))));
    }
    selected.clear();layoutDirty=true;
    // Scroll to the survivor at the deletion point (or the last
    // remaining image when the deleted block ran to the end) — but
    // only when it isn't already on screen: deleting visible tiles
    // reflows the next rows into view on their own.
    const f=firstLiveAtOrAfter(at);
    if(f>=0){
      focus=f;anchor=f;
      const r=tileRects().find(t=>t.i===f);
      if(!(r&&r.y+r.h>scrollY&&r.y<scrollY+canvas.height))scrollToIndex(f);
    }
    scheduleDraw();
  }
  else if((e.key==="f"||e.key==="F")&&!e.ctrlKey&&!e.metaKey&&!e.repeat){
    // Toggle fullscreen preview. Hover picks the entry image once;
    // from then on only Left/Right move the viewed image.
    if(fsMode)fsMode=false;
    else{
      fsMode=true;
      const h=tileAt(mouseX*devicePixelRatio,mouseY*devicePixelRatio+scrollY);
      if(h>=0){focus=h;anchor=h}
    }
    scheduleDraw();
  }
  else if(e.key==="Escape"&&fsMode){fsMode=false;scheduleDraw()}
  else if(e.key==="c"||e.key==="Escape"){}
  else if(e.key.startsWith("Arrow")){
    if(fsMode){
      // Fullscreen mode: Left/Right step through live images, skipping
      // tombstones; Up/Down do nothing.
      if(e.key==="ArrowLeft"||e.key==="ArrowRight"){
        const dir=e.key==="ArrowLeft"?-1:1;
        for(let j=focus+dir;j>=0&&j<items.length;j+=dir)
          if(items[j]&&!items[j].removed){focus=j;anchor=j;break}
        scheduleDraw();
      }
      e.preventDefault();return;
    }
    const n=items.length;let nf=focus;
    if(e.key==="ArrowRight")nf=Math.min(n-1,focus+1);
    if(e.key==="ArrowLeft")nf=Math.max(0,focus-1);
    if(e.key==="ArrowDown")nf=Math.min(n-1,focus+cols());
    if(e.key==="ArrowUp")nf=Math.max(0,focus-cols());
    if(e.ctrlKey){focus=nf}
    else if(e.shiftKey){const[a,b]=[Math.min(anchor,nf),Math.max(anchor,nf)];
      for(let i=a;i<=b;i++)selected.add(i);focus=nf}
    else{focus=nf;anchor=nf;selected=new Set([nf])}
    scheduleDraw();
  }
  else if(e.key==="Home"||e.key==="End"){
    // Jump to the first/last live image — the selection is never reset:
    // plain Home/End only moves focus (anchor stays put so a later
    // Shift+Arrow still extends from where the user anchored), and
    // Shift+Home/End extends the selection from the anchor to the edge.
    const t=items.length?
      (e.key==="Home"?firstLiveAtOrAfter(0):firstLiveAtOrAfter(items.length-1)):-1;
    if(t>=0){
      if(e.shiftKey){const[a,b]=[Math.min(anchor,t),Math.max(anchor,t)];
        for(let i=a;i<=b;i++)if(items[i]&&!items[i].removed)selected.add(i);}
      focus=t;
      const r=tileRects().find(t2=>t2.i===t),H=canvas.height,m=8*devicePixelRatio;
      if(r){
        if(r.y<scrollY)scrollY=Math.max(0,r.y-m);          // above → top-align
        else if(r.y+r.h>scrollY+H)scrollY=r.y+r.h+m-H;   // below → bottom-align
      }
    }
    scheduleDraw();e.preventDefault();
  }
  else if(e.key==="d"||e.key==="D"){
    // Download the untouched original file of the focused tile.
    const it=items[focus];
    if(it&&!it.removed){
      const a=document.createElement("a");
      a.href=`/orig?id=${it.id}&token=${encodeURIComponent(token)}`;
      a.download=it.name;document.body.appendChild(a);a.click();a.remove();
    }
    e.preventDefault();
  }
  else if(e.key===" "){selected.has(focus)?selected.delete(focus):selected.add(focus);scheduleDraw();e.preventDefault()}
  else if(e.key==="a"&&e.ctrlKey){for(let i=0;i<items.length;i++)selected.add(i);scheduleDraw();e.preventDefault()}
});
canvas.addEventListener("mousedown",e=>{
  if(fsMode||e.button!==0)return;
  const x=e.clientX*devicePixelRatio,y=e.clientY*devicePixelRatio+scrollY;
  const a=tileAt(x,y);
  drag={x0:x,y0:y,anchor:a>=0?a:tileNearest(x,y),
    mode:(e.ctrlKey||e.metaKey)?2:(e.shiftKey?1:0),
    base:new Set(selected),rect:null};
  e.preventDefault();
});
addEventListener("mousemove",e=>{
  mouseX=e.clientX;mouseY=e.clientY;
  if(fsMode)return;   // fullscreen view does not follow the pointer
  if(!drag)return;
  const x=e.clientX*devicePixelRatio,y=e.clientY*devicePixelRatio+scrollY;
  const dx=x-drag.x0,dy=y-drag.y0;
  if(!drag.rect&&dx*dx+dy*dy<36)return;   // <6px = still a click
  drag.rect={x:Math.min(drag.x0,x),y:Math.min(drag.y0,y),
    w:Math.abs(dx),h:Math.abs(dy)};
  let hits;
  if(mouseSelect==="rubber-band"){
    hits=new Set();
    for(const t of tileRects())
      if(t.x<drag.rect.x+drag.rect.w&&t.x+t.w>drag.rect.x&&
         t.y<drag.rect.y+drag.rect.h&&t.y+t.h>drag.rect.y)hits.add(t.i);
  }else{
    // sequence: contiguous index run anchor..cursor-tile
    hits=new Set();
    const c0=tileAt(x,y),cur=c0>=0?c0:tileNearest(x,y);
    if(drag.anchor<0)drag.anchor=cur;
    if(drag.anchor>=0&&cur>=0)
      for(let i=Math.min(drag.anchor,cur);i<=Math.max(drag.anchor,cur);i++)
        hits.add(i);
  }
  const b=drag.base;
  selected=drag.mode===1?new Set([...b,...hits])
    :drag.mode===2
      ?new Set([...b].filter(i=>!hits.has(i))
        .concat([...hits].filter(i=>!b.has(i))))
      :hits;
  scheduleDraw();
});
addEventListener("mouseup",e=>{
  if(e.button!==0)return;
  if(drag&&drag.rect)suppressClick=true;
  drag=null;scheduleDraw();
});
document.addEventListener("mouseleave",()=>{mouseX=mouseY=-1});
canvas.addEventListener("click",e=>{
  if(fsMode)return;
  if(suppressClick){suppressClick=false;return}
  const mx=e.clientX*devicePixelRatio,my=e.clientY*devicePixelRatio+scrollY;
  for(const t of tileRects()){
    if(mx>=t.x&&mx<t.x+t.w&&my>=t.y&&my<t.y+t.h){
      if(e.shiftKey){const[a,b]=[Math.min(anchor,t.i),Math.max(anchor,t.i)];
        for(let i=a;i<=b;i++)selected.add(i)}
      else if(e.ctrlKey||e.metaKey){selected.has(t.i)?selected.delete(t.i):selected.add(t.i);anchor=t.i}
      else{selected=new Set([t.i]);anchor=t.i}
      focus=t.i;scheduleDraw();break;
    }
  }
});
addEventListener("resize",()=>{layoutDirty=true;scheduleDraw()});

// ---- debug instrumentation (ic() / icServer() in the JS console) ----
const icStats={wantSent:0,thumbRecv:0,thumbDrop:0,thumbDup:0,batchRecv:0,
  thumbOk:0,decFail:0,resyncs:0,restored:0,thumbClears:0,
  lastFrameAt:0,lastFrameType:0,lastEventAt:0,lastEventEv:"",
  lastThumbMagic:null,frames:{},errs:[]};
const decJobs=new Map();   // thumb id -> {w:workerIdx,at:ms} in decode
window.addEventListener("error",e=>{
  icStats.errs.push(`${Date.now()}: ${e.message} @${e.lineno}`);
  if(icStats.errs.length>50)icStats.errs.shift()});
window.addEventListener("unhandledrejection",e=>{
  icStats.errs.push(`${Date.now()}: rejection ${e.reason}`);
  if(icStats.errs.length>50)icStats.errs.shift()});
// Snapshot of everything that can stall the thumbnail pipeline.
window.ic=function(){
  const now=Date.now(),s={
    items:items.length,
    live:items.reduce((n,i)=>n+(i&&!i.removed?1:0),0),
    manifestVersion:manifest&&manifest.version,
    dirs:manifest?manifest.dirs.length:0,
    thumbs:thumbs.size,
    pending:pending.size,
    pendingOldestMs:pending.size?Math.max(...[...pending.values()].map(at=>now-at)):0,
    pendingIds:[...pending.keys()],
    wantQ:wantQ.length,
    wantSlots:[...wantSlots].map(([id,sl])=>({req:id,left:sl.left.size,
      ids:[...sl.left],ageMs:now-sl.at})),
    scanning,scanErrs,reloading,
    ws:{data:dataWs&&dataWs.readyState,bulk:bulkWs&&bulkWs.readyState},
    stats:icStats,
    decJobs:[...decJobs].map(([id,j])=>({id,w:j.w,ageMs:now-j.at})),
    decRetry:[...decRetry].map(([id,r])=>({id,n:r.n,nextInMs:r.next-now})),
    decWorkers:decPool.length,
    fsMode,focus,screenIdx,screenFetching:[...screenFetching],
    pendingActions:[...pendingActions].map(([t,a])=>({tag:t,kind:a.kind,
      queued:a.queued,ids:a.ids,ageMs:now-a.at})),
    layout:{rows:rows.length,scrollY,tileH,zoom},
    // Manifest entry sanity: 0x0 dims here = aspect falls back to 1 and
    // tiles render square/stretched; a sig that doesn't match the file on
    // disk = every action fails "stale signature".
    sample:(function(){const out=[];
      for(let i=0;i<items.length&&out.length<6;i++){
        const it=items[i];if(!it)continue;
        const b=thumbs.get(i);
        out.push({id:i,name:it.name,w:it.w,h:it.h,sig:String(it.sig),
          dir:manifest?manifest.dirs[it.dir_index]:undefined,
          thumb:b?`${b.width}x${b.height}`:null});}
      return out})(),
  };
  console.log(s);
  return s;
};
// Server-side counters (/debugz) — resolves "is the server still pushing?".
window.icServer=function(){
  return fetch("/debugz").then(r=>r.json()).then(j=>{console.log(j);return j});
};

boot().catch(e=>{
  // A socket-close already scheduled a reload — don't prompt on top of it.
  if(reloading)return;
  // WS upgrade refused — most likely auth (--auth=token / -p password).
  statusEl.textContent="connect failed — "+e;
  const t=prompt("Connection refused — enter the server token/password to retry:");
  if(t!=null&&t!==""){
    const u=new URL(location.href);u.searchParams.set("token",t);location.href=u;
  }
});
