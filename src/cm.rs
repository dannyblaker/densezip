//! dzcm — densezip's context-mixing compressor.
//!
//! Architecture (lpaq/zpaq-family, tuned for ratio over speed):
//!   * bitwise binary arithmetic coder (12-bit probabilities)
//!   * order-0/1 direct + order-2..6,8 / word / sparse / record hashed
//!     context models; each slot holds a bit-history state AND an adaptive
//!     probability counter, both fed to the mixer (hybrid ICM)
//!   * order-8 match model with length-bucketed confidence counters
//!   * two-bank logistic mixer (paq8-style summed weight sets)
//!   * two APM/SSE refinement stages
//!   * reversible E8/E9 x86 branch transform, autodetected
//!   * record/row stride model, autodetected (or supplied for pixel data)
//!
//! Everything is integer arithmetic with an embedded squash table, so output
//! is bit-identical across platforms.

use anyhow::{bail, ensure, Result};

mod tables {
    include!("cm_tables.rs");
}

// hashed tables: orders 2,3,4,5,6,8 + word + sparse1 + sparse2 + rec1 + rec2 + rec3
const NHASH: usize = 12;
const NMODELS: usize = 2 + NHASH; // + o0, o1
const NCHAIN: usize = 6; // ISSE chain stages: orders 2,3,4,5,6,8
// mixer inputs: 2 per model (counter + state map) + ISSE chain + match + bias
const NIN: usize = 2 * NMODELS + 3;
const MIXER_CTXS: usize = 512; // bank 1: c0 (256) x match-active (2); bank 2 follows
const MATCH_MIN: usize = 8;
const GOLD: u64 = 0x9E37_79B9_7F4A_7C15;

#[inline]
fn squash(x: i32) -> i32 {
    let x = x.clamp(-2047, 2047);
    tables::SQUASH[(x + 2048) as usize] as i32
}

fn build_stretch() -> Vec<i16> {
    // Inverse of squash: for each 12-bit probability, the x with squash(x) ~ p.
    let mut st = vec![0i16; 4096];
    let mut j = 0usize;
    for x in -2047..=2047i32 {
        let p = squash(x) as usize;
        while j <= p && j < 4096 {
            st[j] = x as i16;
            j += 1;
        }
    }
    while j < 4096 {
        st[j] = 2047;
        j += 1;
    }
    st
}

