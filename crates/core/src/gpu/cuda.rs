//! Port of `server/gpu/cuda.ts` (Use GPU, 0.13.0): the NVIDIA driver's CUDA interface (nvcuda.dll, part of every
//! NVIDIA graphics driver; libcuda.so.1 on Linux), loaded when Use GPU is on (libloading, as 0.x used koffi). Nothing
//! else to install: the kernel ships as PTX (similarities.ptx, from similarities.cu, 0.22.0's own) and the driver
//! compiles it for the card. One Gpu for the evaluation, behind a lock; each call makes the card's context current on
//! the calling thread.
//!
//! Build Fetcher adds `logits` (logits.ptx, hand-written): the logistic model's forward pass for training and
//! evaluation, off unless the user turns the GPU on, checked against the processor by crates/learn.

use std::ffi::{c_char, c_int, c_uint, c_void, CStr, CString};

/// The most context dimensions the kernel takes (similarities.cu MAX_D).
pub const MAX_D: usize = 64;
/// Marked games the kernel lists (similarities.cu LIST).
const LIST: usize = 256;
const BLOCK: u32 = 256;
/// similarities.cu Params: three pointers, den, n, d, then q.
const Q_AT: usize = 40;

pub const SIMILARITIES_PTX: &str = include_str!("similarities.ptx");
/// The logistic model's forward pass (`logits_csr`) and the weight update (`scatter`), hand-written PTX.
pub const LOGITS_PTX: &str = include_str!("logits.ptx");

fn error_text(code: c_int) -> &'static str {
    match code {
        1 => "invalid value",
        2 => "out of GPU memory",
        3 => "driver not initialised",
        34 => "the CUDA stub library, not the driver",
        35 => "the NVIDIA driver is older than this app needs (CUDA 12.0)",
        46 => "the GPU is busy or unavailable",
        100 => "no NVIDIA GPU found",
        101 => "invalid device",
        200 => "invalid kernel image",
        201 => "invalid context",
        209 => "no kernel for this GPU",
        218 => "invalid PTX",
        222 => "the NVIDIA driver is too old for the kernel (CUDA 12.0 needed): update the driver",
        400 => "invalid handle",
        500 => "not found",
        700 => "illegal memory access",
        702 => "the GPU timed out",
        719 => "the kernel failed",
        999 => "unknown error",
        _ => "CUDA error",
    }
}

#[derive(Debug)]
pub struct CudaError(pub String);

impl std::fmt::Display for CudaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CudaError {}

type R<T> = Result<T, CudaError>;

fn check(code: c_int, what: &str) -> R<()> {
    if code == 0 {
        Ok(())
    } else {
        Err(CudaError(format!("{what}: {} ({code})", error_text(code))))
    }
}

type Ptr = *mut c_void;

#[allow(non_snake_case)]
struct Driver {
    _lib: libloading::Library,
    cuInit: unsafe extern "system" fn(c_uint) -> c_int,
    cuDriverGetVersion: unsafe extern "system" fn(*mut c_int) -> c_int,
    cuDeviceGetCount: unsafe extern "system" fn(*mut c_int) -> c_int,
    cuDeviceGet: unsafe extern "system" fn(*mut c_int, c_int) -> c_int,
    cuDeviceGetName: unsafe extern "system" fn(*mut c_char, c_int, c_int) -> c_int,
    cuDeviceGetAttribute: unsafe extern "system" fn(*mut c_int, c_int, c_int) -> c_int,
    cuDeviceTotalMem_v2: unsafe extern "system" fn(*mut usize, c_int) -> c_int,
    cuMemGetInfo_v2: unsafe extern "system" fn(*mut usize, *mut usize) -> c_int,
    cuDevicePrimaryCtxRetain: unsafe extern "system" fn(*mut Ptr, c_int) -> c_int,
    cuDevicePrimaryCtxRelease_v2: unsafe extern "system" fn(c_int) -> c_int,
    cuDevicePrimaryCtxReset_v2: unsafe extern "system" fn(c_int) -> c_int,
    cuCtxSetCurrent: unsafe extern "system" fn(Ptr) -> c_int,
    cuModuleLoadData: unsafe extern "system" fn(*mut Ptr, *const c_void) -> c_int,
    cuModuleUnload: unsafe extern "system" fn(Ptr) -> c_int,
    cuModuleGetFunction: unsafe extern "system" fn(*mut Ptr, Ptr, *const c_char) -> c_int,
    cuModuleGetGlobal_v2: unsafe extern "system" fn(*mut u64, *mut usize, Ptr, *const c_char) -> c_int,
    cuMemAlloc_v2: unsafe extern "system" fn(*mut u64, usize) -> c_int,
    cuMemFree_v2: unsafe extern "system" fn(u64) -> c_int,
    cuMemcpyHtoD_v2: unsafe extern "system" fn(u64, *const c_void, usize) -> c_int,
    cuMemcpyDtoH_v2: unsafe extern "system" fn(*mut c_void, u64, usize) -> c_int,
    cuMemsetD32_v2: unsafe extern "system" fn(u64, c_uint, usize) -> c_int,
    #[allow(clippy::type_complexity)]
    cuLaunchKernel: unsafe extern "system" fn(Ptr, c_uint, c_uint, c_uint, c_uint, c_uint, c_uint, c_uint, Ptr, *mut Ptr, *mut Ptr) -> c_int,
}

