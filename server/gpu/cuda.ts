// Use GPU (0.13.0; the sums kernel 0.19.0): the NVIDIA driver's CUDA interface (nvcuda.dll, part of every NVIDIA graphics driver; libcuda.so.1
// on Linux), called through koffi. Nothing else to install: the kernel ships as PTX (kernel.ts, from similarities.cu)
// and the driver compiles it for the card. One Gpu per thread: each loads its own copy of the kernel, on the card's
// context shared by the process.
import type Koffi from 'koffi';
import type { GroupedArrays, GroupedOut } from '../model/estimate.ts';
import { SIMILARITIES_PTX, SUMS_PTX } from './kernel.ts';

type KoffiModule = typeof Koffi;
/** The most context dimensions the kernel takes (similarities.cu MAX_D). */
export const MAX_D = 64;
/** Marked games the kernel lists (similarities.cu LIST). */
const LIST = 256;
const BLOCK = 256;
/** similarities.cu Params: three pointers, den, n, d, then q. */
const Q_AT = 40;
/** sums.cu: threads per block, and SumsParams (eight pointers, then options and the lane opponent). */
const SUMS_BLOCK = 64;
const SUMS_PARAMS = 72;
/** Per option the kernel returns eight doubles and two ints. */
const SUMS_OUT = 8 * 8 + 2 * 4;

const ERRORS: Record<number, string> = {
  1: 'invalid value', 2: 'out of GPU memory', 3: 'driver not initialised', 34: 'the CUDA stub library, not the driver',
  35: 'the NVIDIA driver is older than this app needs (CUDA 12.0)', 46: 'the GPU is busy or unavailable', 100: 'no NVIDIA GPU found',
  101: 'invalid device', 200: 'invalid kernel image', 201: 'invalid context', 209: 'no kernel for this GPU', 218: 'invalid PTX',
  222: 'the NVIDIA driver is too old for the kernel (CUDA 12.0 needed): update the driver', 400: 'invalid handle', 500: 'not found',
  700: 'illegal memory access', 702: 'the GPU timed out', 719: 'the kernel failed', 999: 'unknown error',
};

let koffiLoad: Promise<KoffiModule> | null = null;
function loadKoffi(): Promise<KoffiModule> {
  // The bundled app carries koffi next to its bundles (scripts/bundle.mjs); a source run takes it from node_modules.
  const spec = import.meta.url.endsWith('.ts') ? 'koffi' : new URL('./native/koffi/index.js', import.meta.url).href;
  return (koffiLoad ??= import(spec).then((m: { default?: KoffiModule }) => (m.default ?? m) as KoffiModule));
}

/** The driver library: IRELIA_CUDA_LIB overrides it (a stand-in for tests). */
const driverLibrary = () => process.env.IRELIA_CUDA_LIB ?? (process.platform === 'win32' ? 'nvcuda.dll' : 'libcuda.so.1');