#[inline]
fn splitmix(mut x: u64) -> u64 {
    x = x.wrapping_add(GOLD);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

// ---------------------------------------------------------------------------
// Bit-history state machine: states are bounded (n0, n1) count pairs with
// discounting on surprise. ~120 states; deterministic construction.

const STATE_CAPS: [u8; 5] = [40, 10, 6, 5, 4]; // max(n0,n1) allowed per min(n0,n1)

fn state_valid(n0: u32, n1: u32) -> bool {
    let lo = n0.min(n1) as usize;
    let hi = n0.max(n1);
    lo < STATE_CAPS.len() && hi <= STATE_CAPS[lo] as u32
}

struct StateTable {
    /// next[state][bit] -> state
    next: Vec<[u8; 2]>,
    /// counts[state] = (n0, n1)
    counts: Vec<(u8, u8)>,
}

fn build_states() -> StateTable {
    let mut pairs: Vec<(u8, u8)> = Vec::new();
    for n0 in 0..=40u32 {
        for n1 in 0..=40u32 {
            if state_valid(n0, n1) {
                pairs.push((n0 as u8, n1 as u8));
            }
        }
    }
    // stable order: by total count then n1 (deterministic)
    pairs.sort_by_key(|&(a, b)| (a as u32 + b as u32, b, a));
    let index = |n0: u32, n1: u32| -> u8 {
        pairs.iter().position(|&(a, b)| a as u32 == n0 && b as u32 == n1).unwrap() as u8
    };
    let mut next = Vec::with_capacity(pairs.len());
    for &(n0, n1) in &pairs {
        let mut nx = [0u8; 2];
        for bit in 0..2u32 {
            let (mut a, mut b) = (n0 as u32, n1 as u32);
            if bit == 1 {
                b += 1;
                if a > 2 {
                    a = a / 2 + 1; // discount the opposite count (non-stationarity)
                }
            } else {
                a += 1;
                if b > 2 {
                    b = b / 2 + 1;
                }
            }
            while !state_valid(a, b) {
                if a > b {
                    a -= 1;
                } else {
                    b -= 1;
                }
            }
            nx[bit as usize] = index(a, b);
        }
        next.push(nx);
    }
    StateTable { next, counts: pairs }
}

// ---------------------------------------------------------------------------
// Counters and state maps

/// Packed slot: check(8) | state(8) | p16(16). p16==0 reads as 32768 (fresh).
#[inline]
fn slot_p(slot: u32) -> i32 {
    let p = slot & 0xffff;
    if p == 0 { 32768 } else { p as i32 }
}

/// State-map entry: p16(16) | count(16).
#[inline]
fn sm_p(e: u32) -> i32 {
    (e >> 16) as i32
}

#[inline]
fn sm_update(e: &mut u32, bit: i32, recip: &[u32]) {
    let p = (*e >> 16) as i32;
    let n = (*e & 0xffff) as usize;
    let target = if bit == 1 { 65535 } else { 1 };
    let np = (p + (((target - p) * recip[n] as i32) >> 16)).clamp(1, 65535) as u32;
    let nn = if n < recip.len() - 1 { n + 1 } else { n };
    *e = (np << 16) | nn as u32;
}

pub struct CmConfig {
    pub table_bits: u32,
    pub match_bits: u32,
    /// Record/row length for the record model (0 = autodetect).
    pub stride: u32,
    /// Bytes per pixel for image data (0 = not an image).
    pub bpp: u32,
}

/// Approximate model memory for a given table size: NHASH tables of 4-byte
/// slots dominate; o1/match/smaps/mixer add a little.
pub fn model_mem_bytes(table_bits: u32, match_bits: u32) -> u64 {
    (NHASH as u64) * 4 * (1u64 << table_bits) + 4 * (1u64 << match_bits) + (1 << 20)
}

impl CmConfig {
    pub fn for_len(len: usize) -> Self {
        Self::for_len_capped(len, u64::MAX)
    }

    /// Size the model for `len` input bytes, but keep total model memory
    /// under `mem_cap` bytes (floor: 16-bit tables, ~4 MiB).
    pub fn for_len_capped(len: usize, mem_cap: u64) -> Self {
        let lg = (len.max(1) as u64).next_power_of_two().trailing_zeros();
        let mut table_bits = (lg + 3).clamp(16, 26);
        let mut match_bits = lg.clamp(16, 24);
        while table_bits > 16 && model_mem_bytes(table_bits, match_bits) > mem_cap {
            table_bits -= 1;
            match_bits = match_bits.min(table_bits);
        }
        CmConfig { table_bits, match_bits, stride: 0, bpp: 0 }
    }
}

/// Detect a fixed record/row stride by autocorrelation: the s maximizing
/// P(buf[i] == buf[i-s]). Returns 0 if nothing stands out.
fn detect_stride(data: &[u8]) -> u32 {
    const MAX_STRIDE: usize = 4096;
    if data.len() < 4 * MAX_STRIDE {
        return 0;
    }
    let window = data.len().min(1 << 21);
    let mut counts = vec![0u32; MAX_STRIDE + 1];
    let mut total = 0u32;
    let mut i = MAX_STRIDE;
    while i < window {
        let b = data[i];
        for s in 2..=MAX_STRIDE {
            if data[i - s] == b {
                counts[s] += 1;
            }
        }
        total += 1;
        i += 37; // sample sparsely; autocorrelation survives
    }
    if total < 1000 {
        return 0;
    }
    let (best_s, best_c) =
        (2..=MAX_STRIDE).map(|s| (s, counts[s])).max_by_key(|&(_, c)| c).unwrap();
    // Require a clear signal: >18% self-similarity and 1.5x the median level.
    let mut sorted: Vec<u32> = counts[2..].to_vec();
    sorted.sort_unstable();
    let median = sorted[sorted.len() / 2].max(1);
    if best_c * 100 / total >= 18 && best_c >= median * 3 / 2 {
        best_s as u32
    } else {
        0
    }
}

/// Reversible x86 CALL/JMP (E8/E9) relative->absolute address transform.
/// Both directions scan identically and skip 5 bytes after each hit, so the
/// mapping is exact. Absolute targets are stored big-endian so their high
/// bytes cluster for the context models.
fn e8e9_apply(data: &mut [u8], forward: bool) {
    let n = data.len();
    let mut i = 0usize;
    while i + 5 <= n {
        let b = data[i];
        if b == 0xE8 || b == 0xE9 {
            let base = (i as u32).wrapping_add(5);
            if forward {
                let rel = u32::from_le_bytes(data[i + 1..i + 5].try_into().unwrap());
                let abs = rel.wrapping_add(base);
                data[i + 1..i + 5].copy_from_slice(&abs.to_be_bytes());
            } else {
                let abs = u32::from_be_bytes(data[i + 1..i + 5].try_into().unwrap());
                let rel = abs.wrapping_sub(base);
                data[i + 1..i + 5].copy_from_slice(&rel.to_le_bytes());
            }
            i += 5;
        } else {
            i += 1;
        }
    }
}

/// Heuristic: does this look like x86 code? Count E8/E9 bytes followed by a
/// plausible small relative offset (sign-extension byte 0x00 or 0xFF).
fn looks_like_x86(data: &[u8]) -> bool {
    if data.len() < 1 << 16 {
        return false;
    }
    let window = data.len().min(1 << 22);
    let mut hits = 0usize;
    let mut i = 0usize;
    while i + 5 <= window {
        let b = data[i];
        if (b == 0xE8 || b == 0xE9) && (data[i + 4] == 0x00 || data[i + 4] == 0xFF) {
            hits += 1;
            i += 5;
        } else {
            i += 1;
        }
    }
    hits * 1500 > window // > ~1 plausible call site per 1.5KB
}

// ---------------------------------------------------------------------------

/// APM / SSE: refines a probability using a context, with interpolation
/// between 33 stretch-domain buckets.
struct Apm {
    t: Vec<u16>,
    index: usize,
}

impl Apm {
    fn new(n: usize) -> Self {
        let mut t = vec![0u16; n * 33];
        for i in 0..n {
            for j in 0..33 {
                t[i * 33 + j] = (squash((j as i32 - 16) * 128) * 16) as u16;
            }
        }
        Apm { t, index: 0 }
    }

    fn pp(&mut self, pr12: i32, cxt: usize, stretch: &[i16]) -> i32 {
        let s = stretch[pr12.clamp(0, 4095) as usize] as i32; // -2047..2047
        let v = (s + 2048) * 32; // 32..131040
        let j = (v >> 12).min(31) as usize;
        let w = v & 4095;
        self.index = cxt * 33 + j;
        ((self.t[self.index] as i32 * (4096 - w) + self.t[self.index + 1] as i32 * w) >> 16)
            .clamp(1, 4095)
    }

    fn update(&mut self, bit: i32) {
        const RATE: i32 = 7;
        let g = if bit == 1 { 65535 } else { 0 };
        for k in [self.index, self.index + 1] {
            let t = self.t[k] as i32;
            self.t[k] = (t + ((g - t) >> RATE)) as u16;
        }
    }
}

struct Model {
    table_bits: u32,
    stride: usize,
    bpp: usize,
    states: StateTable,
    o0: Vec<u32>,          // 256 direct slots
    o1: Vec<u32>,          // 64K direct slots
    hashed: Vec<Vec<u32>>, // NHASH context-hashed tables
    /// One state map per model: bit-history state -> probability.
    smaps: Vec<Vec<u32>>,
    match_tbl: Vec<u32>,
    match_bits: u32,
    match_ctr: Vec<u32>, // confidence counters by (length bucket, expected bit)
    buf: Vec<u8>,
    window: u64,
    word_hash: u64,
    hashes: [u64; NHASH],
    c0: u32, // partial byte with sentinel bit, 1..255
    bitpos: u32,
    match_pos: usize,
    match_len: u32,
    // mixer
    wx: Vec<i32>,
    inputs: [i32; NIN],
    mixer_ctx: usize,
    mixer_ctx2: usize,
    pr_mix: i32,
    // apm chain
    apm1: Apm,
    apm2: Apm,
    apm3: Apm,
    // ISSE chain: per-stage 2-weight mixers selected by bit-history state
    isse_w: Vec<[i32; 2]>, // NCHAIN * 256
    isse_cache: [(usize, i32, i32, i32); NCHAIN], // (weight idx, st_in, st_icm, p_out)
    // cached (model id, slot index) between predict and update
    active: usize,
    idx: [(usize, usize); NMODELS],
    match_expected: i32,
    match_bucket: usize,
    stretch: Vec<i16>,
    recip_slot: Vec<u32>, // adaptive rates for per-slot counters
    recip_sm: Vec<u32>,   // adaptive rates for state maps (with a rate floor)
}

impl Model {
    fn new(cfg: &CmConfig, expected_len: usize) -> Self {
        let states = build_states();
        let nstates = states.counts.len();
        // per-slot counter rate: keyed by total bit-history count (fast early,
        // slower once confident)
        let recip_slot: Vec<u32> = (0..96).map(|n| 65536 / (n as u32 + 2)).collect();
        // state-map rate: floors at ~1/512 so it keeps adapting
        let recip_sm: Vec<u32> = (0..1024).map(|n| (65536 / (n as u32 + 2)).max(128)).collect();
        // state map init: probability implied by the state's counts
        let mut sm_init = vec![0u32; nstates];
        for (s, &(n0, n1)) in states.counts.iter().enumerate() {
            let p = (((2 * n1 as u64 + 1) * 65536) / (2 * (n0 as u64 + n1 as u64) + 2))
                .clamp(1, 65535) as u32;
            sm_init[s] = p << 16; // count 0: adapts fast initially
        }
        Model {
            table_bits: cfg.table_bits,
            stride: cfg.stride as usize,
            bpp: cfg.bpp as usize,
            o0: vec![0u32; 256],
            o1: vec![0u32; 1 << 16],
            hashed: (0..NHASH).map(|_| vec![0u32; 1 << cfg.table_bits]).collect(),
            smaps: (0..NMODELS).map(|_| sm_init.clone()).collect(),
            match_tbl: vec![0u32; 1 << cfg.match_bits],
            match_bits: cfg.match_bits,
            match_ctr: vec![0u32; 64],
            buf: Vec::with_capacity(expected_len),
            window: 0,
            word_hash: 0,
            hashes: [0; NHASH],
            c0: 1,
            bitpos: 0,
            match_pos: 0,
            match_len: 0,
            wx: vec![0i32; (MIXER_CTXS + 1024) * NIN],
            inputs: [0; NIN],
            mixer_ctx: 0,
            mixer_ctx2: MIXER_CTXS,
            pr_mix: 2048,
            apm1: Apm::new(256),
            apm2: Apm::new(1 << 16),
            apm3: Apm::new(1 << 14),
            isse_w: vec![[3 << 14, 1 << 14]; NCHAIN * 256],
            isse_cache: [(0, 0, 0, 0); NCHAIN],
            active: 0,
            idx: [(0, 0); NMODELS],
            match_expected: -1,
            match_bucket: 0,
            stretch: build_stretch(),
            recip_slot,
            recip_sm,
            states,
        }
    }

    #[inline]
    fn st16(&self, p16: i32) -> i32 {
        self.stretch[(p16 >> 4) as usize] as i32
    }

    #[inline]
    fn rec_active(&self) -> bool {
        self.stride > 0 && self.buf.len() >= 2 * self.stride
    }

    /// Feed one slot's two predictions (counter + state map) into inputs.
    #[inline]
    fn feed(&mut self, model_id: usize, slot: u32, j: usize) {
        let state = ((slot >> 16) & 0xff) as usize;
        self.inputs[2 * j] = self.st16(slot_p(slot));
        self.inputs[2 * j + 1] = self.st16(sm_p(self.smaps[model_id][state]));
    }

    fn predict(&mut self) -> i32 {
        let c0 = self.c0 as usize;
        let b1 = (self.window & 0xff) as usize;

        let mut j = 0usize;
        self.idx[j] = (0, c0);
        let s = self.o0[c0];
        self.feed(0, s, j);
        j += 1;
        let i1 = (b1 << 8) | c0;
        self.idx[j] = (1, i1);
        let s = self.o1[i1];
        self.feed(1, s, j);
        j += 1;

        let shift = 64 - self.table_bits;
        let nhash = if self.rec_active() { NHASH } else { NHASH - 3 };
        for k in 0..nhash {
            let ih = self.hashes[k] ^ (self.c0 as u64).wrapping_mul(GOLD);
            let i = (ih >> shift) as usize;
            let check = ((ih >> 20) & 0xff) as u32;
            let slot = &mut self.hashed[k][i];
            if (*slot >> 24) != check {
                *slot = check << 24; // fresh slot: state 0, p reads as 0.5
            }
            self.idx[j] = (2 + k, i);
            let s = self.hashed[k][i];
            self.feed(2 + k, s, j);
            j += 1;
        }
        self.active = j;
        for jj in j..NMODELS {
            self.inputs[2 * jj] = 0;
            self.inputs[2 * jj + 1] = 0;
        }

        // ISSE chain: o1 -> o2 -> o3 -> o4 -> o5 -> o6 -> o8, each stage a
        // 2-weight mixer selected by the higher order's bit-history state.
        // idx[1] is o1; hashed orders are idx[2..2+NCHAIN] (models 2..2+NCHAIN).
        let mut chain_st = self.st16(slot_p(self.o1[i1]));
        for stage in 0..NCHAIN {
            let (model_id, table_idx) = self.idx[2 + stage];
            let slot = self.hashed[model_id - 2][table_idx];
            let state = ((slot >> 16) & 0xff) as usize;
            let st_icm = self.st16(slot_p(slot));
            let wi = stage * 256 + state;
            let w = self.isse_w[wi];
            let t = ((w[0] as i64 * chain_st as i64 + w[1] as i64 * st_icm as i64) >> 16) as i32;
            let out_st = t.clamp(-2047, 2047);
            self.isse_cache[stage] = (wi, chain_st, st_icm, squash(out_st));
            chain_st = out_st;
        }
        self.inputs[NIN - 3] = chain_st;

        // match model
        self.match_expected = -1;
        self.inputs[NIN - 2] = 0;
        if self.match_len > 0 && self.match_pos < self.buf.len() {
            let pb = self.buf[self.match_pos] as u32;
            let e = ((pb >> (7 - self.bitpos)) & 1) as i32;
            let bucket = (self.match_len.min(31) as usize) * 2 + e as usize;
            self.match_bucket = bucket;
            self.match_expected = e;
            let pc = slot_p(self.match_ctr[bucket]); // P(prediction correct)
            let s = self.st16(pc);
            self.inputs[NIN - 2] = if e == 1 { s } else { -s };
        }
        self.inputs[NIN - 1] = 256; // bias

        // Two weight banks conditioned on different contexts, summed before
        // the squash (paq8-style multi-set mixer).
        self.mixer_ctx = c0 | (((self.match_len > 0) as usize) << 8);
        let mlen_bucket = match self.match_len {
            0 => 0usize,
            1..=15 => 1,
            16..=63 => 2,
            _ => 3,
        };
        self.mixer_ctx2 = MIXER_CTXS + (b1 | (mlen_bucket << 8));
        let mut dot: i64 = 0;
        for ctx in [self.mixer_ctx, self.mixer_ctx2] {
            let w = &self.wx[ctx * NIN..ctx * NIN + NIN];
            for i in 0..NIN {
                dot += self.inputs[i] as i64 * w[i] as i64;
            }
        }
        self.pr_mix = squash((dot >> 16) as i32);

        let pr1 = self.apm1.pp(self.pr_mix, c0, &self.stretch);
        let p_blend1 = (self.pr_mix + pr1 * 3) >> 2;
        let pr2 = self.apm2.pp(p_blend1, ((b1 << 8) | c0) & 0xffff, &self.stretch);
        let p_blend2 = (p_blend1 + pr2 * 3) >> 2;
        // third stage keyed by the current word (text/tag structure)
        let wctx = ((splitmix(self.word_hash ^ (c0 as u64)) >> 50) & 0x3fff) as usize;
        let pr3 = self.apm3.pp(p_blend2, wctx, &self.stretch);
        ((p_blend2 + pr3) >> 1).clamp(1, 4095)
    }

    #[inline]
    fn slot_update(&mut self, model_id: usize, table_idx: usize, bit: i32) {
        let slot = match model_id {
            0 => &mut self.o0[table_idx],
            1 => &mut self.o1[table_idx],
            k => &mut self.hashed[k - 2][table_idx],
        };
        let state = ((*slot >> 16) & 0xff) as usize;
        // per-slot probability, rate from bit-history total count
        let (n0, n1) = self.states.counts[state];
        let n = (n0 as usize + n1 as usize).min(self.recip_slot.len() - 1);
        let p = slot_p(*slot);
        let target = if bit == 1 { 65535 } else { 1 };
        let np = (p + (((target - p) * self.recip_slot[n] as i32) >> 16)).clamp(1, 65535) as u32;
        let ns = self.states.next[state][bit as usize] as u32;
        *slot = (*slot & 0xff00_0000) | (ns << 16) | np;
        // shared state map
        sm_update(&mut self.smaps[model_id][state], bit, &self.recip_sm);
    }

    fn update(&mut self, bit: i32) {
        // ISSE chain weights (each stage trains on its own output error)
        for stage in 0..NCHAIN {
            let (wi, st_in, st_icm, p_out) = self.isse_cache[stage];
            let err = ((bit << 12) - p_out) as i64;
            let w = &mut self.isse_w[wi];
            w[0] += ((st_in as i64 * err) >> 12) as i32;
            w[1] += ((st_icm as i64 * err) >> 12) as i32;
        }
        for j in 0..self.active {
            let (model_id, table_idx) = self.idx[j];
            self.slot_update(model_id, table_idx, bit);
        }
        // match model
        if self.match_expected >= 0 {
            let correct = (bit == self.match_expected) as i32;
            let e = &mut self.match_ctr[self.match_bucket];
            let p = slot_p(*e);
            let n = ((*e >> 16) & 0xff) as usize;
            let target = if correct == 1 { 65535 } else { 1 };
            let rec = self.recip_slot[n.min(self.recip_slot.len() - 1)] as i32;
            let np = (p + (((target - p) * rec) >> 16)).clamp(1, 65535) as u32;
            let nn = (n + 1).min(90) as u32;
            *e = (nn << 16) | np;
            if correct == 0 {
                self.match_len = 0;
            }
        }
        // mixer weights (both banks learn from the same error)
        let err = ((bit << 12) - self.pr_mix) as i64;
        for ctx in [self.mixer_ctx, self.mixer_ctx2] {
            let w = &mut self.wx[ctx * NIN..ctx * NIN + NIN];
            for i in 0..NIN {
                w[i] += ((self.inputs[i] as i64 * err) >> 14) as i32;
            }
        }
        // apms
        self.apm1.update(bit);
        self.apm2.update(bit);
        self.apm3.update(bit);

        // advance bit position
        self.c0 = (self.c0 << 1) | bit as u32;
        self.bitpos += 1;
        if self.bitpos == 8 {
            let byte = (self.c0 & 0xff) as u8;
            self.on_byte(byte);
            self.c0 = 1;
            self.bitpos = 0;
        }
    }

    fn on_byte(&mut self, b: u8) {
        self.buf.push(b);
        let cur = self.buf.len();
        self.window = (self.window << 8) | b as u64;

        // extend or drop the match
        if self.match_len > 0 {
            self.match_pos += 1;
            self.match_len += 1;
        }
        // order-8 match lookup
        if cur >= MATCH_MIN {
            let h = splitmix(self.window);
            let mi = (h >> (64 - self.match_bits)) as usize;
            if self.match_len == 0 {
                let cand = self.match_tbl[mi] as usize;
                if cand >= MATCH_MIN
                    && cand < cur
                    && self.buf[cand - MATCH_MIN..cand] == self.buf[cur - MATCH_MIN..cur]
                {
                    self.match_pos = cand;
                    self.match_len = MATCH_MIN as u32;
                }
            }
            self.match_tbl[mi] = cur as u32;
        }

        // rolling word hash: letters extend the word, anything else resets it
        let lower = b | 0x20;
        if lower.is_ascii_lowercase() || b >= 0x80 {
            self.word_hash = self.word_hash.wrapping_mul(GOLD) ^ splitmix(lower as u64 + 1);
        } else {
            self.word_hash = 0;
        }

        // context hashes: orders 2..6, order 8, word, and two sparse shapes
        for (k, order) in (2u32..=6).enumerate() {
            let ctx = self.window & ((1u128 << (8 * order)) - 1) as u64;
            self.hashes[k] = splitmix(ctx ^ (order as u64).wrapping_mul(0x1234_5678_9ABC_DEF1));
        }
        self.hashes[5] = splitmix(self.window ^ 0x8888_8888_8888_8888); // order 8
        self.hashes[6] = splitmix(self.word_hash ^ 0x0F0F_0F0F_0F0F_0F0F); // word
        // sparse 1: two bytes before the last (skip b1) — record/struct friendly
        self.hashes[7] = splitmix(((self.window >> 8) & 0xffff) ^ 0x3333_0000_0000_3333);
        // sparse 2: coarse nibbles of the last four bytes
        self.hashes[8] = splitmix((self.window & 0xf0f0_f0f0) ^ 0x5555_0000_5555_0000);
        // record model: bytes one and two strides back + column position
        let s = self.stride;
        if s > 0 && cur >= 2 * s {
            let above = self.buf[cur - s] as u64;
            if self.bpp > 0 {
                // image mode: 2D neighborhood in the same color channel
                let bpp = self.bpp;
                let left = if cur >= bpp { self.buf[cur - bpp] as u64 } else { 0 };
                let above_left =
                    if cur >= s + bpp { self.buf[cur - s - bpp] as u64 } else { 0 };
                let above_right =
                    if s > bpp { self.buf[cur - s + bpp] as u64 } else { above };
                // neighbors + gradient context
                self.hashes[9] = splitmix(above | (left << 8) | 0xAAAA_0000_0000_0000);
                self.hashes[10] =
                    splitmix(above | (left << 8) | (above_left << 16) | 0xBBBB_0000_0000_0000);
                let grad = (above as i64 + left as i64 - above_left as i64).clamp(-255, 510)
                    as u64
                    & 0x3ff;
                self.hashes[11] = splitmix(grad | (above_right << 10) | 0xCCCC_0000_0000_0000);
            } else {
                let above2 = self.buf[cur - 2 * s] as u64;
                let col = (cur % s) as u64;
                self.hashes[9] = splitmix(above ^ (col << 8) ^ 0xAAAA_0000_0000_AAAA);
                self.hashes[10] = splitmix(above | (above2 << 8) | 0xBBBB_0000_0000_0000);
                self.hashes[11] = splitmix((col << 8) ^ (b as u64) ^ 0xCCCC_0000_0000_CCCC);
            }
        }
    }
}

// ---------------------------------------------------------------------------

struct Encoder {
    x1: u32,
    x2: u32,
    out: Vec<u8>,
}

impl Encoder {
    fn new() -> Self {
        Encoder { x1: 0, x2: 0xffff_ffff, out: Vec::new() }
    }

    #[inline]
    fn encode(&mut self, bit: i32, p12: i32) {
        let range = (self.x2 - self.x1) as u64;
        let xmid = self.x1 + ((range * p12 as u64) >> 12) as u32;
        if bit == 1 {
            self.x2 = xmid;
        } else {
            self.x1 = xmid + 1;
        }
        while (self.x1 ^ self.x2) & 0xff00_0000 == 0 {
            self.out.push((self.x2 >> 24) as u8);
            self.x1 <<= 8;
            self.x2 = (self.x2 << 8) | 0xff;
        }
    }

    fn flush(mut self) -> Vec<u8> {
        for _ in 0..4 {
            self.out.push((self.x1 >> 24) as u8);
            self.x1 <<= 8;
        }
        self.out
    }
}

struct Decoder<'a> {
    x1: u32,
    x2: u32,
    x: u32,
    input: &'a [u8],
    pos: usize,
}

