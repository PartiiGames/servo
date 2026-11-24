/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

/* Partii: a small C face on libvpx's VP8 encoder (vpx_codec_enc_cfg_t is large and version-specific;
 * declaring it here, against the vendored headers, keeps Rust off its layout). Realtime, no lag. */

#include <stdlib.h>
#include <string.h>

#include "vpx/vp8cx.h"
#include "vpx/vpx_encoder.h"

typedef struct partii_vp8 {
  vpx_codec_ctx_t ctx;
  vpx_image_t img;
  vpx_codec_iter_t iter;
  unsigned w, h;
} partii_vp8;

/* kbps: target bit rate; threads: encoder threads; cpu_used: VP8E_SET_CPUUSED (higher = faster). */
partii_vp8 *partii_vp8_new(unsigned w, unsigned h, unsigned kbps, unsigned threads, int cpu_used) {
  vpx_codec_enc_cfg_t cfg;
  vpx_codec_iface_t *iface = vpx_codec_vp8_cx();
  if (vpx_codec_enc_config_default(iface, &cfg, 0) != VPX_CODEC_OK) return NULL;
  cfg.g_w = w;
  cfg.g_h = h;
  cfg.g_timebase.num = 1;
  cfg.g_timebase.den = 1000; /* pts in milliseconds */
  cfg.g_threads = threads;
  cfg.g_lag_in_frames = 0;
  cfg.g_error_resilient = 0;
  cfg.g_pass = VPX_RC_ONE_PASS;
  cfg.rc_end_usage = VPX_VBR;
  cfg.rc_target_bitrate = kbps;
  cfg.rc_min_quantizer = 4;
  cfg.rc_max_quantizer = 56;
  cfg.rc_dropframe_thresh = 0;
  cfg.kf_mode = VPX_KF_AUTO;
  cfg.kf_min_dist = 0;
  cfg.kf_max_dist = 9999; /* the caller forces keyframes */
  partii_vp8 *e = calloc(1, sizeof(partii_vp8));
  if (!e) return NULL;
  if (vpx_codec_enc_init(&e->ctx, iface, &cfg, 0) != VPX_CODEC_OK) {
    free(e);
    return NULL;
  }
  vpx_codec_control(&e->ctx, VP8E_SET_CPUUSED, cpu_used);
  vpx_codec_control(&e->ctx, VP8E_SET_NOISE_SENSITIVITY, 0);
  vpx_codec_control(&e->ctx, VP8E_SET_STATIC_THRESHOLD, 1);
  vpx_codec_control(&e->ctx, VP8E_SET_TOKEN_PARTITIONS, 0);
  e->w = w;
  e->h = h;
  return e;
}

/* i420: w*h Y, then (w/2)*(h/2) U and V (w and h even). NULL flushes. Returns 0 on success. */
int partii_vp8_encode(partii_vp8 *e, const unsigned char *i420, long long pts, unsigned long duration,
                      int force_keyframe) {
  vpx_image_t *img = NULL;
  if (i420) {
    if (!vpx_img_wrap(&e->img, VPX_IMG_FMT_I420, e->w, e->h, 1, (unsigned char *)i420)) return -1;
    img = &e->img;
  }
  e->iter = NULL;
  vpx_codec_err_t r = vpx_codec_encode(&e->ctx, img, pts, duration,
                                       force_keyframe ? VPX_EFLAG_FORCE_KF : 0, VPX_DL_REALTIME);
  return r == VPX_CODEC_OK ? 0 : -(int)r - 1;
}

/* The next encoded frame since the last encode: 1 with buf/size/pts/key set, 0 when there's none left. */
int partii_vp8_next(partii_vp8 *e, const unsigned char **buf, size_t *size, long long *pts, int *key) {
  const vpx_codec_cx_pkt_t *pkt;
  while ((pkt = vpx_codec_get_cx_data(&e->ctx, &e->iter)) != NULL) {
    if (pkt->kind != VPX_CODEC_CX_FRAME_PKT) continue;
    *buf = pkt->data.frame.buf;
    *size = pkt->data.frame.sz;
    *pts = pkt->data.frame.pts;
    *key = (pkt->data.frame.flags & VPX_FRAME_IS_KEY) ? 1 : 0;
    return 1;
  }
  return 0;
}

const char *partii_vp8_error(partii_vp8 *e) {
  const char *d = vpx_codec_error_detail(&e->ctx);
  return d ? d : vpx_codec_error(&e->ctx);
}

void partii_vp8_free(partii_vp8 *e) {
  if (!e) return;
  vpx_codec_destroy(&e->ctx);
  free(e);
}