function bind(koffi: KoffiModule, path: string) {
  let lib: ReturnType<KoffiModule['load']>;
  try { lib = koffi.load(path); } catch {
    throw new CudaError(`No NVIDIA driver: ${path} could not be loaded (it comes with the NVIDIA graphics driver)`);
  }
  const f = (proto: string) => {
    try { return lib.func(proto); } catch {
      throw new CudaError(`The NVIDIA driver is older than this app needs (CUDA 12.0): it has no ${/\w+(?=\()/.exec(proto)?.[0]}. Update the driver`);
    }
  };
  return {
    init: f('int cuInit(unsigned int flags)'),
    driverVersion: f('int cuDriverGetVersion(_Out_ int *version)'),
    deviceCount: f('int cuDeviceGetCount(_Out_ int *count)'),
    device: f('int cuDeviceGet(_Out_ int *device, int ordinal)'),
    deviceName: f('int cuDeviceGetName(void *name, int len, int dev)'),
    attribute: f('int cuDeviceGetAttribute(_Out_ int *value, int attrib, int dev)'),
    totalMem: f('int cuDeviceTotalMem_v2(_Out_ size_t *bytes, int dev)'),
    memInfo: f('int cuMemGetInfo_v2(_Out_ size_t *free, _Out_ size_t *total)'),
    retain: f('int cuDevicePrimaryCtxRetain(_Out_ void **ctx, int dev)'),
    release: f('int cuDevicePrimaryCtxRelease_v2(int dev)'),
    reset: f('int cuDevicePrimaryCtxReset_v2(int dev)'),
    setCurrent: f('int cuCtxSetCurrent(void *ctx)'),
    loadModule: f('int cuModuleLoadData(_Out_ void **module, const char *image)'),
    unloadModule: f('int cuModuleUnload(void *module)'),
    getFunction: f('int cuModuleGetFunction(_Out_ void **fn, void *module, const char *name)'),
    getGlobal: f('int cuModuleGetGlobal_v2(_Out_ uint64_t *dptr, _Out_ size_t *bytes, void *module, const char *name)'),
    alloc: f('int cuMemAlloc_v2(_Out_ uint64_t *dptr, size_t bytes)'),
    free: f('int cuMemFree_v2(uint64_t dptr)'),
    toDevice: f('int cuMemcpyHtoD_v2(uint64_t dst, const void *src, size_t bytes)'),
    toHost: f('int cuMemcpyDtoH_v2(void *dst, uint64_t src, size_t bytes)'),
    memset32: f('int cuMemsetD32_v2(uint64_t dst, unsigned int value, size_t count)'),
    launch: f('int cuLaunchKernel(void *fn, unsigned int gx, unsigned int gy, unsigned int gz, unsigned int bx, unsigned int by, unsigned int bz, unsigned int shared, void *stream, void *params, void *extra)'),
  };
}
type Driver = ReturnType<typeof bind>;

export class CudaError extends Error {}
function check(code: number, what: string) {
  if (code !== 0) throw new CudaError(`${what}: ${ERRORS[code] ?? 'CUDA error'} (${code})`);
}

export interface GpuDevice { name: string; capability: string; memoryMb: number; freeMb: number; driver: string }

/**
 * A copy of something on the card, freed when its owner goes or when newer ones need the room: a model's draft contexts
 * (`games`), a lane's observations grouped by option (`lane`, for the sums), or a draft's similarities (`sims`).
 */
type UploadKind = 'games' | 'lane' | 'sims';
interface Upload {
  ptr: bigint; bytes: number; key: WeakRef<object>; freed: boolean; kind: UploadKind;
  /** A lane: where each array starts in the copy, and the largest row it reads. */
  at?: { w: number; wpa: number; win: number; opp: number; row: number; maxRow: number };
}
/** The most copies of each kind kept at once (the card's memory limit applies to all of them together). */
const KEEP: Record<UploadKind, number> = { games: 8, lane: 512, sims: 6 };

const NONE: number[] = [];

export class Gpu {
  private readonly uploads = new WeakMap<object, Upload>();
  /** Oldest first. */
  private readonly recent: Upload[] = [];
  private uploaded = 0;
  private readonly registry = new FinalizationRegistry<Upload>((u) => this.drop(u));
  private outPtr = 0n;
  private outBytes = 0;
  private readonly params = new Uint8Array(Q_AT + 8 * MAX_D);
  private readonly view = new DataView(this.params.buffer);
  private readonly marks = new Uint32Array(1 + LIST);
  private closed = false;
  private sumsOutPtr = 0n;
  private sumsOutBytes = 0;
  private sumsHost = new Uint8Array(0);
  private readonly sumsParams = new Uint8Array(SUMS_PARAMS);
  private readonly sumsView = new DataView(this.sumsParams.buffer);

  private constructor(private readonly cu: Driver, private readonly dev: number, private readonly module: unknown,
    private readonly fn: unknown, private readonly paramsPtr: bigint, private readonly marksPtr: bigint,
    /** The card memory the uploads may take. */
    private readonly budget: number, readonly device: GpuDevice,
    /** The sums kernel (0.19.0), or null with the reason it isn't there: the similarities still run. */
    private readonly sumsKernel: { module: unknown; fn: unknown; params: bigint } | null, readonly sumsReason: string | null) {}