impl<'a> Decoder<'a> {
    fn new(input: &'a [u8]) -> Self {
        let mut d = Decoder { x1: 0, x2: 0xffff_ffff, x: 0, input, pos: 0 };
        for _ in 0..4 {
            d.x = (d.x << 8) | d.next_byte() as u32;
        }
        d
    }

    #[inline]
    fn next_byte(&mut self) -> u8 {
        let b = self.input.get(self.pos).copied().unwrap_or(0);
        self.pos += 1;
        b
    }

    #[inline]
    fn decode(&mut self, p12: i32) -> i32 {
        let range = (self.x2 - self.x1) as u64;
        let xmid = self.x1 + ((range * p12 as u64) >> 12) as u32;
        let bit = if self.x <= xmid { 1 } else { 0 };
        if bit == 1 {
            self.x2 = xmid;
        } else {
            self.x1 = xmid + 1;
        }
        while (self.x1 ^ self.x2) & 0xff00_0000 == 0 {
            self.x1 <<= 8;
            self.x2 = (self.x2 << 8) | 0xff;
            self.x = (self.x << 8) | self.next_byte() as u32;
        }
        bit
    }
}

const VERSION: u8 = 3;
const FLAG_E8E9: u8 = 1;

pub fn compress(data: &[u8]) -> Result<Vec<u8>> {
    compress_with_stride(data, 0, 0, u64::MAX)
}

