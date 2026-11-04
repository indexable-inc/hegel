//! One regular file's three images (page cache, device cache, media) and how a
//! crash resolves them.

use std::collections::BTreeSet;
use std::fmt::Write;

use super::buf::Buf;
use super::{Env, Errno, PAGE};
use crate::Fault;

/// Where a sync stops.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Level {
    /// The device's volatile cache: survives an OS crash, not power loss.
    Dev,
    /// Stable media: survives power loss.
    Media,
}

#[derive(Debug)]
pub(crate) struct FileData {
    pub(crate) cache: Buf,
    pub(crate) dev: Vec<u8>,
    pub(crate) media: Vec<u8>,
    /// Sizes since the device image was last synced; `[0]` is its size.
    pub(crate) hist_dev: Vec<u64>,
    /// Sizes since the media image was last synced; `[0]` is its size.
    pub(crate) hist_media: Vec<u64>,
    /// Pages a writeback error marked clean although media never got them
    /// (mm/filemap.c, `filemap_check_errors`; the "fsyncgate" behaviour).
    pub(crate) stale: BTreeSet<u64>,
    /// errseq_t counter: bumped on each writeback error (lib/errseq.c).
    pub(crate) wb_err: u32,
    pub(crate) wb_err_code: Errno,
    pub(crate) wb_seen: bool,
}

fn page_range(p: u64, len: usize) -> std::ops::Range<usize> {
    let start = (p * PAGE) as usize;
    start.min(len)..((p + 1) * PAGE).min(len as u64) as usize
}

fn page_of(img: &[u8], p: u64, upto: usize) -> Vec<u8> {
    let start = (p * PAGE) as usize;
    let end = (((p + 1) * PAGE) as usize).min(upto);
    let mut out = vec![0u8; end.saturating_sub(start)];
    if start < img.len() {
        let n = (img.len().min(end)) - start;
        out[..n].copy_from_slice(&img[start..start + n]);
    }
    out
}

fn pages(len: usize) -> u64 {
    (len as u64).div_ceil(PAGE)
}

impl FileData {
    pub(crate) fn new() -> FileData {
        FileData::from_image(Vec::new())
    }

    pub(crate) fn from_image(img: Vec<u8>) -> FileData {
        let len = img.len() as u64;
        FileData {
            cache: Buf::from_vec(img.clone()),
            dev: img.clone(),
            media: img,
            hist_dev: vec![len],
            hist_media: vec![len],
            stale: BTreeSet::new(),
            wb_err: 0,
            wb_err_code: Errno::EIO,
            wb_seen: true,
        }
    }

    pub(crate) fn size(&self) -> u64 {
        self.cache.len() as u64
    }

    fn note_size(&mut self) {
        let s = self.size();
        for h in [&mut self.hist_dev, &mut self.hist_media] {
            if h.last() != Some(&s) {
                h.push(s);
            }
        }
    }

    pub(crate) fn set_size(&mut self, len: u64) -> Result<(), Errno> {
        self.cache.set_len(len as usize)?;
        self.note_size();
        Ok(())
    }

    pub(crate) fn write(&mut self, offset: u64, data: &[u8]) -> Result<(), Errno> {
        self.cache.write_at(offset as usize, data)?;
        if !data.is_empty() {
            for p in offset / PAGE..=(offset + data.len() as u64 - 1) / PAGE {
                self.stale.remove(&p);
            }
        }
        self.note_size();
        Ok(())
    }

    /// Pages whose page cache differs from `img` and are not stale-clean.
    pub(crate) fn dirty_pages(&self, img: &[u8]) -> Vec<u64> {
        let cache = self.cache.bytes();
        let n = pages(cache.len()).max(pages(img.len()));
        (0..n)
            .filter(|p| !self.stale.contains(p))
            .filter(|&p| {
                page_of(cache, p, cache.len().max(img.len()))
                    != page_of(img, p, cache.len().max(img.len()))
                    || page_range(p, cache.len()) != page_range(p, img.len())
            })
            .collect()
    }

    pub(crate) fn is_dirty(&self) -> bool {
        self.size() != self.media.len() as u64
            || !self.dirty_pages(&self.media).is_empty()
            || self.size() != self.dev.len() as u64
            || !self.dirty_pages(&self.dev).is_empty()
    }