  /** The first NVIDIA GPU, ready to run the kernel; throws with the reason it can't (no driver, no GPU, too old). */
  static async open(): Promise<Gpu> {
    const koffi = await loadKoffi();
    const cu = bind(koffi, driverLibrary());
    check(cu.init(0), 'Starting CUDA');
    const version = [0], count = [0], dev = [0];
    check(cu.driverVersion(version), 'Driver version');
    check(cu.deviceCount(count), 'Counting GPUs');
    if (count[0] < 1) throw new CudaError('No NVIDIA GPU found');
    check(cu.device(dev, 0), 'Opening the GPU');
    const nameBuf = Buffer.alloc(256);
    check(cu.deviceName(nameBuf, nameBuf.length, dev[0]), 'GPU name');
    const major = [0], minor = [0], total = [0];
    check(cu.attribute(major, 75, dev[0]), 'Compute capability');
    check(cu.attribute(minor, 76, dev[0]), 'Compute capability');
    check(cu.totalMem(total, dev[0]), 'GPU memory');
    const name = nameBuf.toString('utf8', 0, Math.max(0, nameBuf.indexOf(0)));
    if (major[0] * 10 + minor[0] < 52) throw new CudaError(`${name} is too old (compute capability ${major[0]}.${minor[0]}; 5.2 or later needed)`);
    const ctx: unknown[] = [null];
    check(cu.retain(ctx, dev[0]), 'Opening the GPU context');
    let module: unknown = null;
    try {
      check(cu.setCurrent(ctx[0]), 'Opening the GPU context');
      const mod: unknown[] = [null], fn: unknown[] = [null], params = [0n as number | bigint], size = [0], marks = [0n as number | bigint], free = [0];
      check(cu.loadModule(mod, SIMILARITIES_PTX), 'Loading the kernel');
      module = mod[0];
      check(cu.getFunction(fn, module, 'similarities'), 'Loading the kernel');
      check(cu.getGlobal(params, size, module, 'params'), 'Loading the kernel');
      check(cu.alloc(marks, 4 * (1 + LIST)), 'GPU memory');
      check(cu.memInfo(free, [0]), 'GPU memory');
      // The second kernel: if the driver won't take it, the similarities still run on the card and the sums on the processor.
      let sumsKernel: { module: unknown; fn: unknown; params: bigint } | null = null, sumsReason: string | null = null;
      try {
        const m2: unknown[] = [null], f2: unknown[] = [null], p2 = [0n as number | bigint], s2 = [0];
        check(cu.loadModule(m2, SUMS_PTX), 'Loading the sums kernel');
        try {
          check(cu.getFunction(f2, m2[0], 'sums'), 'Loading the sums kernel');
          check(cu.getGlobal(p2, s2, m2[0], 'params'), 'Loading the sums kernel');
        } catch (err) { cu.unloadModule(m2[0]); throw err; }
        sumsKernel = { module: m2[0], fn: f2[0], params: BigInt(p2[0]) };
      } catch (err) { sumsReason = err instanceof Error ? err.message : String(err); }
      const v = Number(version[0]);
      const device: GpuDevice = {
        name, capability: `${major[0]}.${minor[0]}`, memoryMb: Math.round(Number(total[0]) / 2 ** 20), freeMb: Math.round(Number(free[0]) / 2 ** 20),
        driver: `CUDA ${Math.floor(v / 1000)}.${(v % 1000) / 10}`,
      };
      return new Gpu(cu, dev[0], module, fn[0], BigInt(params[0]), BigInt(marks[0]), Math.min(2 ** 30, Number(free[0]) / 4), device, sumsKernel, sumsReason);
    } catch (err) {
      if (module) cu.unloadModule(module);
      cu.release(dev[0]);
      throw err;
    }
  }

