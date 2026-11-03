//! wgpu plumbing — replaces the C++ vkmini + shaderc + Vulkan path.
//! Device/queue/pipelines are initialized lazily once; buffer lifetimes
//! are managed by wgpu handles (no explicit session map needed).

use super::coeffs;
use super::jpeg::{
    pack_flut, pack_tables, scan_mode, survey_scan, FrameGpu, Interval, Jpg,
    NoCoef, Scan, K_SURVEY_IV,
};
use super::shaders;
use std::future::Future;
use std::sync::OnceLock;

const CHUNK_MCUS: u32 = 2048;
/// MCU budget per queue submission: a single multi-second submit can
/// get killed by the driver's job watchdog on some GPUs, so (scan,
/// generation) dispatches are grouped into submits under this cap.
const SUBMIT_MCUS: u32 = 32768;

pub struct GpuCtx {
    device: wgpu::Device,
    queue: wgpu::Queue,
    d_bgl: wgpu::BindGroupLayout,
    p_entropy: wgpu::ComputePipeline,
    p_recon: wgpu::ComputePipeline,
    p_upsample: wgpu::ComputePipeline,
    r_bgl: wgpu::BindGroupLayout,
    p_resample: wgpu::ComputePipeline,
    p_pack: wgpu::ComputePipeline,
}

fn block_on<F: Future>(f: F) -> F::Output {
    use std::sync::Arc;
    use std::task::{Context, Poll, Wake, Waker};
    struct Nop;
    impl Wake for Nop {
        fn wake(self: Arc<Self>) {}
    }
    let waker = Waker::from(Arc::new(Nop));
    let mut cx = Context::from_waker(&waker);
    let mut f = std::pin::pin!(f);
    loop {
        match f.as_mut().poll(&mut cx) {
            Poll::Ready(v) => return v,
            Poll::Pending => std::thread::yield_now(),
        }
    }
}

fn pc_bytes(vals: &[u32]) -> Vec<u8> {
    let mut v = Vec::with_capacity(vals.len() * 4);
    for &x in vals {
        v.extend_from_slice(&x.to_le_bytes());
    }
    v
}

fn i32_bytes(vals: &[i32]) -> Vec<u8> {
    let mut v = Vec::with_capacity(vals.len() * 4);
    for &x in vals {
        v.extend_from_slice(&x.to_le_bytes());
    }
    v
}

fn u32_bytes(vals: &[u32]) -> Vec<u8> {
    let mut v = Vec::with_capacity(vals.len() * 4);
    for &x in vals {
        v.extend_from_slice(&x.to_le_bytes());
    }
    v
}

fn buf(device: &wgpu::Device, size: u64, usage: wgpu::BufferUsages) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: size.max(4),
        usage,
        mapped_at_creation: false,
    })
}

fn bgl_entry(binding: u32, ty: wgpu::BufferBindingType) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::Buffer {
            ty,
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
}

fn bg_entry(binding: u32, buf: &wgpu::Buffer) -> wgpu::BindGroupEntry<'_> {
    wgpu::BindGroupEntry {
        binding,
        resource: buf.as_entire_binding(),
    }
}

const RO: wgpu::BufferBindingType = wgpu::BufferBindingType::Storage { read_only: true };
const RW: wgpu::BufferBindingType = wgpu::BufferBindingType::Storage { read_only: false };