/// The driver library: BF_CUDA_LIB overrides it (a stand-in for tests).
fn driver_library() -> String {
    std::env::var("BF_CUDA_LIB").unwrap_or_else(|_| if cfg!(windows) { "nvcuda.dll".into() } else { "libcuda.so.1".into() })
}

fn bind() -> R<Driver> {
    let path = driver_library();
    let lib = unsafe { libloading::Library::new(&path) }
        .map_err(|_| CudaError(format!("No NVIDIA driver: {path} could not be loaded (it comes with the NVIDIA graphics driver)")))?;
    macro_rules! f {
        ($name:ident) => {{
            let sym = unsafe { lib.get::<*const c_void>(concat!(stringify!($name), "\0").as_bytes()) }.map_err(|_| {
                CudaError(format!("The NVIDIA driver is older than this app needs (CUDA 12.0): it has no {}. Update the driver", stringify!($name)))
            })?;
            unsafe { std::mem::transmute::<*const c_void, _>(*sym) }
        }};
    }
    Ok(Driver {
        cuInit: f!(cuInit),
        cuDriverGetVersion: f!(cuDriverGetVersion),
        cuDeviceGetCount: f!(cuDeviceGetCount),
        cuDeviceGet: f!(cuDeviceGet),
        cuDeviceGetName: f!(cuDeviceGetName),
        cuDeviceGetAttribute: f!(cuDeviceGetAttribute),
        cuDeviceTotalMem_v2: f!(cuDeviceTotalMem_v2),
        cuMemGetInfo_v2: f!(cuMemGetInfo_v2),
        cuDevicePrimaryCtxRetain: f!(cuDevicePrimaryCtxRetain),
        cuDevicePrimaryCtxRelease_v2: f!(cuDevicePrimaryCtxRelease_v2),
        cuDevicePrimaryCtxReset_v2: f!(cuDevicePrimaryCtxReset_v2),
        cuCtxSetCurrent: f!(cuCtxSetCurrent),
        cuModuleLoadData: f!(cuModuleLoadData),
        cuModuleUnload: f!(cuModuleUnload),
        cuModuleGetFunction: f!(cuModuleGetFunction),
        cuModuleGetGlobal_v2: f!(cuModuleGetGlobal_v2),
        cuMemAlloc_v2: f!(cuMemAlloc_v2),
        cuMemFree_v2: f!(cuMemFree_v2),
        cuMemcpyHtoD_v2: f!(cuMemcpyHtoD_v2),
        cuMemcpyDtoH_v2: f!(cuMemcpyDtoH_v2),
        cuMemsetD32_v2: f!(cuMemsetD32_v2),
        cuLaunchKernel: f!(cuLaunchKernel),
        _lib: lib,
    })
}

#[derive(Clone, Debug)]
pub struct GpuDevice {
    pub name: String,
    pub capability: String,
    pub memory_mb: f64,
    pub free_mb: f64,
    pub driver: String,
}

