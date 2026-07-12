use anyhow::{bail, Result};

pub fn write_varint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

pub struct Reader<'a> {
    pub data: &'a [u8],
    pub pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Reader { data, pos: 0 }
    }

    pub fn varint(&mut self) -> Result<u64> {
        let mut v: u64 = 0;
        let mut shift = 0;
        loop {
            if self.pos >= self.data.len() {
                bail!("truncated varint");
            }
            let b = self.data[self.pos];
            self.pos += 1;
            if shift >= 64 {
                bail!("varint overflow");
            }
            v |= ((b & 0x7f) as u64) << shift;
            if b & 0x80 == 0 {
                return Ok(v);
            }
            shift += 7;
        }
    }

    pub fn byte(&mut self) -> Result<u8> {
        if self.pos >= self.data.len() {
            bail!("truncated byte");
        }
        let b = self.data[self.pos];
        self.pos += 1;
        Ok(b)
    }

    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.pos + n > self.data.len() {
            bail!("truncated bytes: want {} have {}", n, self.data.len() - self.pos);
        }
        let s = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
}

/// Adler-32 checksum (as used by zlib).
pub fn adler32(data: &[u8]) -> u32 {
    const MOD: u32 = 65521;
    let mut a: u32 = 1;
    let mut b: u32 = 0;
    for chunk in data.chunks(5552) {
        for &x in chunk {
            a += x as u32;
            b += a;
        }
        a %= MOD;
        b %= MOD;
    }
    (b << 16) | a
}

pub fn xxh3(data: &[u8]) -> u64 {
    xxhash_rust::xxh3::xxh3_64(data)
}

/// Available RAM in bytes. Linux: MemAvailable from /proc/meminfo;
/// elsewhere a conservative 8 GiB default (override with --mem).
pub fn available_ram_bytes() -> u64 {
    if let Ok(s) = std::fs::read_to_string("/proc/meminfo") {
        for line in s.lines() {
            if let Some(rest) = line.strip_prefix("MemAvailable:") {
                if let Some(kb) =
                    rest.trim().split_whitespace().next().and_then(|v| v.parse::<u64>().ok())
                {
                    return kb * 1024;
                }
            }
        }
    }
    8 << 30
}

pub fn human(n: u64) -> String {
    if n >= 10 * 1024 * 1024 {
        format!("{:.1} MiB", n as f64 / (1024.0 * 1024.0))
    } else if n >= 10 * 1024 {
        format!("{:.1} KiB", n as f64 / 1024.0)
    } else {
        format!("{} B", n)
    }
}
