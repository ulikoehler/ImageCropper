// ImageCropper web client (minimal Canvas2D implementation over the WS
// protocol). The WebGPU/wasm client replaces this; the protocol is the same.
"use strict";

const EV_EVENT=0x01, EV_THUMB=0x02, EV_THUMB_BATCH=0x03, EV_PONG=0x06;
const CL_HELLO=0x10, CL_WANT=0x11, CL_VIEWPORT=0x12, CL_FOCUS=0x13,
      CL_ACTION=0x14, CL_CREDIT=0x15, CL_PAUSE=0x16, CL_RESUME=0x17,
      CL_PING=0x18;
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
helpEl.textContent="scroll: wheel · zoom: +/- · width: w/W · select: click/ctrl/shift · delete: Del · move: m · fullscreen: hold f · crop: c";

let manifest=null, items=[], thumbs=new Map(), pending=new Set();
let scanning=false;
let tileH=180, zoom=1.0, scrollY=0, selected=new Set(), focus=0, anchor=0;
let creditBudget=8*1024*1024;

function connect(path){ return new Promise((res,rej)=>{
  const ws=new WebSocket(`ws://${location.host}${path}?token=${encodeURIComponent(token)}`);
  ws.binaryType="arraybuffer"; ws.onopen=()=>res(ws); ws.onerror=rej; });
}

// /bulk request router: GET_MANIFEST + GET_FULL replies share the socket,
// correlated by req_id.
let bulkWs=null;
const bulkReqs=new Map(); let bulkReqId=0x100;
function bulkRequest(type,payload){
  const reqId=bulkReqId++;
  return new Promise(res=>{
    bulkReqs.set(reqId,{parts:[],head:null,res});
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
    bulkReqs.delete(f.reqId);
    const all=new Uint8Array(req.parts.reduce((s,p)=>s+p.length,0));let o=0;
    for(const p of req.parts){all.set(p,o);o+=p.length}
    req.res({head:req.head,body:all});
  }
}

async function refreshManifest(){
  const {body}=await bulkRequest(BK_GET_MANIFEST,u32s(1,0));
  manifest=JSON.parse(new TextDecoder().decode(body));
  mouseSelect=manifest.mouse_select||"sequence";
  items=manifest.items;
  thumbs.clear();pending.clear();
  selected=new Set();focus=0;anchor=0;scanning=false;
  screenBmp=null;screenIdx=-1;
  layout();draw();
}

// ids are positional (items[id]); tombstoned entries keep their slot.
function applyPatch(p){
  if(p.resync){
    items=[];manifest.dirs=[];thumbs.clear();pending.clear();
    selected=new Set();focus=0;anchor=0;scanning=false;
    screenBmp=null;screenIdx=-1;
    manifest.dirs=(p.dirs_added||[]).slice();
  }else{
    (p.dirs_added||[]).forEach(d=>manifest.dirs.push(d));
  }
  (p.upsert||[]).forEach(it=>{items[it.id]=it});
  (p.remove||[]).forEach(id=>{if(items[id])items[id].removed=true;thumbs.delete(id)});
  manifest.version=p.version;
  layout();draw();
}

async function boot(){
  const bulk=await connect("/bulk"), data=await connect("/data");
  bulkWs=bulk;dataWs=data;
  bulk.onmessage=e=>onBulk(unframe(e.data));

  // /data: hello then handle thumbs+events
  data.onmessage=e=>onData(unframe(e.data));
  data.send(frame(CL_HELLO,0,str(JSON.stringify({token,manifest_version:0}))));

  // /bulk: manifest (chunked)
  const {body}=await bulkRequest(BK_GET_MANIFEST,u32s(1,0));
  manifest=JSON.parse(new TextDecoder().decode(body));
  mouseSelect=manifest.mouse_select||"sequence";
  items=manifest.items;
  layout(); draw();

  // credits: keep the push pipeline fed
  const grant=()=>{if(data.readyState===1){
    data.send(frame(CL_CREDIT,0,(()=>{const b=new Uint8Array(8),
      v=new DataView(b.buffer);v.setBigUint64(0,BigInt(creditBudget),true);return b})()));}};
  grant(); setInterval(grant, 500);

  data.onclose=()=>{statusEl.textContent="connection closed — reloading…";
    setTimeout(()=>location.reload(),1500);};
}

const thumbReqs=[];
function onData(f){
  if(f.type===EV_THUMB){
    const v=new DataView(f.payload.buffer,f.payload.byteOffset);
    const id=v.getUint32(0,true);
    const jpeg=f.payload.subarray(12);
    pending.delete(id);
    createImageBitmap(new Blob([jpeg],{type:"image/jpeg"})).then(bmp=>{
      thumbs.set(id,bmp); draw();});
  } else if(f.type===EV_EVENT){
    const ev=JSON.parse(new TextDecoder().decode(f.payload));
    if(ev.ev==="action-done"){
      const kind=pendingActions.get(ev.tag);
      const id=pendingIds.get(ev.tag);
      if(kind!=null&&ev.ok){
        pendingActions.delete(ev.tag);pendingIds.delete(ev.tag);
        // Optimistic tombstone — the server's manifest-patch remove
        // arrives too; both are idempotent and keep ids positional.
        if(items[id])items[id].removed=true;
        thumbs.delete(id);selected.delete(id);
        layout();draw();
      } else if(kind!=null){
        pendingActions.delete(ev.tag);pendingIds.delete(ev.tag);
        // Undo the optimistic tombstone — the file is still there.
        if(items[id]){items[id].removed=false;layout();}
        statusEl.textContent=`${kind} failed: ${ev.error||"?"}`;
        draw();
      }
    }
    if(ev.ev==="scan-start"){scanning=true;draw();}
    if(ev.ev==="manifest-patch"){applyPatch(ev.patch||{});}
    if(ev.ev==="fs-changed"){refreshManifest();}
    if(ev.ev==="progress") statusEl.textContent=
      `previews ${ev.done}/${ev.queued}`;
  }
}

// ---- justified layout (same math as the native album) ----
let rows=[];
function layout(){
  rows=[]; const W=canvas.width;
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
  const target=tileH*zoom, W=canvas.width;
  if(row.last)return Math.min(target,W/row.ids.reduce((s,i)=>s+aspect(i),0));
  return target*(W/row.w)/1||0;
}
const aspect=i=>{const it=items[i];return (!it||it.removed)?1:(it.h>0?it.w/it.h:1)};

function tileRects(){
  const out=[];
  let y=0;
  for(const row of rows){
    const h=rowH(row); let x=0;
    const scale=row.last?h/(tileH*zoom):canvas.width/row.w;
    for(const i of row.ids){
      const w=aspect(i)*tileH*zoom*scale;
      out.push({i,x,y,w,h});x+=w;
    }
    y+=h;
  }
  return out;
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

// ---- hold-`f` fullscreen preview (1920-class JPEG from /bulk) ----
let fHeld=false, screenBmp=null, screenIdx=-1;
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
    screenBmp=bmp;screenIdx=i;if(fHeld)draw();
  }finally{screenFetching.delete(i)}
}