/// A model's draft contexts on the card (`id`: Contexts::id).
struct Upload {
    id: u64,
    ptr: u64,
    bytes: usize,
}

/// The most copies of the games kept at once.
const KEEP: usize = 8;

pub struct Gpu {
    cu: Driver,
    dev: c_int,
    ctx: Ptr,
    module: Ptr,
    func: Ptr,
    params_ptr: u64,
    marks_ptr: u64,
    out_ptr: u64,
    out_bytes: usize,
    /// Oldest first.
    uploads: Vec<Upload>,
    uploaded: usize,
    /// The card memory the uploads may take.
    budget: usize,
    params: Vec<u8>,
    marks: Vec<u32>,
    closed: bool,
    /// The forward-pass kernels, loaded the first time `logits` is called.
    lk: Option<LogitsKernel>,
    pub device: GpuDevice,
}

/// A buffer on the card, grown as needed (never shrunk until closed).
#[derive(Default)]
struct DevBuf {
    ptr: u64,
    bytes: usize,
}

/// The forward-pass module, the weights kept on the card, and a copy of what was sent (to send only what changed).
struct LogitsKernel {
    module: Ptr,
    f_logits: Ptr,
    f_scatter: Ptr,
    w: DevBuf,
    shadow: Vec<f64>,
    ptr: DevBuf,
    idx: DevBuf,
    val: DevBuf,
    out: DevBuf,
    s_idx: DevBuf,
    s_val: DevBuf,
}

// The driver's handles are used under the Gpu's lock, with the context made current on the calling thread.
unsafe impl Send for Gpu {}

impl Gpu {
    /// The first NVIDIA GPU, ready to run the kernel; an error with the reason it can't (no driver, no GPU, too old).
    pub fn open() -> R<Gpu> {
        let cu = bind()?;
        unsafe {
            check((cu.cuInit)(0), "Starting CUDA")?;
            let (mut version, mut count, mut dev) = (0, 0, 0);
            check((cu.cuDriverGetVersion)(&mut version), "Driver version")?;
            check((cu.cuDeviceGetCount)(&mut count), "Counting GPUs")?;
            if count < 1 {
                return Err(CudaError("No NVIDIA GPU found".into()));
            }
            check((cu.cuDeviceGet)(&mut dev, 0), "Opening the GPU")?;
            let mut name_buf = [0 as c_char; 256];
            check((cu.cuDeviceGetName)(name_buf.as_mut_ptr(), 256, dev), "GPU name")?;
            let name = CStr::from_ptr(name_buf.as_ptr()).to_string_lossy().into_owned();
            let (mut major, mut minor, mut total) = (0, 0, 0usize);
            check((cu.cuDeviceGetAttribute)(&mut major, 75, dev), "Compute capability")?;
            check((cu.cuDeviceGetAttribute)(&mut minor, 76, dev), "Compute capability")?;
            check((cu.cuDeviceTotalMem_v2)(&mut total, dev), "GPU memory")?;
            if major * 10 + minor < 52 {
                return Err(CudaError(format!("{name} is too old (compute capability {major}.{minor}; 5.2 or later needed)")));
            }
            let mut ctx: Ptr = std::ptr::null_mut();
            check((cu.cuDevicePrimaryCtxRetain)(&mut ctx, dev), "Opening the GPU context")?;
            let mut module: Ptr = std::ptr::null_mut();
            let r = (|| -> R<(Ptr, u64, u64, usize)> {
                check((cu.cuCtxSetCurrent)(ctx), "Opening the GPU context")?;
                let image = CString::new(SIMILARITIES_PTX).unwrap();
                check((cu.cuModuleLoadData)(&mut module, image.as_ptr() as *const c_void), "Loading the kernel")?;
                let mut func: Ptr = std::ptr::null_mut();
                check((cu.cuModuleGetFunction)(&mut func, module, c"similarities".as_ptr()), "Loading the kernel")?;
                let (mut params, mut size) = (0u64, 0usize);
                check((cu.cuModuleGetGlobal_v2)(&mut params, &mut size, module, c"params".as_ptr()), "Loading the kernel")?;
                let mut marks = 0u64;
                check((cu.cuMemAlloc_v2)(&mut marks, 4 * (1 + LIST)), "GPU memory")?;
                let (mut free, mut tot) = (0usize, 0usize);
                check((cu.cuMemGetInfo_v2)(&mut free, &mut tot), "GPU memory")?;
                Ok((func, params, marks, free))
            })();
            let (func, params_ptr, marks_ptr, free) = match r {
                Ok(x) => x,
                Err(e) => {
                    if !module.is_null() {
                        (cu.cuModuleUnload)(module);
                    }
                    (cu.cuDevicePrimaryCtxRelease_v2)(dev);
                    return Err(e);
                }
            };
            let device = GpuDevice {
                name,
                capability: format!("{major}.{minor}"),
                memory_mb: (total as f64 / 1_048_576.0).round(),
                free_mb: (free as f64 / 1_048_576.0).round(),
                driver: format!("CUDA {}.{}", version / 1000, crate::js::num_to_string((version % 1000) as f64 / 10.0)),
            };
            Ok(Gpu {
                cu,
                dev,
                ctx,
                module,
                func,
                params_ptr,
                marks_ptr,
                out_ptr: 0,
                out_bytes: 0,
                uploads: vec![],
                uploaded: 0,
                budget: (1usize << 30).min(free / 4),
                params: vec![0u8; Q_AT + 8 * MAX_D],
                marks: vec![0u32; 1 + LIST],
                closed: false,
                lk: None,
                device,
            })
        }
    }