  /**
   * Frees everything this process put on the first NVIDIA GPU (its context reset): an evaluation's threads, stopped
   * before they could. Nothing without an NVIDIA driver.
   */
  static async reset(): Promise<boolean> {
    const koffi = await loadKoffi();
    let cu: Driver;
    try { cu = bind(koffi, driverLibrary()); } catch { return false; }   // no driver: nothing was put there
    const dev = [0];
    if (cu.init(0) !== 0 || cu.device(dev, 0) !== 0) return false;
    check(cu.reset(dev[0]), 'Freeing the GPU');
    return true;
  }

  /**
   * Each of `rows` games' similarity (the rows of `flat`, `d` wide) to the draft `q`, into `out`. Returns the games the
   * GPU left to the processor (exp too close to call, or out of range), whose `out` is -1 meanwhile.
   */
  similarities(flat: Float64Array, rows: number, d: number, q: Float64Array, den: number, out: Float64Array): ArrayLike<number> {
    if (this.closed) throw new CudaError('The GPU was closed');
    const cu = this.cu;
    const flatPtr = this.upload(flat);
    if (this.outBytes < rows * 8) {
      if (this.outBytes) check(cu.free(this.outPtr), 'GPU memory');
      this.outBytes = 0;
      const ptr = [0n as number | bigint];
      check(cu.alloc(ptr, Math.ceil(rows * 1.25) * 8), 'GPU memory');
      this.outPtr = BigInt(ptr[0]);
      this.outBytes = Math.ceil(rows * 1.25) * 8;
    }
    const v = this.view;
    v.setBigUint64(0, flatPtr, true);
    v.setBigUint64(8, this.outPtr, true);
    v.setBigUint64(16, this.marksPtr, true);
    v.setFloat64(24, den, true);
    v.setInt32(32, rows, true);
    v.setInt32(36, d, true);
    for (let c = 0; c < d; c++) v.setFloat64(Q_AT + 8 * c, q[c], true);
    // All on the context's own stream, in this order: each step waits for the one before.
    check(cu.toDevice(this.paramsPtr, this.params, Q_AT + 8 * d), 'Copying the draft');
    check(cu.memset32(this.marksPtr, 0, 1), 'Copying the draft');
    check(cu.launch(this.fn, Math.ceil(rows / BLOCK), 1, 1, BLOCK, 1, 1, 0, null, null, null), 'Starting the kernel');
    check(cu.toHost(out, this.outPtr, rows * 8), 'The kernel or copying its results');
    check(cu.toHost(this.marks, this.marksPtr, this.marks.byteLength), 'Copying the results');
    const marked = this.marks[0];
    if (marked === 0) return NONE;
    if (marked <= LIST) return this.marks.slice(1, 1 + marked);
    const all: number[] = [];
    for (let i = 0; i < rows; i++) if (out[i] === -1) all.push(i);
    return all;
  }

  /** Card memory taken and the card's free memory at opening, in MB. */
  memory() { return { uploadedMb: Math.round((this.uploaded + this.outBytes + this.sumsOutBytes) / 2 ** 20), freeMb: this.device.freeMb }; }

  /**
   * Frees what this thread put on the card. `reset`: everything every thread of the process put there too (the card's
   * context reset), for the evaluation's other threads, which are stopped without a chance to free theirs.
   */
  close(o: { reset?: boolean } = {}) {
    if (this.closed) return;
    for (const u of [...this.recent]) this.drop(u);
    this.closed = true;
    if (this.outBytes) this.cu.free(this.outPtr);
    if (this.sumsOutBytes) this.cu.free(this.sumsOutPtr);
    this.cu.free(this.marksPtr);
    if (this.sumsKernel) this.cu.unloadModule(this.sumsKernel.module);
    this.cu.unloadModule(this.module);
    if (o.reset) this.cu.reset(this.dev); else this.cu.release(this.dev);
  }