function draw(){
  const W=canvas.width=innerWidth*devicePixelRatio|0,
        H=canvas.height=innerHeight*devicePixelRatio|0;
  // The canvas is sized here, so the layout (which uses canvas.width) must
  // be (re)computed inside draw() — boot calls draw() before canvas.width
  // is meaningful otherwise.
  layout();
  canvas.style.width=innerWidth+"px";canvas.style.height=innerHeight+"px";
  ctx.fillStyle="#111";ctx.fillRect(0,0,W,H);

  if(fHeld){
    ctx.fillStyle="#000";ctx.fillRect(0,0,W,H);
    const it=items[focus];
    if(screenIdx===focus&&screenBmp){
      const s=Math.min(W/screenBmp.width,H/screenBmp.height),
            w=screenBmp.width*s,h=screenBmp.height*s;
      ctx.drawImage(screenBmp,(W-w)/2,(H-h)/2,w,h);
    }else{
      if(it&&!it.removed)fetchScreen(focus);
      ctx.fillStyle="#888";ctx.font=`${16*devicePixelRatio}px monospace`;
      ctx.fillText("loading fullscreen preview…",W/2-160*devicePixelRatio,H/2);
    }
    topEl.textContent=`${items.length} images · fullscreen: ${it?it.name:"?"}`;
    return;
  }
  const want=[];
  for(const t of tileRects()){
    const y=t.y-scrollY;
    if(y+t.h<0||y>H)continue;
    const bmp=thumbs.get(t.i);
    if(bmp)ctx.drawImage(bmp,t.x,y,t.w,t.h);
    else{ctx.fillStyle="#1c1c1c";ctx.fillRect(t.x,y,t.w,t.h);
      if(!pending.has(t.i)){pending.add(t.i);want.push(t.i)}}
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
  if(want.length&&dataWs&&dataWs.readyState===1){
    const b=new Uint8Array(2+4*want.length),v=new DataView(b.buffer);
    v.setUint16(0,want.length,true);
    want.forEach((id,i)=>v.setUint32(2+4*i,id,true));
    dataWs.send(frame(CL_WANT,0,b));
  }
  topEl.textContent=`${items.length} images · ${thumbs.size} cached · ${selected.size} selected${scanning?" · scanning…":""}`;
}

let dataWs=null;

// ---- input ----
let pendingTag=1, pendingActions=new Map(), pendingIds=new Map();
addEventListener("wheel",e=>{scrollY=Math.max(0,scrollY+e.deltaY*devicePixelRatio);draw()});
addEventListener("keydown",e=>{
  const cols=()=>Math.max(1,Math.floor(canvas.width/(tileH*zoom)));
  if(e.key==="+"||e.key==="="){zoom*=1.1;layout();draw()}
  else if(e.key==="-"){zoom/=1.1;layout();draw()}
  else if(e.key==="w"){tileH*=e.shiftKey?1.25:0.8;layout();draw()}
  else if(e.key==="Delete"){
    for(const i of selected){
      const it=items[i];if(!it||it.removed)continue;const tag=pendingTag++;
      pendingActions.set(tag,"delete");pendingIds.set(tag,it.id);
      // Optimistic: the tile vanishes now, the FS delete lands async.
      it.removed=true;thumbs.delete(it.id);
      dataWs.send(frame(CL_ACTION,0,str(JSON.stringify({
        tag,kind:"delete",id:it.id,sig:it.sig}))));
    }
    selected.clear();layout();draw();
  }
  else if((e.key==="m"||e.key==="M")&&!e.ctrlKey&&!e.metaKey){
    for(const i of selected){
      const it=items[i];if(!it||it.removed)continue;const tag=pendingTag++;
      pendingActions.set(tag,"move");pendingIds.set(tag,it.id);
      it.removed=true;thumbs.delete(it.id);
      dataWs.send(frame(CL_ACTION,0,str(JSON.stringify({
        tag,kind:"move",id:it.id,sig:it.sig}))));
    }
    selected.clear();layout();draw();
  }
  else if((e.key==="f"||e.key==="F")&&!e.ctrlKey&&!e.metaKey){
    fHeld=true;if(screenIdx!==focus)fetchScreen(focus);draw();
  }
  else if(e.key==="c"||e.key==="Escape"){}
  else if(e.key.startsWith("Arrow")){
    const n=items.length;let nf=focus;
    if(e.key==="ArrowRight")nf=Math.min(n-1,focus+1);
    if(e.key==="ArrowLeft")nf=Math.max(0,focus-1);
    if(e.key==="ArrowDown")nf=Math.min(n-1,focus+cols());
    if(e.key==="ArrowUp")nf=Math.max(0,focus-cols());
    if(e.ctrlKey){focus=nf}
    else if(e.shiftKey){const[a,b]=[Math.min(anchor,nf),Math.max(anchor,nf)];
      for(let i=a;i<=b;i++)selected.add(i);focus=nf}
    else{focus=nf;anchor=nf;selected=new Set([nf])}
    draw();
  }
  else if(e.key===" "){selected.has(focus)?selected.delete(focus):selected.add(focus);draw();e.preventDefault()}
  else if(e.key==="a"&&e.ctrlKey){for(let i=0;i<items.length;i++)selected.add(i);draw();e.preventDefault()}
});
addEventListener("keyup",e=>{
  if(e.key==="f"||e.key==="F"){fHeld=false;draw()}
});
canvas.addEventListener("mousedown",e=>{
  if(e.button!==0)return;
  const x=e.clientX*devicePixelRatio,y=e.clientY*devicePixelRatio+scrollY;
  const a=tileAt(x,y);
  drag={x0:x,y0:y,anchor:a>=0?a:tileNearest(x,y),
    mode:(e.ctrlKey||e.metaKey)?2:(e.shiftKey?1:0),
    base:new Set(selected),rect:null};
  e.preventDefault();
});
addEventListener("mousemove",e=>{
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
  draw();
});
addEventListener("mouseup",e=>{
  if(e.button!==0)return;
  if(drag&&drag.rect)suppressClick=true;
  drag=null;draw();
});
canvas.addEventListener("click",e=>{
  if(suppressClick){suppressClick=false;return}
  const mx=e.clientX*devicePixelRatio,my=e.clientY*devicePixelRatio+scrollY;
  for(const t of tileRects()){
    if(mx>=t.x&&mx<t.x+t.w&&my>=t.y&&my<t.y+t.h){
      if(e.shiftKey){const[a,b]=[Math.min(anchor,t.i),Math.max(anchor,t.i)];
        for(let i=a;i<=b;i++)selected.add(i)}
      else if(e.ctrlKey||e.metaKey){selected.has(t.i)?selected.delete(t.i):selected.add(t.i);anchor=t.i}
      else{selected=new Set([t.i]);anchor=t.i}
      focus=t.i;draw();break;
    }
  }
});
addEventListener("resize",()=>{layout();draw()});

boot().catch(e=>{
  // WS upgrade refused — most likely auth (--auth=token / -p password).
  statusEl.textContent="connect failed — "+e;
  const t=prompt("Connection refused — enter the server token/password to retry:");
  if(t!=null&&t!==""){
    const u=new URL(location.href);u.searchParams.set("token",t);location.href=u;
  }
});