    /// Frees everything this process put on the first NVIDIA GPU (its context reset): an evaluation stopped before it
    /// could. Nothing without an NVIDIA driver.
    pub fn reset() -> R<bool> {
        let Ok(cu) = bind() else { return Ok(false) };
        unsafe {
            let mut dev = 0;
            if (cu.cuInit)(0) != 0 || (cu.cuDeviceGet)(&mut dev, 0) != 0 {
                return Ok(false);
            }
            check((cu.cuDevicePrimaryCtxReset_v2)(dev), "Freeing the GPU")?;
        }
        Ok(true)
    }

    fn upload(&mut self, id: u64, flat: &[f64]) -> R<u64> {
        if id != 0 {
            if let Some(i) = self.uploads.iter().position(|u| u.id == id) {
                let u = self.uploads.remove(i);
                let ptr = u.ptr;
                self.uploads.push(u);
                return Ok(ptr);
            }
        }
        let bytes = std::mem::size_of_val(flat);
        if bytes > self.budget {
            return Err(CudaError("GPU memory: the games do not fit the share the evaluation may take".into()));
        }
        while self.uploads.len() >= KEEP || (!self.uploads.is_empty() && self.uploaded + bytes > self.budget) {
            self.drop_oldest();
        }
        let mut ptr = 0u64;
        unsafe {
            check((self.cu.cuMemAlloc_v2)(&mut ptr, bytes), "GPU memory for the games")?;
            if let Err(e) = check((self.cu.cuMemcpyHtoD_v2)(ptr, flat.as_ptr() as *const c_void, bytes), "Copying the games") {
                (self.cu.cuMemFree_v2)(ptr);
                return Err(e);
            }
        }
        if id == 0 {
            // No identity to find it by again: freed after this pass (see similarities).
            self.uploads.push(Upload { id: 0, ptr, bytes });
        } else {
            self.uploads.push(Upload { id, ptr, bytes });
        }
        self.uploaded += bytes;
        Ok(ptr)
    }

    fn drop_oldest(&mut self) {
        let u = self.uploads.remove(0);
        self.uploaded -= u.bytes;
        unsafe {
            (self.cu.cuMemFree_v2)(u.ptr);
        }
    }