impl GpuCtx {
    fn init() -> Option<GpuCtx> {
        let inst = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapter = block_on(inst.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: None,
            force_fallback_adapter: false,
            ..Default::default()
        }))
        .ok()?;
        if !adapter.features().contains(wgpu::Features::IMMEDIATES) {
            return None;
        }
        let mut limits = wgpu::Limits::default();
        limits.max_immediate_size = 128;
        limits.max_storage_buffer_binding_size = adapter.limits().max_storage_buffer_binding_size;
        let (device, queue) = block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("gpujpeg"),
            required_features: wgpu::Features::IMMEDIATES,
            required_limits: limits,
            ..Default::default()
        }))
        .ok()?;

        use wgpu::BufferBindingType::Uniform as UNI;
        let d_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: None,
            entries: &[
                bgl_entry(0, RO),
                bgl_entry(1, RO),
                bgl_entry(2, RO),
                bgl_entry(3, RW),
                bgl_entry(4, UNI),
                bgl_entry(5, RW),
                bgl_entry(6, RW),
                bgl_entry(7, RW),
                bgl_entry(8, RO),
            ],
        });
        let d_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: None,
            bind_group_layouts: &[Some(&d_bgl)],
            immediate_size: 88,
        });
        let r_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: None,
            entries: &[
                bgl_entry(0, RO),
                bgl_entry(1, RO),
                bgl_entry(2, RO),
                bgl_entry(3, RW),
            ],
        });
        let r_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: None,
            bind_group_layouts: &[Some(&r_bgl)],
            immediate_size: 24,
        });
        let mk = |src: &str, layout: &wgpu::PipelineLayout, entry: &str| {
            let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: None,
                source: wgpu::ShaderSource::Wgsl(src.into()),
            });
            device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: None,
                layout: Some(layout),
                module: &module,
                entry_point: Some(entry),
                compilation_options: Default::default(),
                cache: None,
            })
        };
        let ctx = GpuCtx {
            p_entropy: mk(shaders::ENTROPY, &d_layout, "main"),
            p_recon: mk(shaders::RECON, &d_layout, "main"),
            p_upsample: mk(shaders::UPSAMPLE, &d_layout, "main"),
            p_resample: mk(shaders::RESAMPLE, &r_layout, "main"),
            p_pack: mk(shaders::PACK, &r_layout, "main"),
            device,
            queue,
            d_bgl,
            r_bgl,
        };
        Some(ctx)
    }

    fn wait(&self) {
        let _ = self.device.poll(wgpu::PollType::wait_indefinitely());
    }

    /// Copy `size` bytes of `src` into a mapped staging buffer and return
    /// them. One submit; the map resolves after a blocking poll.
    pub fn read_buf(&self, src: &wgpu::Buffer, size: u64) -> Vec<u8> {
        let staging = buf(
            &self.device,
            size,
            wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        );
        let mut enc = self.device.create_command_encoder(&Default::default());
        enc.copy_buffer_to_buffer(src, 0, &staging, 0, size);
        self.queue.submit([enc.finish()]);
        let (tx, rx) = std::sync::mpsc::channel();
        staging
            .slice(..size)
            .map_async(wgpu::MapMode::Read, move |r| {
                let _ = tx.send(r);
            });
        self.wait();
        let _ = rx.recv();
        let out = staging.slice(..size).get_mapped_range().unwrap().to_vec();
        staging.unmap();
        out
    }

    /// Full JPEG decode -> RGBA u32 buffer on device (0xAABBGGRR words).
    /// Returns the output buffer; caller may keep it (session) or read it.
    /// Handles baseline (SOF0/1) and progressive (SOF2, multi-scan) files.
    /// DRI=0 scans are parallelized by host "survey" passes that
    /// synthesize virtual restart intervals (see jpeg::survey_scan).
    pub fn decode_into(&self, j: &mut Jpg<'_>, fg: &FrameGpu) -> wgpu::Buffer {
        let dev = &self.device;
        let q = &self.queue;
        let n_scans = j.scans.len() as u32;

        // ---- DRI=0 surveys -------------------------------------------------
        // Serial scans above K_SURVEY_IV MCUs get split into virtual
        // intervals by a CPU symbol-walk. AC-refine scans need prior
        // coef state — their survey is deferred to dispatch time.
        let needs_survey = |sc: &Scan| sc.dri == 0 && sc.total_mcus > K_SURVEY_IV;
        let deferred = |sc: &Scan| needs_survey(sc) && scan_mode(sc) == 4;
        let mut scans = std::mem::take(&mut j.scans);
        for sc in scans.iter_mut() {
            if !needs_survey(sc) || deferred(sc) {
                continue;
            }
            if !survey_scan(j, sc, &NoCoef, fg) {
                // fall back to a single serial interval
                sc.surv_st.clear();
                sc.intervals = vec![Interval {
                    byte_start: 0,
                    mcu_start: 0,
                    n_mcu: sc.total_mcus,
                }];
            }
        }

        // ---- one-time upload: scan data, chunk grid, state slots, tables ----
        // Whole file as scan buffer: interval byteStarts are file-absolute.
        let scan_buf = buf(
            dev,
            j.data.len().div_ceil(4) as u64 * 4,
            wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        );
        {
            let mut file_bytes = j.data.to_vec();
            file_bytes.resize(j.data.len().div_ceil(4) * 4, 0);
            q.write_buffer(&scan_buf, 0, &file_bytes);
        }

        // per-scan chunk entries concatenated into one buffer.
        // Surveyed scans emit flag=2 chunks (host-preloaded st[]);
        // deferred-survey scans emit the fixed K_SURVEY_IV partition
        // now — the survey only fills st[] at dispatch time.
        let mut ck_all: Vec<u32> = Vec::new();
        let mut ck_base = vec![0u32; n_scans as usize];
        let mut n_iv_v = Vec::with_capacity(n_scans as usize);
        let mut n_gen_v = Vec::with_capacity(n_scans as usize);
        let mut total_slots = 0u32;
        for (si, sc) in scans.iter().enumerate() {
            ck_base[si] = total_slots;
            if deferred(sc) {
                let n_iv = sc.total_mcus.div_ceil(K_SURVEY_IV);
                n_iv_v.push(n_iv);
                n_gen_v.push(1);
                for i in 0..n_iv {
                    let nm = K_SURVEY_IV.min(sc.total_mcus - i * K_SURVEY_IV);
                    ck_all.extend_from_slice(&[0, i * K_SURVEY_IV, nm, 2]);
                }
                total_slots += n_iv;
                continue;
            }
            let n_iv = sc.intervals.len() as u32;
            let n_gen = sc
                .intervals
                .iter()
                .map(|iv| iv.n_mcu.div_ceil(CHUNK_MCUS))
                .max()
                .unwrap_or(1)
                .max(1);
            n_iv_v.push(n_iv);
            n_gen_v.push(n_gen);
            let surveyed = !sc.surv_st.is_empty();
            for g_i in 0..n_gen {
                for iv in &sc.intervals {
                    let ms = iv.mcu_start + g_i * CHUNK_MCUS;
                    let nm = (iv.mcu_start as i64 + iv.n_mcu as i64 - ms as i64)
                        .clamp(0, CHUNK_MCUS as i64) as u32;
                    ck_all.extend_from_slice(&[
                        if g_i == 0 {
                            sc.data_off + iv.byte_start
                        } else {
                            0
                        },
                        ms,
                        nm,
                        if surveyed {
                            2
                        } else {
                            u32::from(g_i > 0)
                        },
                    ]);
                }
            }
            total_slots += n_iv * n_gen;
        }
        let iv_buf = buf(
            dev,
            (ck_all.len() * 4) as u64,
            wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        );
        q.write_buffer(&iv_buf, 0, &u32_bytes(&ck_all));
        let st_buf = buf(
            dev,
            (total_slots * 12 * 4) as u64,
            wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        );
        // preload surveyed states into each interval's own slot
        {
            let mut st_words = vec![0u32; total_slots as usize * 12];
            for (si, sc) in scans.iter().enumerate() {
                if sc.surv_st.is_empty() {
                    continue;
                }
                let base = ck_base[si] as usize * 12;
                st_words[base..base + sc.surv_st.len()].copy_from_slice(&sc.surv_st);
            }
            q.write_buffer(&st_buf, 0, &u32_bytes(&st_words));
        }

        // per-scan Huffman/quant tables at region si*TAB_WORDS, plus one
        // recon region (final quant state) at n_scans*TAB_WORDS
        let tab_buf = buf(
            dev,
            ((n_scans as usize + 1) * super::jpeg::TAB_WORDS * 4) as u64,
            wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        );
        {
            let mut words = Vec::with_capacity((n_scans as usize + 1) * super::jpeg::TAB_WORDS);
            for sc in &scans {
                words.extend_from_slice(&pack_tables(sc));
            }
            let mut qt = vec![0i32; super::jpeg::TAB_WORDS];
            for t in 0..4 {
                for i in 0..64 {
                    qt[t * 64 + i] = j.qt[t][i] as i32;
                }
            }
            words.extend_from_slice(&qt);
            q.write_buffer(&tab_buf, 0, &i32_bytes(&words));
        }

        // per-scan fast LUTs at region si*FLUT_WORDS (dedup table slots
        // identical to the previous scan's — cjpeg re-emits some tables)
        let flut_buf = buf(
            dev,
            (n_scans as usize * super::jpeg::FLUT_WORDS * 4) as u64,
            wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        );
        {
            let mut all = Vec::with_capacity(n_scans as usize * super::jpeg::FLUT_WORDS);
            let mut prev: Option<(&super::jpeg::Scan, Vec<u32>)> = None;
            for sc in &scans {
                let fl = pack_flut(sc, prev.as_ref().map(|(s, f)| (*s, f.as_slice())));
                all.extend_from_slice(&fl);
                prev = Some((sc, fl));
            }
            q.write_buffer(&flut_buf, 0, &u32_bytes(&all));
        }

        let comp_buf = buf(
            dev,
            256,
            wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        );
        q.write_buffer(&comp_buf, 0, &i32_bytes(&fg.comps.concat()));
        let coef_buf = buf(
            dev,
            (fg.total_blocks as u64) * 32 * 4,
            wgpu::BufferUsages::STORAGE
                | wgpu::BufferUsages::COPY_DST
                | wgpu::BufferUsages::COPY_SRC, // deferred surveys read it
        );
        let plane_buf = buf(
            dev,
            (fg.plane_words as u64) * 4,
            wgpu::BufferUsages::STORAGE,
        );
        let (ow, oh) = j.out_dims();
        let out_buf = buf(
            dev,
            (ow as u64) * (oh as u64) * 4,
            wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
        );

        let bg = dev.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &self.d_bgl,
            entries: &[
                bg_entry(0, &scan_buf),
                bg_entry(1, &iv_buf),
                bg_entry(2, &tab_buf),
                bg_entry(3, &st_buf),
                bg_entry(4, &comp_buf),
                bg_entry(5, &coef_buf),
                bg_entry(6, &plane_buf),
                bg_entry(7, &out_buf),
                bg_entry(8, &flut_buf),
            ],
        });

        // ---- dispatch grouping: a single multi-second submit can get
        // killed by the driver's job watchdog on some GPUs — group
        // (scan, generation) dispatches into submits bounded by MCU work.
        struct Job {
            si: u32,
            g: u32,
            mcus: u32,
        }
        let mut jobs = Vec::new();
        for si in 0..n_scans {
            let sc = &scans[si as usize];
            for g in 0..n_gen_v[si as usize] {
                let mut w = 0u64;
                for iv in &sc.intervals {
                    let ms = iv.mcu_start + g * CHUNK_MCUS;
                    w += (iv.mcu_start as i64 + iv.n_mcu as i64 - ms as i64)
                        .clamp(0, CHUNK_MCUS as i64) as u64;
                }
                jobs.push(Job {
                    si,
                    g,
                    mcus: w as u32,
                });
            }
        }
        // greedy grouping into ranges whose total MCUs stay under budget;
        // a deferred survey begins a new submit — its coef readback needs
        // all prior scans' dispatches complete first
        let mut submits: Vec<(u32, u32)> = Vec::new();
        {
            let mut acc = 0u32;
            let mut start = 0usize;
            for i in 0..=jobs.len() {
                if i == jobs.len()
                    || (acc > 0
                        && (acc + jobs[i].mcus > SUBMIT_MCUS
                            || deferred(&scans[jobs[i].si as usize])))
                {
                    submits.push((start as u32, i as u32));
                    start = i;
                    acc = 0;
                    if i == jobs.len() {
                        break;
                    }
                }
                acc += jobs[i].mcus;
            }
        }

        let mut pc = [0u32; 22];
        pc[0] = j.w as u32;
        pc[1] = j.h as u32;
        pc[2] = j.mcus_x as u32;
        pc[3] = j.n_comp as u32;
        pc[4] = j.max_h as u32;
        pc[5] = j.max_v as u32;
        pc[7] = j.data.len() as u32;

        // 1) entropy decode: scan generations grouped by MCU budget
        let coef_len = (fg.total_blocks as u64) * 32 * 4;
        let mut first = true;
        for &(j0, j1) in &submits {
            // AC-refine surveys run between submits: the host reads the
            // coef buffer only after all prior scans' dispatches land.
            {
                let si = jobs[j0 as usize].si as usize;
                if deferred(&scans[si]) && scans[si].surv_st.is_empty() {
                    let mut sc0 = std::mem::take(&mut scans[si]);
                    let ok = if first {
                        // coef buffer not yet filled — survey on zeros
                        survey_scan(j, &mut sc0, &NoCoef, fg)
                    } else {
                        self.wait();
                        let bytes = self.read_buf(&coef_buf, coef_len);
                        let words: Vec<u32> = bytes
                            .chunks_exact(4)
                            .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                            .collect();
                        survey_scan(j, &mut sc0, &&*words.as_slice(), fg)
                    };
                    if ok {
                        q.write_buffer(
                            &st_buf,
                            (ck_base[si] * 12 * 4) as u64,
                            &u32_bytes(&sc0.surv_st),
                        );
                    } else {
                        // Degrade to a single serial chunk: entry 0
                        // covers the whole scan from its data offset
                        // (flag 0 = fresh state); the rest of this
                        // scan's slots get n_mcu=0 and are skipped.
                        eprintln!(
                            "gpujpeg: deferred survey failed (scan {si}) — serial fallback"
                        );
                        let n_iv = n_iv_v[si] as usize;
                        let mut ent = vec![0u32; n_iv * 4];
                        ent[0] = sc0.data_off;
                        ent[2] = sc0.total_mcus;
                        q.write_buffer(
                            &iv_buf,
                            (ck_base[si] * 16) as u64,
                            &u32_bytes(&ent),
                        );
                    }
                    scans[si] = sc0;
                }
            }
            let mut enc = dev.create_command_encoder(&Default::default());
            if first {
                enc.clear_buffer(&coef_buf, 0, None);
                first = false;
            }
            {
                let mut cp = enc.begin_compute_pass(&Default::default());
                cp.set_pipeline(&self.p_entropy);
                cp.set_bind_group(0, &bg, &[]);
                let mut last_si = u32::MAX;
                for ji in j0..j1 {
                    let jb = &jobs[ji as usize];
                    let si = jb.si;
                    let sc = &scans[si as usize];
                    let n_iv = n_iv_v[si as usize];
                    if si != last_si {
                        pc[6] = n_iv;
                        pc[9] = scan_mode(sc);
                        pc[10] = sc.ns as u32;
                        pc[11] = sc.ss as u32;
                        pc[12] = sc.se as u32;
                        pc[13] = sc.ah as u32;
                        pc[14] = sc.al as u32;
                        pc[15] = sc.total_mcus;
                        for i in 0..sc.ns as usize {
                            pc[16 + i] = sc.sc[i].comp as u32
                                | ((sc.sc[i].td as u32) << 8)
                                | ((sc.sc[i].ta as u32) << 16);
                        }
                        pc[20] = si * (super::jpeg::TAB_WORDS as u32) / 4;
                        pc[21] = si * (super::jpeg::FLUT_WORDS as u32);
                        last_si = si;
                    }
                    pc[8] = ck_base[si as usize] + jb.g * n_iv;
                    cp.set_immediates(0, &pc_bytes(&pc));
                    cp.dispatch_workgroups(n_iv.div_ceil(32), 1, 1);
                }
            }
            q.submit([enc.finish()]);
        }
        self.wait();
        // 2) + 3) reconstruction and upsample in one submit. Upsample
        // applies EXIF orientation: pc.w/h are the ORIENTED output dims
        // (swapped for orient 5-8); uW/uH stay the source plane dims.
        let pc8: [u32; 12] = [
            ow,
            oh,
            j.mcus_x as u32,
            j.n_comp as u32,
            j.max_h as u32,
            j.max_v as u32,
            0,
            j.data.len() as u32,
            n_scans * (super::jpeg::TAB_WORDS as u32) / 4,
            j.orient as u32,
            j.w as u32,
            j.h as u32,
        ];
        let mut enc = dev.create_command_encoder(&Default::default());
        {
            let mut cp = enc.begin_compute_pass(&Default::default());
            cp.set_pipeline(&self.p_recon);
            cp.set_bind_group(0, &bg, &[]);
            cp.set_immediates(0, &pc_bytes(&pc8));
            cp.dispatch_workgroups((fg.total_blocks as u32).div_ceil(64), 1, 1);
        }
        {
            let mut cp = enc.begin_compute_pass(&Default::default());
            cp.set_pipeline(&self.p_upsample);
            cp.set_bind_group(0, &bg, &[]);
            cp.set_immediates(0, &pc_bytes(&pc8));
            cp.dispatch_workgroups(ow.div_ceil(16), oh.div_ceil(16), 1);
        }
        q.submit([enc.finish()]);
        self.wait();
        j.scans = scans;
        out_buf
    }

    /// Resize a GPU-resident RGBA word buffer and read back packed
    /// n_comp-byte pixels. Only the small result leaves the device.
    pub fn resize_pack(
        &self,
        px: &wgpu::Buffer,
        in_w: u32,
        in_h: u32,
        out_w: u32,
        out_h: u32,
        n_comp: u32,
        filter: i32,
    ) -> Option<Vec<u8>> {
        let dev = &self.device;
        let out_bytes = u64::from(out_w) * u64::from(out_h) * u64::from(n_comp);
        let pack_size = (out_bytes + 3) & !3;
        let pack_buf = buf(
            dev,
            pack_size,
            wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
        );
        let mut enc = dev.create_command_encoder(&Default::default());
        let dispatch_pack = |enc: &mut wgpu::CommandEncoder,
                             src: &wgpu::Buffer,
                             bndx: &wgpu::Buffer,
                             cfx: &wgpu::Buffer| {
            let bg = dev.create_bind_group(&wgpu::BindGroupDescriptor {
                label: None,
                layout: &self.r_bgl,
                entries: &[
                    bg_entry(0, src),
                    bg_entry(1, bndx),
                    bg_entry(2, cfx),
                    bg_entry(3, &pack_buf),
                ],
            });
            let mut cp = enc.begin_compute_pass(&Default::default());
            cp.set_pipeline(&self.p_pack);
            cp.set_bind_group(0, &bg, &[]);
            cp.set_immediates(0, &pc_bytes(&[out_w * out_h, n_comp]));
            let wgs = (pack_size / 4).div_ceil(256) as u32;
            cp.dispatch_workgroups(wgs.min(65535), wgs.div_ceil(65535), 1);
        };

        if out_w == in_w && out_h == in_h {
            dispatch_pack(&mut enc, px, px, px);
        } else {
            let ax = coeffs::axis_coeffs(in_w as i32, out_w as i32, filter);
            let ay = coeffs::axis_coeffs(in_h as i32, out_h as i32, filter);
            let bnd_buf = |b: &[i32]| {
                let bb = buf(
                    dev,
                    (b.len() * 4) as u64,
                    wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                );
                self.queue.write_buffer(&bb, 0, &i32_bytes(b));
                bb
            };
            let bnd_x = bnd_buf(&ax.bounds);
            let cf_x = bnd_buf(&ax.coefs);
            let bnd_y = bnd_buf(&ay.bounds);
            let cf_y = bnd_buf(&ay.coefs);
            let tmp_buf = buf(
                dev,
                u64::from(out_w) * u64::from(in_h) * 4,
                wgpu::BufferUsages::STORAGE,
            );
            let out_rgba = buf(
                dev,
                u64::from(out_w) * u64::from(out_h) * 4,
                wgpu::BufferUsages::STORAGE,
            );
            let resample_pass = |enc: &mut wgpu::CommandEncoder,
                                 inp: &wgpu::Buffer,
                                 bnd: &wgpu::Buffer,
                                 cf: &wgpu::Buffer,
                                 outp: &wgpu::Buffer,
                                 pc: [u32; 6],
                                 dx: u32,
                                 dy: u32| {
                let bg = dev.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: None,
                    layout: &self.r_bgl,
                    entries: &[
                        bg_entry(0, inp),
                        bg_entry(1, bnd),
                        bg_entry(2, cf),
                        bg_entry(3, outp),
                    ],
                });
                let mut cp = enc.begin_compute_pass(&Default::default());
                cp.set_pipeline(&self.p_resample);
                cp.set_bind_group(0, &bg, &[]);
                cp.set_immediates(0, &pc_bytes(&pc));
                cp.dispatch_workgroups(dx.div_ceil(8), dy.div_ceil(8), 1);
            };
            resample_pass(
                &mut enc,
                px,
                &bnd_x,
                &cf_x,
                &tmp_buf,
                [in_w, in_h, out_w, in_h, ax.ksize as u32, 0],
                out_w,
                in_h,
            );
            resample_pass(
                &mut enc,
                &tmp_buf,
                &bnd_y,
                &cf_y,
                &out_rgba,
                [out_w, in_h, out_w, out_h, ay.ksize as u32, 1],
                out_w,
                out_h,
            );
            dispatch_pack(&mut enc, &out_rgba, &bnd_x, &cf_x);
        }
        // readback rides the same submit
        let staging = buf(
            dev,
            pack_size,
            wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        );
        enc.copy_buffer_to_buffer(&pack_buf, 0, &staging, 0, pack_size);
        self.queue.submit([enc.finish()]);
        let (tx, rx) = std::sync::mpsc::channel();
        staging
            .slice(..pack_size)
            .map_async(wgpu::MapMode::Read, move |r| {
                let _ = tx.send(r);
            });
        self.wait();
        let _ = rx.recv();
        let mut out = staging
            .slice(..pack_size)
            .get_mapped_range()
            .unwrap()
            .to_vec();
        staging.unmap();
        out.truncate(out_bytes as usize);
        Some(out)
    }
}

static GPU: OnceLock<Option<GpuCtx>> = OnceLock::new();

pub fn ctx() -> Option<&'static GpuCtx> {
    GPU.get_or_init(GpuCtx::init).as_ref()
}