/// Compress with a known record/row stride and bytes-per-pixel (image pixel
/// data), skipping autodetection when `stride_hint` > 0. `mem_cap` bounds
/// model memory in bytes (the chosen size is recorded in the header, so
/// decompression needs the same amount).
pub fn compress_with_stride(data: &[u8], stride_hint: u32, bpp: u32, mem_cap: u64) -> Result<Vec<u8>> {
    ensure!(data.len() < u32::MAX as usize - 16, "dzcm streams are limited to <4 GiB");
    let mut cfg = CmConfig::for_len_capped(data.len(), mem_cap);

    let mut flags = 0u8;
    let mut transformed: Vec<u8>;
    let input: &[u8] = if stride_hint == 0 && looks_like_x86(data) {
        flags |= FLAG_E8E9;
        transformed = data.to_vec();
        e8e9_apply(&mut transformed, true);
        &transformed
    } else {
        data
    };
    cfg.stride = if stride_hint > 0 { stride_hint } else { detect_stride(input) };
    cfg.bpp = if stride_hint > 0 { bpp } else { 0 };

    let mut header = vec![VERSION, cfg.table_bits as u8, cfg.match_bits as u8, flags];
    crate::util::write_varint(&mut header, cfg.stride as u64);
    crate::util::write_varint(&mut header, cfg.bpp as u64);
    crate::util::write_varint(&mut header, data.len() as u64);
    if data.is_empty() {
        return Ok(header);
    }
    let mut model = Model::new(&cfg, input.len());
    let mut enc = Encoder::new();
    for &byte in input {
        for j in (0..8).rev() {
            let bit = ((byte >> j) & 1) as i32;
            let p = model.predict();
            enc.encode(bit, p);
            model.update(bit);
        }
    }
    let mut out = header;
    out.extend_from_slice(&enc.flush());
    Ok(out)
}