    /// Copy `pages` (all dirty pages when `None`) and the size from the page
    /// cache to `level`. Stale-clean pages keep the old image's bytes.
    pub(crate) fn sync(&mut self, level: Level, which: Option<&[u64]>) {
        let size = self.cache.len();
        let cache = self.cache.to_vec();
        let mut images: Vec<&mut Vec<u8>> = match level {
            Level::Dev => vec![&mut self.dev],
            Level::Media => vec![&mut self.dev, &mut self.media],
        };
        for img in images.iter_mut() {
            let old_len = img.len();
            let mut next = vec![0u8; size];
            let keep = old_len.min(size);
            next[..keep].copy_from_slice(&img[..keep]);
            let chosen: Vec<u64> = match which {
                None => (0..pages(size)).collect(),
                // O_DSYNC and msync persist their range, and under ordered-mode
                // semantics a size extension carries the data it covers
                // (Documentation/filesystems/ext4.rst, "data=ordered").
                Some(ps) => {
                    let mut v: Vec<u64> = ps.to_vec();
                    v.extend((old_len as u64 / PAGE)..pages(size));
                    v
                }
            };
            for p in chosen {
                if self.stale.contains(&p) {
                    continue;
                }
                let r = page_range(p, size);
                next[r.clone()].copy_from_slice(&cache[r]);
            }
            **img = next;
        }
        let s = size as u64;
        self.hist_dev = vec![s];
        if level == Level::Media {
            self.hist_media = vec![s];
        }
    }

    /// A writeback error during a sync: each dirty page is drawn written or
    /// dropped, and both end clean (Documentation/filesystems/vfs.rst, "Handling
    /// errors during writeback"); the error is recorded in the errseq counter.
    pub(crate) fn fail_writeback(&mut self, level: Level, code: Errno, env: &mut Env<'_>) -> String {
        let dirty = self.dirty_pages(match level {
            Level::Dev => &self.dev,
            Level::Media => &self.media,
        });
        let mut written = Vec::new();
        let mut dropped = Vec::new();
        for p in dirty {
            if env.choices.coin("kernel.wb_error_page_written", 0.5) {
                written.push(p);
            } else {
                dropped.push(p);
            }
        }
        let old_hist = (self.hist_dev.clone(), self.hist_media.clone());
        self.sync(level, Some(&written));
        if !dropped.is_empty() {
            // The size update may not have been written either.
            self.hist_dev = old_hist.0;
            self.hist_media = old_hist.1;
            self.note_size();
        }
        for p in &dropped {
            self.stale.insert(*p);
        }
        self.wb_err = self.wb_err.wrapping_add(1);
        self.wb_err_code = code;
        self.wb_seen = false;
        format!("pages written {written:?}, dropped {dropped:?}")
    }

    /// Whether some crash [`resolve`](Self::resolve) can draw leaves exactly
    /// `observed`, with the same candidates per page and the same tearing at
    /// `aw_unit`. `Err` names the first size or page no draw produces.
    pub(crate) fn could_resolve_to(&self, power_loss: bool, zeros_past_eof: bool, torn: bool, aw_unit: u64, observed: &[u8]) -> Result<(), String> {
        let (base, hist, mid): (&[u8], &[u64], Option<&[u8]>) = if power_loss {
            let mid = (self.dev != self.media).then_some(self.dev.as_slice());
            (&self.media, &self.hist_media, mid)
        } else {
            (&self.dev, &self.hist_dev, None)
        };
        let size = observed.len();
        if !hist.contains(&(size as u64)) {
            return Err(format!("size {size} is not one the file had since its last sync ({hist:?})"));
        }
        let cache = self.cache.bytes();
        for p in 0..pages(size) {
            let r = page_range(p, size);
            let want = &observed[r.clone()];
            let old = page_of(base, p, size);
            let new = if self.stale.contains(&p) { old.clone() } else { page_of(cache, p, size) };
            let beyond_old = p * PAGE >= base.len() as u64;
            let mut cands: Vec<Vec<u8>> = Vec::new();
            if beyond_old {
                cands.push(new);
                cands.extend(mid.map(|m| page_of(m, p, size)));
                if zeros_past_eof {
                    cands.push(vec![0u8; r.len()]);
                }
            } else {
                cands.push(old.clone());
                cands.extend(mid.map(|m| page_of(m, p, size)));
                cands.push(new);
            }
            if cands.iter().any(|c| c.as_slice() == want) {
                continue;
            }
            let unit = aw_unit as usize;
            let tears = |c: &Vec<u8>| {
                c.chunks(unit).zip(want.chunks(unit)).zip(old.chunks(unit)).all(|((n, w), o)| n == w || o == w)
            };
            if torn && !beyond_old && cands.iter().skip(1).any(tears) {
                continue;
            }
            return Err(format!("page {p} matches no version the file had (nor a tear of one at {aw_unit} bytes)"));
        }
        Ok(())
    }

