//! wgpu plumbing — replaces the C++ vkmini + shaderc + Vulkan path.
//! Device/queue/pipelines are initialized lazily once; buffer lifetimes
//! are managed by wgpu handles (no explicit session map needed).

use super::coeffs;
use super::jpeg::{pack_flut, pack_tables, FrameGpu, Jpg};
use super::shaders;
use std::future::Future;
use std::sync::OnceLock;

const CHUNK_MCUS: u32 = 2048;

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
                bgl_entry(2, UNI),
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
            immediate_size: 36,
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
    pub fn decode_into(&self, j: &Jpg<'_>, fg: &FrameGpu) -> wgpu::Buffer {
        let dev = &self.device;
        let q = &self.queue;
        let n_iv = j.intervals.len() as u32;
        let n_gen = j
            .intervals
            .iter()
            .map(|iv| iv.n_mcu.div_ceil(CHUNK_MCUS))
            .max()
            .unwrap_or(1)
            .max(1);
        let mut ck = vec![0u32; (n_iv * n_gen * 4) as usize];
        for g_i in 0..n_gen {
            for i in 0..n_iv {
                let iv = &j.intervals[i as usize];
                let ms = iv.mcu_start + g_i * CHUNK_MCUS;
                let nm = (iv.mcu_start as i64 + iv.n_mcu as i64 - ms as i64)
                    .clamp(0, CHUNK_MCUS as i64) as u32;
                let w = &mut ck[((g_i * n_iv + i) * 4) as usize..][..4];
                w[0] = if g_i == 0 { iv.byte_start } else { 0 };
                w[1] = ms;
                w[2] = nm;
                w[3] = if g_i > 0 { 1 } else { 0 };
            }
        }

        let scan_buf = buf(
            dev,
            u64::from(j.scan_len.div_ceil(4) * 4),
            wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        );
        {
            let mut scan_bytes =
                j.data[j.scan_off as usize..(j.scan_off + j.scan_len) as usize].to_vec();
            scan_bytes.resize(j.scan_len.div_ceil(4) as usize * 4, 0);
            q.write_buffer(&scan_buf, 0, &scan_bytes);
        }
        let iv_buf = buf(
            dev,
            (n_iv * n_gen * 16) as u64,
            wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        );
        q.write_buffer(&iv_buf, 0, &u32_bytes(&ck));
        let st_buf = buf(dev, (n_iv * n_gen * 32) as u64, wgpu::BufferUsages::STORAGE);
        let tab_buf = buf(
            dev,
            (super::jpeg::TAB_WORDS * 4) as u64,
            wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        );
        q.write_buffer(&tab_buf, 0, &i32_bytes(&pack_tables(j)));
        let flut_buf = buf(
            dev,
            (super::jpeg::FLUT_WORDS * 4) as u64,
            wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        );
        q.write_buffer(&flut_buf, 0, &u32_bytes(&pack_flut(j)));
        let comp_buf = buf(
            dev,
            256,
            wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        );
        q.write_buffer(&comp_buf, 0, &i32_bytes(&fg.comps.concat()));
        let coef_buf = buf(
            dev,
            (fg.total_blocks as u64) * 32 * 4,
            wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        );
        let plane_buf = buf(
            dev,
            (fg.plane_words as u64) * 4,
            wgpu::BufferUsages::STORAGE,
        );
        let out_buf = buf(
            dev,
            (j.w as u64) * (j.h as u64) * 4,
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

        let mut pc = [
            j.w as u32,
            j.h as u32,
            j.mcus_x as u32,
            j.n_comp as u32,
            j.max_h as u32,
            j.max_v as u32,
            n_iv,
            j.scan_len,
            0,
        ];
        // 1) entropy decode: one submit per chunk generation
        for g_i in 0..n_gen {
            pc[8] = g_i * n_iv;
            let mut enc = dev.create_command_encoder(&Default::default());
            if g_i == 0 {
                enc.clear_buffer(&coef_buf, 0, None);
            }
            {
                let mut cp = enc.begin_compute_pass(&Default::default());
                cp.set_pipeline(&self.p_entropy);
                cp.set_bind_group(0, &bg, &[]);
                cp.set_immediates(0, &pc_bytes(&pc));
                cp.dispatch_workgroups(n_iv.div_ceil(32), 1, 1);
            }
            q.submit([enc.finish()]);
            self.wait();
        }
        // 2) + 3) reconstruction and upsample in one submit
        let mut enc = dev.create_command_encoder(&Default::default());
        {
            let mut cp = enc.begin_compute_pass(&Default::default());
            cp.set_pipeline(&self.p_recon);
            cp.set_bind_group(0, &bg, &[]);
            cp.set_immediates(0, &pc_bytes(&pc[..8]));
            cp.dispatch_workgroups((fg.total_blocks as u32).div_ceil(64), 1, 1);
        }
        {
            let mut cp = enc.begin_compute_pass(&Default::default());
            cp.set_pipeline(&self.p_upsample);
            cp.set_bind_group(0, &bg, &[]);
            cp.set_immediates(0, &pc_bytes(&pc[..8]));
            cp.dispatch_workgroups((j.w as u32).div_ceil(16), (j.h as u32).div_ceil(16), 1);
        }
        q.submit([enc.finish()]);
        self.wait();
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