    /// Each of `rows` games' similarity (the rows of `flat`, `d` wide) to the draft `q`, into `out`. Returns the games
    /// the GPU left to the processor (exp too close to call, or out of range), whose `out` is -1 meanwhile.
    #[allow(clippy::too_many_arguments)]
    pub fn similarities(&mut self, id: u64, flat: &[f64], rows: usize, d: usize, q: &[f64], den: f64, out: &mut [f64]) -> R<Vec<usize>> {
        if self.closed {
            return Err(CudaError("The GPU was closed".into()));
        }
        unsafe {
            check((self.cu.cuCtxSetCurrent)(self.ctx), "Opening the GPU context")?;
        }
        let flat_ptr = self.upload(id, &flat[..rows * d])?;
        let r = self.run(flat_ptr, rows, d, q, den, out);
        if id == 0 {
            if let Some(i) = self.uploads.iter().position(|u| u.ptr == flat_ptr) {
                let u = self.uploads.remove(i);
                self.uploaded -= u.bytes;
                unsafe {
                    (self.cu.cuMemFree_v2)(u.ptr);
                }
            }
        }
        r
    }

    fn run(&mut self, flat_ptr: u64, rows: usize, d: usize, q: &[f64], den: f64, out: &mut [f64]) -> R<Vec<usize>> {
        let cu = &self.cu;
        unsafe {
            if self.out_bytes < rows * 8 {
                if self.out_bytes > 0 {
                    check((cu.cuMemFree_v2)(self.out_ptr), "GPU memory")?;
                }
                self.out_bytes = 0;
                let size = (rows as f64 * 1.25).ceil() as usize * 8;
                let mut ptr = 0u64;
                check((cu.cuMemAlloc_v2)(&mut ptr, size), "GPU memory")?;
                self.out_ptr = ptr;
                self.out_bytes = size;
            }
            let p = &mut self.params;
            p[0..8].copy_from_slice(&flat_ptr.to_le_bytes());
            p[8..16].copy_from_slice(&self.out_ptr.to_le_bytes());
            p[16..24].copy_from_slice(&self.marks_ptr.to_le_bytes());
            p[24..32].copy_from_slice(&den.to_le_bytes());
            p[32..36].copy_from_slice(&(rows as i32).to_le_bytes());
            p[36..40].copy_from_slice(&(d as i32).to_le_bytes());
            for (c, x) in q.iter().enumerate().take(d) {
                p[Q_AT + 8 * c..Q_AT + 8 * c + 8].copy_from_slice(&x.to_le_bytes());
            }
            // All on the context's own stream, in this order: each step waits for the one before.
            check((cu.cuMemcpyHtoD_v2)(self.params_ptr, p.as_ptr() as *const c_void, Q_AT + 8 * d), "Copying the draft")?;
            check((cu.cuMemsetD32_v2)(self.marks_ptr, 0, 1), "Copying the draft")?;
            let blocks = rows.div_ceil(BLOCK as usize) as c_uint;
            check(
                (cu.cuLaunchKernel)(self.func, blocks, 1, 1, BLOCK, 1, 1, 0, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut()),
                "Starting the kernel",
            )?;
            check((cu.cuMemcpyDtoH_v2)(out.as_mut_ptr() as *mut c_void, self.out_ptr, rows * 8), "The kernel or copying its results")?;
            check((cu.cuMemcpyDtoH_v2)(self.marks.as_mut_ptr() as *mut c_void, self.marks_ptr, 4 * (1 + LIST)), "Copying the results")?;
        }
        let marked = self.marks[0] as usize;
        if marked == 0 {
            return Ok(vec![]);
        }
        if marked <= LIST {
            return Ok(self.marks[1..1 + marked].iter().map(|x| *x as usize).collect());
        }
        Ok((0..rows).filter(|i| out[*i] == -1.0).collect())
    }

    /// Makes `b` at least `bytes` large (contents not kept).
    fn grow(cu: &Driver, b: &mut DevBuf, bytes: usize, what: &str) -> R<()> {
        if b.bytes >= bytes.max(8) {
            return Ok(());
        }
        unsafe {
            if b.bytes > 0 {
                (cu.cuMemFree_v2)(b.ptr);
                b.bytes = 0;
            }
            let size = (bytes.max(8) as f64 * 1.25).ceil() as usize;
            let mut p = 0u64;
            check((cu.cuMemAlloc_v2)(&mut p, size), what)?;
            *b = DevBuf { ptr: p, bytes: size };
        }
        Ok(())
    }