pub fn decompress(comp: &[u8], raw_len: usize) -> Result<Vec<u8>> {
    let mut r = crate::util::Reader::new(comp);
    let ver = r.byte()?;
    if ver != VERSION {
        bail!("dzcm version mismatch: {}", ver);
    }
    let mut cfg =
        CmConfig { table_bits: r.byte()? as u32, match_bits: r.byte()? as u32, stride: 0, bpp: 0 };
    let flags = r.byte()?;
    cfg.stride = r.varint()? as u32;
    cfg.bpp = r.varint()? as u32;
    ensure!((16..=30).contains(&cfg.table_bits), "bad table bits");
    ensure!((16..=30).contains(&cfg.match_bits), "bad match bits");
    ensure!(cfg.stride <= 1 << 20 && cfg.bpp <= 16, "bad stride/bpp");
    let stored_len = r.varint()? as usize;
    ensure!(stored_len == raw_len, "dzcm length mismatch: {} != {}", stored_len, raw_len);
    if raw_len == 0 {
        return Ok(Vec::new());
    }
    let mut model = Model::new(&cfg, raw_len);
    let mut dec = Decoder::new(&comp[r.pos..]);
    for _ in 0..raw_len {
        for _ in 0..8 {
            let p = model.predict();
            let bit = dec.decode(p);
            model.update(bit);
        }
    }
    let mut out = model.buf;
    if flags & FLAG_E8E9 != 0 {
        e8e9_apply(&mut out, false);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(data: &[u8]) {
        let comp = compress(data).unwrap();
        let back = decompress(&comp, data.len()).unwrap();
        assert!(back == data, "roundtrip failed for {} bytes", data.len());
    }

    #[test]
    fn mem_cap_bounds_model() {
        // uncapped: a 100 MB input wants big tables
        let big = CmConfig::for_len_capped(100 << 20, u64::MAX);
        assert_eq!(big.table_bits, 26);
        // 1 GiB cap must fit: NHASH tables of 4B slots + match + slack
        let capped = CmConfig::for_len_capped(100 << 20, 1 << 30);
        assert!(model_mem_bytes(capped.table_bits, capped.match_bits) <= 1 << 30);
        assert!(capped.table_bits < big.table_bits);
        // tiny caps floor at 16 bits instead of underflowing
        let tiny = CmConfig::for_len_capped(100 << 20, 1);
        assert_eq!(tiny.table_bits, 16);
        // capped streams still round-trip
        let s = "compressible text under a small memory cap. ".repeat(5000);
        let comp = compress_with_stride(s.as_bytes(), 0, 0, 64 << 20).unwrap();
        let back = decompress(&comp, s.len()).unwrap();
        assert!(back == s.as_bytes());
    }

    #[test]
    fn state_table_sane() {
        let st = build_states();
        assert!(st.counts.len() <= 256, "too many states: {}", st.counts.len());
        assert_eq!(st.counts[0], (0, 0));
        for s in 0..st.counts.len() {
            for b in 0..2 {
                assert!((st.next[s][b] as usize) < st.counts.len());
            }
        }
    }

    #[test]
    fn e8e9_roundtrip() {
        let mut data = vec![0u8; 100];
        data[10] = 0xE8;
        data[30] = 0xE9;
        data[36] = 0xE8;
        let orig = data.clone();
        e8e9_apply(&mut data, true);
        e8e9_apply(&mut data, false);
        assert_eq!(data, orig);
    }

    #[test]
    fn empty() {
        roundtrip(b"");
    }

    #[test]
    fn tiny() {
        roundtrip(b"a");
        roundtrip(b"ab");
        roundtrip(b"hello world");
    }

    #[test]
    fn repetitive_text() {
        let s = "the quick brown fox jumps over the lazy dog. ".repeat(2000);
        let comp = compress(s.as_bytes()).unwrap();
        assert!(comp.len() < s.len() / 20, "repetitive text should crush: {}", comp.len());
        roundtrip(s.as_bytes());
    }

    #[test]
    fn binary_random() {
        // xorshift pseudo-random: incompressible, must still round-trip
        let mut x = 0x12345678u64;
        let mut v = Vec::new();
        for _ in 0..100_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            v.push(x as u8);
        }
        roundtrip(&v);
    }

    #[test]
    fn all_zeroes() {
        roundtrip(&vec![0u8; 1 << 20]);
    }

    #[test]
    fn x86_like_data() {
        // synthetic "code" with E8 call sites, must round-trip through e8e9
        let mut v = Vec::new();
        let mut x = 7u64;
        for i in 0..300_000u32 {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
            if i % 13 == 0 {
                v.push(0xE8);
                let rel = (x as u32) & 0xffff; // small offset, high byte 00
                v.extend_from_slice(&rel.to_le_bytes());
            } else {
                v.push((x >> 32) as u8 & 0x7f);
            }
        }
        roundtrip(&v);
    }
}