  /** Room for `bytes` more of `kind`: the oldest copies go first (of this kind past its limit, then of any kind past the budget). */
  private makeRoom(kind: UploadKind, bytes: number): boolean {
    if (bytes > this.budget) return false;
    for (;;) {
      const mine = this.recent.filter((u) => u.kind === kind);
      if (mine.length < KEEP[kind]) break;
      this.drop(mine[0]);
    }
    while (this.recent.length && this.uploaded + bytes > this.budget) this.drop(this.recent[0]);
    return true;
  }

  /** Marks `u` as the most recently used. */
  private touch(u: Upload) {
    this.recent.splice(this.recent.indexOf(u), 1);
    this.recent.push(u);
  }

  private keep(key: object, u: Upload) {
    this.uploads.set(key, u);
    this.recent.push(u);
    this.uploaded += u.bytes;
    this.registry.register(key, u, u);
  }

  private upload(flat: Float64Array): bigint {
    const known = this.uploads.get(flat);
    if (known) { this.touch(known); return known.ptr; }
    const bytes = flat.byteLength;
    if (!this.makeRoom('games', bytes)) throw new CudaError('GPU memory: the games do not fit the share this thread may take');
    const ptr = [0n as number | bigint];
    check(this.cu.alloc(ptr, bytes), 'GPU memory for the games');
    const u: Upload = { ptr: BigInt(ptr[0]), bytes, key: new WeakRef(flat), freed: false, kind: 'games' };
    check(this.cu.toDevice(u.ptr, flat, bytes), 'Copying the games');
    this.keep(flat, u);
    return u.ptr;
  }

  /** The lane's observations on the card (once, until they go or newer ones need the room), or null when they don't fit. */
  private uploadLane(g: GroupedArrays, K: number, N: number): Upload | null {
    const known = this.uploads.get(g);
    if (known) { this.touch(known); return known; }
    const align = (n: number) => Math.ceil(n / 8) * 8;
    const w = align((K + 1) * 4), wpa = w + 8 * N, win = wpa + 8 * N, opp = win + 8 * N, row = opp + 4 * N, bytes = align(row + 4 * N);
    if (!this.makeRoom('lane', bytes)) return null;
    const host = new ArrayBuffer(bytes);
    new Int32Array(host, 0, K + 1).set(g.start);
    new Float64Array(host, w, N).set(g.gw);
    new Float64Array(host, wpa, N).set(g.gwpa);
    new Float64Array(host, win, N).set(g.gwin);
    new Int32Array(host, opp, N).set(g.gopp);
    new Int32Array(host, row, N).set(g.grow);
    let maxRow = -1;
    for (let i = 0; i < N; i++) { const r = g.grow[i]; if (r < 0) return null; if (r > maxRow) maxRow = r; }   // a row the processor would read as undefined: left to it
    const ptr = [0n as number | bigint];
    check(this.cu.alloc(ptr, bytes), 'GPU memory for the sums');
    const u: Upload = { ptr: BigInt(ptr[0]), bytes, key: new WeakRef(g), freed: false, kind: 'lane', at: { w, wpa, win, opp, row, maxRow } };
    check(this.cu.toDevice(u.ptr, new Uint8Array(host), bytes), 'Copying the games for the sums');
    this.keep(g, u);
    return u;
  }

  /**
   * A draft's similarities on the card. The processor's loop reuses its buffers, so they are copied on every question
   * (cheap next to the sums): a copy kept from an earlier one could be stale, and the numbers must be the processor's.
   */
  private uploadSims(sims: Float64Array): bigint | null {
    let u = this.uploads.get(sims);
    if (u && u.bytes !== sims.byteLength) { this.drop(u); u = undefined; }
    if (u) this.touch(u);
    else {
      const bytes = sims.byteLength;
      if (!this.makeRoom('sims', bytes)) return null;
      const ptr = [0n as number | bigint];
      check(this.cu.alloc(ptr, bytes), 'GPU memory for the similarities');
      u = { ptr: BigInt(ptr[0]), bytes, key: new WeakRef(sims), freed: false, kind: 'sims' };
      this.keep(sims, u);
    }
    check(this.cu.toDevice(u.ptr, sims, sims.byteLength), 'Copying the similarities');
    return u.ptr;
  }