    fn launch(cu: &Driver, f: Ptr, n: usize, args: &mut [*mut c_void]) -> R<()> {
        let blocks = n.div_ceil(BLOCK as usize).max(1) as c_uint;
        unsafe { check((cu.cuLaunchKernel)(f, blocks, 1, 1, BLOCK, 1, 1, 0, std::ptr::null_mut(), args.as_mut_ptr(), std::ptr::null_mut()), "Starting the kernel") }
    }

    /// The logistic model's forward pass: out[r] = Σ w[idx[k]] · val[k] over k in ptr[r]..ptr[r + 1] (compressed
    /// sparse rows), each product and sum rounded on its own, in index order. The weights stay on the card; only the
    /// ones that changed since the last call are sent (the training loop changes a few hundred per mini-batch).
    pub fn logits(&mut self, ptr: &[u64], idx: &[u32], val: &[f64], w: &[f64], out: &mut [f64]) -> R<()> {
        if self.closed {
            return Err(CudaError("The GPU was closed".into()));
        }
        let rows = ptr.len().saturating_sub(1);
        if rows == 0 {
            return Ok(());
        }
        if rows > u32::MAX as usize || w.len() > u32::MAX as usize || idx.len() != val.len() || out.len() < rows {
            return Err(CudaError("logits: sizes out of range".into()));
        }
        unsafe {
            check((self.cu.cuCtxSetCurrent)(self.ctx), "Opening the GPU context")?;
        }
        if self.lk.is_none() {
            let mut module: Ptr = std::ptr::null_mut();
            let image = CString::new(LOGITS_PTX).unwrap();
            unsafe {
                check((self.cu.cuModuleLoadData)(&mut module, image.as_ptr() as *const c_void), "Loading the forward-pass kernel")?;
                let (mut fl, mut fs): (Ptr, Ptr) = (std::ptr::null_mut(), std::ptr::null_mut());
                let r = check((self.cu.cuModuleGetFunction)(&mut fl, module, c"logits_csr".as_ptr()), "Loading the forward-pass kernel")
                    .and_then(|_| check((self.cu.cuModuleGetFunction)(&mut fs, module, c"scatter".as_ptr()), "Loading the forward-pass kernel"));
                if let Err(e) = r {
                    (self.cu.cuModuleUnload)(module);
                    return Err(e);
                }
                self.lk = Some(LogitsKernel {
                    module,
                    f_logits: fl,
                    f_scatter: fs,
                    w: DevBuf::default(),
                    shadow: vec![],
                    ptr: DevBuf::default(),
                    idx: DevBuf::default(),
                    val: DevBuf::default(),
                    out: DevBuf::default(),
                    s_idx: DevBuf::default(),
                    s_val: DevBuf::default(),
                });
            }
        }
        let cu = &self.cu;
        let k = self.lk.as_mut().unwrap();
        unsafe {
            // The weights: all of them when new (or when most changed), else only the changed ones through `scatter`.
            if k.shadow.len() != w.len() {
                Self::grow(cu, &mut k.w, w.len() * 8, "GPU memory for the weights")?;
                check((cu.cuMemcpyHtoD_v2)(k.w.ptr, w.as_ptr() as *const c_void, w.len() * 8), "Copying the weights")?;
                k.shadow = w.to_vec();
            } else {
                let changed: Vec<u32> = (0..w.len()).filter(|j| w[*j].to_bits() != k.shadow[*j].to_bits()).map(|j| j as u32).collect();
                if changed.len() * 8 > w.len() {
                    check((cu.cuMemcpyHtoD_v2)(k.w.ptr, w.as_ptr() as *const c_void, w.len() * 8), "Copying the weights")?;
                    k.shadow.copy_from_slice(w);
                } else if !changed.is_empty() {
                    let vals: Vec<f64> = changed.iter().map(|j| w[*j as usize]).collect();
                    Self::grow(cu, &mut k.s_idx, changed.len() * 4, "GPU memory")?;
                    Self::grow(cu, &mut k.s_val, vals.len() * 8, "GPU memory")?;
                    check((cu.cuMemcpyHtoD_v2)(k.s_idx.ptr, changed.as_ptr() as *const c_void, changed.len() * 4), "Copying the weights")?;
                    check((cu.cuMemcpyHtoD_v2)(k.s_val.ptr, vals.as_ptr() as *const c_void, vals.len() * 8), "Copying the weights")?;
                    let (mut a0, mut a1, mut a2, mut a3) = (k.s_idx.ptr, k.s_val.ptr, k.w.ptr, changed.len() as u32);
                    let mut args = [&mut a0 as *mut _ as *mut c_void, &mut a1 as *mut _ as *mut c_void, &mut a2 as *mut _ as *mut c_void, &mut a3 as *mut _ as *mut c_void];
                    Self::launch(cu, k.f_scatter, changed.len(), &mut args)?;
                    for j in &changed {
                        k.shadow[*j as usize] = w[*j as usize];
                    }
                }
            }
            // The rows, then the forward pass, then the results (all on the context's own stream, in order).
            Self::grow(cu, &mut k.ptr, ptr.len() * 8, "GPU memory for the rows")?;
            Self::grow(cu, &mut k.idx, idx.len() * 4, "GPU memory for the rows")?;
            Self::grow(cu, &mut k.val, val.len() * 8, "GPU memory for the rows")?;
            Self::grow(cu, &mut k.out, rows * 8, "GPU memory for the results")?;
            check((cu.cuMemcpyHtoD_v2)(k.ptr.ptr, ptr.as_ptr() as *const c_void, ptr.len() * 8), "Copying the rows")?;
            if !idx.is_empty() {
                check((cu.cuMemcpyHtoD_v2)(k.idx.ptr, idx.as_ptr() as *const c_void, idx.len() * 4), "Copying the rows")?;
                check((cu.cuMemcpyHtoD_v2)(k.val.ptr, val.as_ptr() as *const c_void, val.len() * 8), "Copying the rows")?;
            }
            let (mut a0, mut a1, mut a2, mut a3, mut a4, mut a5) = (k.ptr.ptr, k.idx.ptr, k.val.ptr, k.w.ptr, k.out.ptr, rows as u32);
            let mut args = [
                &mut a0 as *mut _ as *mut c_void,
                &mut a1 as *mut _ as *mut c_void,
                &mut a2 as *mut _ as *mut c_void,
                &mut a3 as *mut _ as *mut c_void,
                &mut a4 as *mut _ as *mut c_void,
                &mut a5 as *mut _ as *mut c_void,
            ];
            Self::launch(cu, k.f_logits, rows, &mut args)?;
            check((cu.cuMemcpyDtoH_v2)(out.as_mut_ptr() as *mut c_void, k.out.ptr, rows * 8), "The kernel or copying its results")?;
        }
        Ok(())
    }

