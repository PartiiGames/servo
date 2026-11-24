/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! A live WebM writer for one VP8 track: the EBML header, a Segment of unknown size (the recording's
//! length isn't known when its first bytes go out, as in Chrome's MediaRecorder), Info, Tracks, then
//! Clusters of known size made of SimpleBlocks. A cluster closes at each keyframe and whenever the
//! recorder hands bytes out (`take`), so every chunk is whole clusters and the chunks concatenate
//! into one playable file. Timestamps in milliseconds.

const EBML: u32 = 0x1A45_DFA3;
const SEGMENT: u32 = 0x1853_8067;
const INFO: u32 = 0x1549_A966;
const TRACKS: u32 = 0x1654_AE6B;
const CLUSTER: u32 = 0x1F43_B675;
const UNKNOWN_SIZE: [u8; 8] = [0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];

fn id(out: &mut Vec<u8>, id: u32) {
    let b = id.to_be_bytes();
    let skip = b.iter().position(|&x| x != 0).unwrap_or(3);
    out.extend_from_slice(&b[skip..]);
}

/// An EBML size as an 8-byte vint.
fn size(out: &mut Vec<u8>, n: u64) {
    let mut b = n.to_be_bytes();
    b[0] = 0x01;
    out.extend_from_slice(&b);
}

fn master(out: &mut Vec<u8>, eid: u32, body: &[u8]) {
    id(out, eid);
    size(out, body.len() as u64);
    out.extend_from_slice(body);
}

fn uint(out: &mut Vec<u8>, eid: u32, v: u64) {
    let b = v.to_be_bytes();
    let skip = b.iter().position(|&x| x != 0).unwrap_or(7);
    id(out, eid);
    out.push(0x80 | (8 - skip) as u8);
    out.extend_from_slice(&b[skip..]);
}

fn string(out: &mut Vec<u8>, eid: u32, s: &str) {
    master(out, eid, s.as_bytes());
}

pub(crate) struct WebmWriter {
    out: Vec<u8>,
    cluster: Vec<u8>,
    cluster_start: Option<i64>,
    pub(crate) frames: u64,
    pub(crate) last_pts: i64,
}

impl WebmWriter {
    /// The header for a `width`x`height` VP8 track.
    pub(crate) fn new(width: u32, height: u32, uid: u64) -> WebmWriter {
        let mut out = Vec::with_capacity(64 * 1024);
        let mut h = Vec::new();
        uint(&mut h, 0x4286, 1); // EBMLVersion
        uint(&mut h, 0x42F7, 1); // EBMLReadVersion
        uint(&mut h, 0x42F2, 4); // EBMLMaxIDLength
        uint(&mut h, 0x42F3, 8); // EBMLMaxSizeLength
        string(&mut h, 0x4282, "webm"); // DocType
        uint(&mut h, 0x4287, 4); // DocTypeVersion
        uint(&mut h, 0x4285, 2); // DocTypeReadVersion
        master(&mut out, EBML, &h);
        id(&mut out, SEGMENT);
        out.extend_from_slice(&UNKNOWN_SIZE);
        let mut info = Vec::new();
        uint(&mut info, 0x2AD7B1, 1_000_000); // TimestampScale: 1 ms
        string(&mut info, 0x4D80, "Partii"); // MuxingApp
        string(&mut info, 0x5741, "Partii"); // WritingApp
        master(&mut out, INFO, &info);
        let mut video = Vec::new();
        uint(&mut video, 0xB0, width as u64); // PixelWidth
        uint(&mut video, 0xBA, height as u64); // PixelHeight
        let mut entry = Vec::new();
        uint(&mut entry, 0xD7, 1); // TrackNumber
        uint(&mut entry, 0x73C5, uid.max(1)); // TrackUID
        uint(&mut entry, 0x83, 1); // TrackType: video
        uint(&mut entry, 0x9C, 0); // FlagLacing
        string(&mut entry, 0x86, "V_VP8"); // CodecID
        master(&mut entry, 0xE0, &video); // Video
        let mut tracks = Vec::new();
        master(&mut tracks, 0xAE, &entry); // TrackEntry
        master(&mut out, TRACKS, &tracks);
        WebmWriter { out, cluster: Vec::new(), cluster_start: None, frames: 0, last_pts: -1 }
    }

    /// One encoded frame at `pts` ms (increasing).
    pub(crate) fn frame(&mut self, pts: i64, key: bool, data: &[u8]) {
        if let Some(start) = self.cluster_start {
            if key || pts - start > 30_000 {
                self.close_cluster();
            }
        }
        let start = *self.cluster_start.get_or_insert_with(|| {
            uint(&mut self.cluster, 0xE7, pts.max(0) as u64); // Timestamp
            pts
        });
        let rel = (pts - start) as i16;
        id(&mut self.cluster, 0xA3); // SimpleBlock
        size(&mut self.cluster, data.len() as u64 + 4);
        self.cluster.push(0x81); // track 1
        self.cluster.extend_from_slice(&rel.to_be_bytes());
        self.cluster.push(if key { 0x80 } else { 0x00 });
        self.cluster.extend_from_slice(data);
        self.frames += 1;
        self.last_pts = pts;
    }

    fn close_cluster(&mut self) {
        if self.cluster_start.take().is_some() {
            let body = std::mem::take(&mut self.cluster);
            master(&mut self.out, CLUSTER, &body);
        }
    }

    /// Everything written since the last take, ending on a whole cluster.
    pub(crate) fn take(&mut self) -> Vec<u8> {
        self.close_cluster();
        std::mem::take(&mut self.out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_and_clusters() {
        let mut w = WebmWriter::new(480, 270, 7);
        let head = w.take();
        assert_eq!(&head[..4], &[0x1A, 0x45, 0xDF, 0xA3]);
        w.frame(0, true, &[1, 2, 3]);
        w.frame(125, false, &[4]);
        let c = w.take();
        assert_eq!(&c[..4], &[0x1F, 0x43, 0xB6, 0x75]);
        // Cluster size = Timestamp (E7 81 00) + two SimpleBlocks (1+8+4+3, 1+8+4+1).
        assert_eq!(u64::from_be_bytes([0, c[5], c[6], c[7], c[8], c[9], c[10], c[11]]), 3 + 16 + 14);
        assert!(w.take().is_empty());
    }
}