    /// The bytes this file holds after a crash that resolves from `baseline`
    /// (media for power loss, the device cache for an OS crash).
    pub(crate) fn resolve(&self, power_loss: bool, env: &mut Env<'_>, aw_unit: u64) -> (Vec<u8>, String) {
        let (base, hist, mid): (&[u8], &[u64], Option<&[u8]>) = if power_loss {
            let mid = (self.dev != self.media).then_some(self.dev.as_slice());
            (&self.media, &self.hist_media, mid)
        } else {
            (&self.dev, &self.hist_dev, None)
        };
        let cache = self.cache.bytes();
        let size = hist[env.choices.int("crash.size", 0, hist.len() as u64 - 1) as usize] as usize;
        let mut out = vec![0u8; size];
        let mut summary = String::new();
        if size != base.len() {
            let _ = write!(summary, "size {}->{} (of {:?})", base.len(), size, hist);
        }
        let mut lost = Vec::new();
        let mut kept = Vec::new();
        let mut torn = Vec::new();
        let mut zeros = Vec::new();
        for p in 0..pages(size) {
            let r = page_range(p, size);
            let old = page_of(base, p, size);
            let new = if self.stale.contains(&p) {
                old.clone()
            } else {
                page_of(cache, p, size)
            };
            let beyond_old = p * PAGE >= base.len() as u64;
            let mut cands: Vec<Vec<u8>> = Vec::new();
            let mut push = |v: Vec<u8>| {
                if !cands.contains(&v) {
                    cands.push(v);
                }
            };
            if beyond_old {
                push(new.clone());
                if let Some(m) = mid {
                    push(page_of(m, p, size));
                }
                if env.swarm.on(Fault::ZerosPastEof) {
                    push(vec![0u8; r.len()]);
                }
            } else {
                push(old.clone());
                if let Some(m) = mid {
                    push(page_of(m, p, size));
                }
                push(new.clone());
            }
            let idx = match cands.len() {
                1 => 0,
                2 if !beyond_old => usize::from(env.choices.coin("crash.page_written", env.swarm.rates.page_written)),
                n => env.choices.int("crash.page_version", 0, n as u64 - 1) as usize,
            };
            let mut chosen = cands[idx].clone();
            if !beyond_old && idx > 0 {
                if env.swarm.fire(env.choices, Fault::TornWrites, env.swarm.rates.torn) {
                    let unit = aw_unit as usize;
                    for (i, chunk) in chosen.chunks_mut(unit).enumerate() {
                        if !env.choices.coin("crash.torn_sector_written", 0.5) {
                            let s = i * unit;
                            chunk.copy_from_slice(&old[s..s + chunk.len()]);
                        }
                    }
                    torn.push(p);
                } else {
                    kept.push(p);
                }
            } else if !beyond_old && cands.len() > 1 {
                lost.push(p);
            } else if beyond_old && chosen.iter().all(|b| *b == 0) && new.iter().any(|b| *b != 0) {
                zeros.push(p);
            }
            out[r].copy_from_slice(&chosen);
        }
        for (label, v) in [("lost", &lost), ("written", &kept), ("torn", &torn), ("zeroed", &zeros)] {
            if !v.is_empty() {
                let _ = write!(summary, "{}pages {label} {v:?}", if summary.is_empty() { "" } else { ", " });
            }
        }
        (out, summary)
    }
}
