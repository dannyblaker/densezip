//! Shared payload channels. Segment trees reference these; each channel is
//! compressed independently by the best backend.

#[derive(Debug, Clone)]
pub struct PixelBlob {
    pub data: Vec<u8>,
    /// Bytes per scanline (hint for image-aware compression).
    pub stride: u32,
    /// Bytes per pixel unit (hint for image-aware compression).
    pub bpp: u32,
}

pub const PIXEL_IDENTITY: u8 = 0;
/// RGB(A) -> (R-G, G, B-G, A): decorrelates color channels.
pub const PIXEL_SUB_GREEN: u8 = 1;

pub fn sub_green(data: &[u8], bpp: usize, forward: bool) -> Vec<u8> {
    let mut out = data.to_vec();
    if bpp < 3 {
        return out;
    }
    let mut i = 0;
    while i + 2 < out.len() {
        let g = out[i + 1];
        if forward {
            out[i] = out[i].wrapping_sub(g);
            out[i + 2] = out[i + 2].wrapping_sub(g);
        } else {
            out[i] = out[i].wrapping_add(g);
            out[i + 2] = out[i + 2].wrapping_add(g);
        }
        i += bpp;
    }
    out
}

#[derive(Debug, Default)]
pub struct Channels {
    pub plain: Vec<u8>,
    pub corrections: Vec<u8>,
    pub filters: Vec<u8>,
    pub lepton: Vec<u8>,
    pub pixel_blobs: Vec<PixelBlob>,
}

/// Snapshot of channel lengths, used to roll back a failed transform attempt.
#[derive(Debug, Clone, Copy)]
pub struct Mark {
    plain: usize,
    corrections: usize,
    filters: usize,
    lepton: usize,
    pixel_blobs: usize,
    pub segs: usize,
}

impl Channels {
    pub fn mark(&self, segs: usize) -> Mark {
        Mark {
            plain: self.plain.len(),
            corrections: self.corrections.len(),
            filters: self.filters.len(),
            lepton: self.lepton.len(),
            pixel_blobs: self.pixel_blobs.len(),
            segs,
        }
    }

    pub fn rollback(&mut self, m: Mark) {
        self.plain.truncate(m.plain);
        self.corrections.truncate(m.corrections);
        self.filters.truncate(m.filters);
        self.lepton.truncate(m.lepton);
        self.pixel_blobs.truncate(m.pixel_blobs);
    }
}

/// Sequential read cursors over a channel set (rebuild side).
pub struct Cursors<'a> {
    pub ch: &'a Channels,
    pub plain: usize,
    pub corrections: usize,
    pub filters: usize,
    pub lepton: usize,
}

impl<'a> Cursors<'a> {
    pub fn new(ch: &'a Channels) -> Self {
        Cursors {
            ch,
            plain: 0,
            corrections: 0,
            filters: 0,
            lepton: 0,
        }
    }

    pub fn take_plain(&mut self, n: usize) -> anyhow::Result<&'a [u8]> {
        anyhow::ensure!(
            self.plain + n <= self.ch.plain.len(),
            "plain channel underrun"
        );
        let s = &self.ch.plain[self.plain..self.plain + n];
        self.plain += n;
        Ok(s)
    }

    pub fn take_corrections(&mut self, n: usize) -> anyhow::Result<&'a [u8]> {
        anyhow::ensure!(
            self.corrections + n <= self.ch.corrections.len(),
            "corrections channel underrun"
        );
        let s = &self.ch.corrections[self.corrections..self.corrections + n];
        self.corrections += n;
        Ok(s)
    }

    pub fn take_filters(&mut self, n: usize) -> anyhow::Result<&'a [u8]> {
        anyhow::ensure!(
            self.filters + n <= self.ch.filters.len(),
            "filters channel underrun"
        );
        let s = &self.ch.filters[self.filters..self.filters + n];
        self.filters += n;
        Ok(s)
    }

    pub fn take_lepton(&mut self, n: usize) -> anyhow::Result<&'a [u8]> {
        anyhow::ensure!(
            self.lepton + n <= self.ch.lepton.len(),
            "lepton channel underrun"
        );
        let s = &self.ch.lepton[self.lepton..self.lepton + n];
        self.lepton += n;
        Ok(s)
    }
}