    /// Card memory taken, in MB.
    pub fn uploaded_mb(&self) -> f64 {
        ((self.uploaded + self.out_bytes) as f64 / 1_048_576.0).round()
    }

    /// Frees what was put on the card; `reset`: the card's context reset too.
    pub fn close(&mut self, reset: bool) {
        if self.closed {
            return;
        }
        unsafe {
            (self.cu.cuCtxSetCurrent)(self.ctx);
        }
        while !self.uploads.is_empty() {
            self.drop_oldest();
        }
        self.closed = true;
        unsafe {
            if self.out_bytes > 0 {
                (self.cu.cuMemFree_v2)(self.out_ptr);
            }
            (self.cu.cuMemFree_v2)(self.marks_ptr);
            if let Some(k) = self.lk.take() {
                for b in [k.w, k.ptr, k.idx, k.val, k.out, k.s_idx, k.s_val] {
                    if b.bytes > 0 {
                        (self.cu.cuMemFree_v2)(b.ptr);
                    }
                }
                (self.cu.cuModuleUnload)(k.module);
            }
            (self.cu.cuModuleUnload)(self.module);
            if reset {
                (self.cu.cuDevicePrimaryCtxReset_v2)(self.dev);
            } else {
                (self.cu.cuDevicePrimaryCtxRelease_v2)(self.dev);
            }
        }
    }
}

impl Drop for Gpu {
    fn drop(&mut self) {
        self.close(false);
    }
}
