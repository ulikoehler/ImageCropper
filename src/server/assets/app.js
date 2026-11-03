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
helpEl.textContent="scroll: wheel · zoom: +/- · width: w/W · select: click/ctrl/shift · delete: Del · move: m · crop: c";

let manifest=null, items=[], thumbs=new Map(), pending=new Set();
let tileH=180, zoom=1.0, scrollY=0, selected=new Set(), focus=0, anchor=0;
let creditBudget=8*1024*1024;

function connect(path){ return new Promise((res,rej)=>{
  const ws=new WebSocket(`ws://${location.host}${path}`);
  ws.binaryType="arraybuffer"; ws.onopen=()=>res(ws); ws.onerror=rej; });
}

async function boot(){
  const bulk=await connect("/bulk"), data=await connect("/data");

  // /data: hello then handle thumbs+events
  data.onmessage=e=>onData(unframe(e.data));
  data.send(frame(CL_HELLO,0,str(JSON.stringify({token,manifest_version:0}))));

  // /bulk: manifest (chunked)
  const parts=[]; let got=0;
  const manifestReady=new Promise(res=>{
    bulk.onmessage=e=>{const f=unframe(e.data);
      if(f.type===BK_MANIFEST){parts.push(f.payload);got+=f.payload.length;
        if(f.flags&1){const all=new Uint8Array(got);let o=0;
          for(const p of parts){all.set(p,o);o+=p.length}
          res(JSON.parse(new TextDecoder().decode(all)));}}}});
  bulk.send(frame(BK_GET_MANIFEST,1,u32s(1,0)));
  manifest=await manifestReady;
  items=manifest.items;
  layout(); draw();

  // credits: keep the push pipeline fed
  const grant=()=>{data.send(frame(CL_CREDIT,0,(()=>{const b=new Uint8Array(8),
    v=new DataView(b.buffer);v.setBigUint64(0,BigInt(creditBudget),true);return b})()));};
  grant(); setInterval(grant, 500);

  data.onclose=()=>setTimeout(()=>location.reload(),1500);
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
      if(kind!=null&&ev.ok){
        const id=pendingIds.get(ev.tag);
        pendingActions.delete(ev.tag);pendingIds.delete(ev.tag);
        const idx=items.findIndex(it=>it.id===id);
        if(idx>=0){
          items.splice(idx,1);
          thumbs.delete(id);
          selected.delete(idx);
          // indices shift after splice — remap remaining selection
          selected=new Set([...selected].map(i=>i>idx?i-1:i));
          if(focus>idx)focus--;if(anchor>idx)anchor--;
        }
        layout();draw();
      } else if(kind!=null){
        pendingActions.delete(ev.tag);pendingIds.delete(ev.tag);
        statusEl.textContent=`${kind} failed: ${ev.error||"?"}`;
      }
    }
    if(ev.ev==="fs-changed") location.reload();
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
  const aspect=i=>{const it=items[i];
    return it.h>0?it.w/it.h:1};
  for(let i=0;i<items.length;i++){
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
const aspect=i=>{const it=items[i];return it.h>0?it.w/it.h:1};

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

function draw(){
  const W=canvas.width=innerWidth*devicePixelRatio|0,
        H=canvas.height=innerHeight*devicePixelRatio|0;
  // The canvas is sized here, so the layout (which uses canvas.width) must
  // be (re)computed inside draw() — boot calls draw() before canvas.width
  // is meaningful otherwise.
  layout();
  canvas.style.width=innerWidth+"px";canvas.style.height=innerHeight+"px";
  ctx.fillStyle="#111";ctx.fillRect(0,0,W,H);
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
  if(want.length){
    const b=new Uint8Array(2+4*want.length),v=new DataView(b.buffer);
    v.setUint16(0,want.length,true);
    want.forEach((id,i)=>v.setUint32(2+4*i,id,true));
    dataWs.send(frame(CL_WANT,0,b));
  }
  topEl.textContent=`${items.length} images · ${thumbs.size} cached · ${selected.size} selected`;
}

let dataWs=null;
const _connect=connect;
connect=async p=>{const ws=await _connect(p);if(p==="/data")dataWs=ws;return ws};

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
      const it=items[i],tag=pendingTag++;
      pendingActions.set(tag,"delete");pendingIds.set(tag,it.id);
      dataWs.send(frame(CL_ACTION,0,str(JSON.stringify({
        tag,kind:"delete",id:it.id,sig:it.sig}))));
    }
    selected.clear();draw();
  }
  else if((e.key==="m"||e.key==="M")&&!e.ctrlKey&&!e.metaKey){
    for(const i of selected){
      const it=items[i],tag=pendingTag++;
      pendingActions.set(tag,"move");pendingIds.set(tag,it.id);
      dataWs.send(frame(CL_ACTION,0,str(JSON.stringify({
        tag,kind:"move",id:it.id,sig:it.sig}))));
    }
    selected.clear();draw();
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
canvas.addEventListener("click",e=>{
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

boot().catch(e=>statusEl.textContent="connect failed: "+e);