  /** Whether the sums kernel is there (else `sumsReason` says why not). */
  get hasSums() { return this.sumsKernel !== null; }

  /**
   * The sums estimate.ts groupedLoop() adds up (one thread per option, the same additions in the same order), into
   * `out`. False: left to the processor (too little to gain, or it doesn't fit the card).
   */
  sums(g: GroupedArrays, sims: Float64Array, opponent: number, out: GroupedOut): boolean {
    const k = this.sumsKernel;
    if (this.closed || !k) return false;
    const K = g.start.length - 1, N = g.gw.length;
    if (K < 1 || N < 1) return false;
    const lane = this.uploadLane(g, K, N);
    if (!lane || lane.at!.maxRow >= sims.length) return false;   // out of range: the processor reads what the loop reads
    const simsPtr = this.uploadSims(sims);
    if (simsPtr === null) return false;
    const cu = this.cu;
    const bytes = K * SUMS_OUT;
    if (this.sumsOutBytes < bytes) {
      if (this.sumsOutBytes) check(cu.free(this.sumsOutPtr), 'GPU memory');
      this.sumsOutBytes = 0;
      const ptr = [0n as number | bigint];
      const size = Math.ceil(bytes * 1.25 / 8) * 8;
      check(cu.alloc(ptr, size), 'GPU memory for the sums');
      this.sumsOutPtr = BigInt(ptr[0]);
      this.sumsOutBytes = size;
      this.sumsHost = new Uint8Array(new ArrayBuffer(size));
    }
    const at = lane.at!, v = this.sumsView;
    v.setBigUint64(0, lane.ptr, true);
    v.setBigUint64(8, lane.ptr + BigInt(at.w), true);
    v.setBigUint64(16, lane.ptr + BigInt(at.wpa), true);
    v.setBigUint64(24, lane.ptr + BigInt(at.win), true);
    v.setBigUint64(32, lane.ptr + BigInt(at.opp), true);
    v.setBigUint64(40, lane.ptr + BigInt(at.row), true);
    v.setBigUint64(48, simsPtr, true);
    v.setBigUint64(56, this.sumsOutPtr, true);
    v.setInt32(64, K, true);
    v.setInt32(68, opponent, true);
    check(cu.toDevice(k.params, this.sumsParams, SUMS_PARAMS), 'Copying the question');
    check(cu.launch(k.fn, Math.ceil(K / SUMS_BLOCK), 1, 1, SUMS_BLOCK, 1, 1, 0, null, null, null), 'Starting the sums kernel');
    check(cu.toHost(this.sumsHost, this.sumsOutPtr, bytes), 'The sums kernel or copying its results');
    const d = new Float64Array(this.sumsHost.buffer, 0, 8 * K), n = new Int32Array(this.sumsHost.buffer, 64 * K, 2 * K);
    out.csw.set(d.subarray(0, K)); out.csw2.set(d.subarray(K, 2 * K)); out.cswx.set(d.subarray(2 * K, 3 * K)); out.cswin.set(d.subarray(3 * K, 4 * K));
    out.msw.set(d.subarray(4 * K, 5 * K)); out.msw2.set(d.subarray(5 * K, 6 * K)); out.mswx.set(d.subarray(6 * K, 7 * K)); out.mswin.set(d.subarray(7 * K, 8 * K));
    out.mn.set(n.subarray(0, K)); out.mseen.set(n.subarray(K, 2 * K));
    return true;
  }

  private drop(u: Upload) {
    if (u.freed) return;
    u.freed = true;
    const at = this.recent.indexOf(u);
    if (at >= 0) this.recent.splice(at, 1);
    const key = u.key.deref();
    if (key) this.uploads.delete(key);
    this.registry.unregister(u);
    this.uploaded -= u.bytes;
    this.cu.free(u.ptr);
  }
}
