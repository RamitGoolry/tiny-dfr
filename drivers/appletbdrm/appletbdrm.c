// SPDX-License-Identifier: GPL-2.0
/*
 * Apple Touch Bar DRM Driver
 *
 * Copyright (c) 2023 Kerem Karabay <kekrby@gmail.com>
 */

#include <linux/align.h>
#include <linux/array_size.h>
#include <linux/bitops.h>
#include <linux/bug.h>
#include <linux/container_of.h>
#include <linux/err.h>
#include <linux/limits.h>
#include <linux/minmax.h>
#include <linux/module.h>
#include <linux/mutex.h>
#include <linux/overflow.h>
#include <linux/slab.h>
#include <linux/types.h>
#include <linux/unaligned.h>
#include <linux/usb.h>
#include <linux/workqueue.h>

#include <drm/drm_atomic.h>
#include <drm/drm_atomic_helper.h>
#include <drm/drm_crtc.h>
#include <drm/drm_damage_helper.h>
#include <drm/drm_drv.h>
#include <drm/drm_encoder.h>
#include <drm/drm_format_helper.h>
#include <drm/drm_fourcc.h>
#include <drm/drm_framebuffer.h>
#include <drm/drm_gem_atomic_helper.h>
#include <drm/drm_gem_framebuffer_helper.h>
#include <drm/drm_gem_shmem_helper.h>
#include <drm/drm_plane.h>
#include <drm/drm_print.h>
#include <drm/drm_probe_helper.h>

#define APPLETBDRM_PIXEL_FORMAT		cpu_to_le32(0x52474241) /* RGBA, the actual format is BGR888 */
#define APPLETBDRM_BITS_PER_PIXEL	24

#define APPLETBDRM_MSG_CLEAR_DISPLAY	cpu_to_le32(0x434c5244) /* CLRD */
#define APPLETBDRM_MSG_GET_INFORMATION	cpu_to_le32(0x47494e46) /* GINF */
#define APPLETBDRM_MSG_UPDATE_COMPLETE	cpu_to_le32(0x5544434c) /* UDCL */
#define APPLETBDRM_MSG_SIGNAL_READINESS	cpu_to_le32(0x52454459) /* REDY */

#define APPLETBDRM_BULK_MSG_TIMEOUT	1000
/*
 * The per-frame UPDATE_COMPLETE ack normally arrives at ~1s; give the off-path
 * worker read a wide margin so a near-miss doesn't time out and desync the pipe.
 */
#define APPLETBDRM_ACK_TIMEOUT		3000
/*
 * Cap the device update rate (~30fps). Firing frames back-to-back overruns the
 * device's request/response pipe; the stock driver was paced by the render loop.
 */
#define APPLETBDRM_FLUSH_INTERVAL_MS	33
/* Drain the device's stale ack backlog so the response pipe never chokes. */
#define APPLETBDRM_DRIFT_DRAIN_MS	200	/* drain when this far behind on acks */
#define APPLETBDRM_DRAIN_TIMEOUT	50	/* ms; short — only mops already-queued acks */
#define APPLETBDRM_DRAIN_MAX		8192	/* safety cap on one drain pass */

#define drm_to_adev(_drm)		container_of(_drm, struct appletbdrm_device, drm)
#define adev_to_udev(adev)		interface_to_usbdev(to_usb_interface((adev)->drm.dev))

struct appletbdrm_msg_request_header {
	__le16 unk_00;
	__le16 unk_02;
	__le32 unk_04;
	__le32 unk_08;
	__le32 size;
} __packed;

struct appletbdrm_msg_response_header {
	u8 unk_00[16];
	__le32 msg;
} __packed;

struct appletbdrm_msg_simple_request {
	struct appletbdrm_msg_request_header header;
	__le32 msg;
	u8 unk_14[8];
	__le32 size;
} __packed;

struct appletbdrm_msg_information {
	struct appletbdrm_msg_response_header header;
	u8 unk_14[12];
	__le32 width;
	__le32 height;
	u8 bits_per_pixel;
	__le32 bytes_per_row;
	__le32 orientation;
	__le32 bitmap_info;
	__le32 pixel_format;
	__le32 width_inches;	/* floating point */
	__le32 height_inches;	/* floating point */
} __packed;

struct appletbdrm_frame {
	__le16 begin_x;
	__le16 begin_y;
	__le16 width;
	__le16 height;
	__le32 buf_size;
	u8 buf[];
} __packed;

struct appletbdrm_fb_request_footer {
	u8 unk_00[12];
	__le32 unk_0c;
	u8 unk_10[12];
	__le32 unk_1c;
	__le64 timestamp;
	u8 unk_28[12];
	__le32 unk_34;
	u8 unk_38[20];
	__le32 unk_4c;
} __packed;

struct appletbdrm_fb_request {
	struct appletbdrm_msg_request_header header;
	__le16 unk_10;
	u8 msg_id;
	u8 unk_13[29];
	/*
	 * Contents of `data`:
	 * - struct appletbdrm_frame frames[];
	 * - struct appletbdrm_fb_request_footer footer;
	 * - padding to make the total size a multiple of 16
	 */
	u8 data[];
} __packed;

struct appletbdrm_fb_request_response {
	struct appletbdrm_msg_response_header header;
	u8 unk_14[12];
	__le64 timestamp;
} __packed;

struct appletbdrm_device {
	unsigned int in_ep;
	unsigned int out_ep;

	unsigned int width;
	unsigned int height;

	struct drm_device drm;
	struct drm_display_mode mode;
	struct drm_connector connector;
	struct drm_plane primary_plane;
	struct drm_crtc crtc;
	struct drm_encoder encoder;

	/*
	 * The per-frame UPDATE_COMPLETE ack (read in appletbdrm_read_response)
	 * can block for up to APPLETBDRM_BULK_MSG_TIMEOUT ms, which must not sit
	 * on the atomic commit / DIRTYFB critical path. The atomic update copies
	 * the damaged pixels into shadow_buf (a full frame in BGR888), unions the
	 * dirty rect into damage, and hands the slow USB send+ack to flush_work.
	 */
	struct delayed_work flush_work;
	struct mutex damage_lock; /* protects damage */
	struct drm_rect damage; /* accumulated dirty rect awaiting a send */
	void *shadow_buf; /* full frame in BGR888, lazily allocated */
	size_t shadow_pitch;
	struct appletbdrm_fb_request_response *response; /* worker-owned ack */
	u64 flush_seq; /* per-frame counter for the flush instrumentation log */
};

/*
 * Runtime toggle for per-frame flush instrumentation. Off by default; enable with
 *   echo 1 | sudo tee /sys/module/appletbdrm/parameters/flush_log
 * Logs each queue + flush with timing and the ack offset (drift), then disable to
 * stop the dmesg flood. Reusable for any future display-pipeline debugging.
 */
static bool flush_log;
module_param(flush_log, bool, 0644);
MODULE_PARM_DESC(flush_log, "Log every frame queue + flush with timing and ack offset");

static int appletbdrm_send_request(struct appletbdrm_device *adev,
				   struct appletbdrm_msg_request_header *request, size_t size)
{
	struct usb_device *udev = adev_to_udev(adev);
	struct drm_device *drm = &adev->drm;
	int ret, actual_size;

	ret = usb_bulk_msg(udev, usb_sndbulkpipe(udev, adev->out_ep),
			   request, size, &actual_size, APPLETBDRM_BULK_MSG_TIMEOUT);
	if (ret) {
		drm_err(drm, "Failed to send message (%d)\n", ret);
		return ret;
	}

	if (actual_size != size) {
		drm_err(drm, "Actual size (%d) doesn't match expected size (%zu)\n",
			actual_size, size);
		return -EIO;
	}

	return 0;
}

static int appletbdrm_read_response(struct appletbdrm_device *adev,
				    struct appletbdrm_msg_response_header *response,
				    size_t size, __le32 expected_response,
				    unsigned int timeout)
{
	struct usb_device *udev = adev_to_udev(adev);
	struct drm_device *drm = &adev->drm;
	int ret, actual_size;
	bool readiness_signal_received = false;

retry:
	ret = usb_bulk_msg(udev, usb_rcvbulkpipe(udev, adev->in_ep),
			   response, size, &actual_size, timeout);
	if (ret) {
		drm_err(drm, "Failed to read response (%d)\n", ret);
		return ret;
	}

	/*
	 * The device responds to the first request sent in a particular
	 * timeframe after the USB device configuration is set with a readiness
	 * signal, in which case the response should be read again
	 */
	if (response->msg == APPLETBDRM_MSG_SIGNAL_READINESS) {
		if (!readiness_signal_received) {
			readiness_signal_received = true;
			goto retry;
		}

		drm_err(drm, "Encountered unexpected readiness signal\n");
		return -EINTR;
	}

	if (actual_size != size) {
		drm_err(drm, "Actual size (%d) doesn't match expected size (%zu)\n",
			actual_size, size);
		return -EBADMSG;
	}

	if (response->msg != expected_response) {
		drm_err(drm, "Unexpected response from device (expected %p4cl found %p4cl)\n",
			&expected_response, &response->msg);
		return -EIO;
	}

	return 0;
}

/*
 * Drain (and discard) any responses the device has already queued, until the IN
 * endpoint runs dry. Clears a stale-ack backlog so the worker never falls
 * permanently behind — a deep backlog is what eventually chokes the device's pipe
 * (garbage short acks, then -110 send timeouts). Returns the count drained.
 */
static int appletbdrm_drain_responses(struct appletbdrm_device *adev)
{
	struct usb_device *udev = adev_to_udev(adev);
	int ret, drained = 0;

	while (drained < APPLETBDRM_DRAIN_MAX) {
		ret = usb_bulk_msg(udev, usb_rcvbulkpipe(udev, adev->in_ep),
				   adev->response, sizeof(*adev->response),
				   NULL, APPLETBDRM_DRAIN_TIMEOUT);
		if (ret)
			break;	/* -ETIMEDOUT (empty) or any error: stop */
		drained++;
	}

	return drained;
}

static int appletbdrm_send_msg(struct appletbdrm_device *adev, __le32 msg)
{
	struct appletbdrm_msg_simple_request *request;
	int ret;

	request = kzalloc_obj(*request);
	if (!request)
		return -ENOMEM;

	request->header.unk_00 = cpu_to_le16(2);
	request->header.unk_02 = cpu_to_le16(0x1512);
	request->header.size = cpu_to_le32(sizeof(*request) - sizeof(request->header));
	request->msg = msg;
	request->size = request->header.size;

	ret = appletbdrm_send_request(adev, &request->header, sizeof(*request));

	kfree(request);

	return ret;
}

static int appletbdrm_clear_display(struct appletbdrm_device *adev)
{
	return appletbdrm_send_msg(adev, APPLETBDRM_MSG_CLEAR_DISPLAY);
}

static int appletbdrm_signal_readiness(struct appletbdrm_device *adev)
{
	return appletbdrm_send_msg(adev, APPLETBDRM_MSG_SIGNAL_READINESS);
}

static int appletbdrm_get_information(struct appletbdrm_device *adev)
{
	struct appletbdrm_msg_information *info;
	struct drm_device *drm = &adev->drm;
	u8 bits_per_pixel;
	__le32 pixel_format;
	int ret;

	info = kzalloc_obj(*info);
	if (!info)
		return -ENOMEM;

	ret = appletbdrm_send_msg(adev, APPLETBDRM_MSG_GET_INFORMATION);
	if (ret)
		return ret;

	ret = appletbdrm_read_response(adev, &info->header, sizeof(*info),
				       APPLETBDRM_MSG_GET_INFORMATION, APPLETBDRM_BULK_MSG_TIMEOUT);
	if (ret)
		goto free_info;

	bits_per_pixel = info->bits_per_pixel;
	pixel_format = get_unaligned(&info->pixel_format);

	adev->width = get_unaligned_le32(&info->width);
	adev->height = get_unaligned_le32(&info->height);

	if (bits_per_pixel != APPLETBDRM_BITS_PER_PIXEL) {
		drm_err(drm, "Encountered unexpected bits per pixel value (%d)\n", bits_per_pixel);
		ret = -EINVAL;
		goto free_info;
	}

	if (pixel_format != APPLETBDRM_PIXEL_FORMAT) {
		drm_err(drm, "Encountered unknown pixel format (%p4cl)\n", &pixel_format);
		ret = -EINVAL;
		goto free_info;
	}

free_info:
	kfree(info);

	return ret;
}

static u32 rect_size(struct drm_rect *rect)
{
	return drm_rect_width(rect) * drm_rect_height(rect) *
		(BITS_TO_BYTES(APPLETBDRM_BITS_PER_PIXEL));
}

static void appletbdrm_damage_init(struct drm_rect *r)
{
	*r = (struct drm_rect){ .x1 = INT_MAX, .y1 = INT_MAX, .x2 = INT_MIN, .y2 = INT_MIN };
}

static bool appletbdrm_damage_empty(const struct drm_rect *r)
{
	return r->x1 >= r->x2 || r->y1 >= r->y2;
}

static void appletbdrm_damage_add(struct drm_rect *r, const struct drm_rect *d)
{
	r->x1 = min(r->x1, d->x1);
	r->y1 = min(r->y1, d->y1);
	r->x2 = max(r->x2, d->x2);
	r->y2 = max(r->y2, d->y2);
}

static int appletbdrm_connector_helper_get_modes(struct drm_connector *connector)
{
	struct appletbdrm_device *adev = drm_to_adev(connector->dev);

	return drm_connector_helper_get_modes_fixed(connector, &adev->mode);
}

static const u32 appletbdrm_primary_plane_formats[] = {
	DRM_FORMAT_BGR888,
	DRM_FORMAT_XRGB8888, /* emulated */
};

static int appletbdrm_primary_plane_helper_atomic_check(struct drm_plane *plane,
						   struct drm_atomic_state *state)
{
	struct drm_plane_state *new_plane_state = drm_atomic_get_new_plane_state(state, plane);
	struct drm_crtc *new_crtc = new_plane_state->crtc;
	struct drm_crtc_state *new_crtc_state = NULL;
	int ret;

	if (new_crtc)
		new_crtc_state = drm_atomic_get_new_crtc_state(state, new_crtc);

	ret = drm_atomic_helper_check_plane_state(new_plane_state, new_crtc_state,
						  DRM_PLANE_NO_SCALING,
						  DRM_PLANE_NO_SCALING,
						  false, false);
	if (ret)
		return ret;

	return 0;
}

/*
 * Synchronous part of a flush: copy the damaged pixels into the driver-owned
 * shadow buffer (in BGR888, full-frame), accumulate the dirty rect and kick the
 * worker. No USB I/O happens here, so the atomic commit / DIRTYFB ioctl returns
 * as soon as the pixels are copied instead of blocking on the per-frame ack.
 */
static int appletbdrm_flush_damage(struct appletbdrm_device *adev,
				   struct drm_plane_state *old_state,
				   struct drm_plane_state *state)
{
	struct drm_shadow_plane_state *shadow_plane_state = to_drm_shadow_plane_state(state);
	struct drm_atomic_helper_damage_iter iter;
	struct drm_framebuffer *fb = state->fb;
	struct drm_device *drm = &adev->drm;
	struct drm_rect frame_damage;
	unsigned int dst_pitch;
	struct drm_rect damage;
	bool dirty = false;
	int ret;

	ret = drm_gem_fb_begin_cpu_access(fb, DMA_FROM_DEVICE);
	if (ret) {
		drm_err(drm, "Failed to start CPU framebuffer access (%d)\n", ret);
		return ret;
	}

	if (!adev->shadow_buf) {
		adev->shadow_pitch = fb->width * BITS_TO_BYTES(APPLETBDRM_BITS_PER_PIXEL);
		adev->shadow_buf = kvzalloc(adev->shadow_pitch * fb->height, GFP_KERNEL);
		if (!adev->shadow_buf) {
			ret = -ENOMEM;
			goto end_fb_cpu_access;
		}
	}

	dst_pitch = adev->shadow_pitch;
	appletbdrm_damage_init(&frame_damage);

	drm_atomic_helper_damage_iter_init(&iter, old_state, state);
	drm_atomic_for_each_plane_damage(&iter, &damage) {
		struct drm_rect dst_clip = state->dst;
		struct iosys_map dst = IOSYS_MAP_INIT_VADDR((u8 *)adev->shadow_buf +
			damage.y1 * adev->shadow_pitch +
			damage.x1 * BITS_TO_BYTES(APPLETBDRM_BITS_PER_PIXEL));

		if (!drm_rect_intersect(&dst_clip, &damage))
			continue;

		switch (fb->format->format) {
		case DRM_FORMAT_XRGB8888:
			drm_fb_xrgb8888_to_bgr888(&dst, &dst_pitch, &shadow_plane_state->data[0], fb, &damage, &shadow_plane_state->fmtcnv_state);
			break;
		default:
			drm_fb_memcpy(&dst, &dst_pitch, &shadow_plane_state->data[0], fb, &damage);
			break;
		}

		appletbdrm_damage_add(&frame_damage, &damage);
		dirty = true;
	}

end_fb_cpu_access:
	drm_gem_fb_end_cpu_access(fb, DMA_FROM_DEVICE);

	if (ret || !dirty)
		return ret;

	mutex_lock(&adev->damage_lock);
	appletbdrm_damage_add(&adev->damage, &frame_damage);
	mutex_unlock(&adev->damage_lock);

	queue_delayed_work(system_long_wq, &adev->flush_work,
			   msecs_to_jiffies(APPLETBDRM_FLUSH_INTERVAL_MS));

	return 0;
}

/*
 * Slow part of a flush, off the commit critical path: build one coalesced frame
 * from the shadow buffer and run the full USB send + UPDATE_COMPLETE ack. Handles
 * ONE frame per run and re-queues itself with a delay, so the device is never
 * driven faster than ~30fps — firing frames back-to-back overruns its
 * request/response pipe (the stock driver was paced by the render loop).
 */
static void appletbdrm_flush_work(struct work_struct *work)
{
	struct appletbdrm_device *adev = container_of(to_delayed_work(work),
						      struct appletbdrm_device, flush_work);
	struct appletbdrm_fb_request_response *response = adev->response;
	struct appletbdrm_fb_request_footer *footer;
	struct appletbdrm_fb_request *request;
	struct drm_device *drm = &adev->drm;
	struct appletbdrm_frame *frame;
	u64 timestamp = ktime_get_ns();
	u64 t_send, t_done, wall;
	unsigned int width, height, y;
	size_t frames_size, request_size;
	const u8 *src;
	u8 *dst;
	struct drm_rect r;
	u32 buf_size;
	s64 off_ms = -1;
	int send_ret = 0, read_ret = -1, drained = 0;
	bool more;
	int idx;

	if (!drm_dev_enter(drm, &idx))
		return;

	mutex_lock(&adev->damage_lock);
	r = adev->damage;
	appletbdrm_damage_init(&adev->damage);
	mutex_unlock(&adev->damage_lock);

	if (appletbdrm_damage_empty(&r))
		goto out;

	width = drm_rect_width(&r);
	height = drm_rect_height(&r);
	buf_size = rect_size(&r);

	frames_size = struct_size((struct appletbdrm_frame *)0, buf, buf_size);
	request_size = ALIGN(sizeof(struct appletbdrm_fb_request) +
			     frames_size +
			     sizeof(struct appletbdrm_fb_request_footer), 16);

	request = kzalloc(request_size, GFP_KERNEL);
	if (!request)
		goto out;

	request->header.unk_00 = cpu_to_le16(2);
	request->header.unk_02 = cpu_to_le16(0x12);
	request->header.unk_04 = cpu_to_le32(9);
	request->header.size = cpu_to_le32(request_size - sizeof(request->header));
	request->unk_10 = cpu_to_le16(1);
	request->msg_id = timestamp;

	frame = (struct appletbdrm_frame *)request->data;

	/*
	 * The coordinates need to be translated to the coordinate
	 * system the device expects, see the comment in
	 * appletbdrm_setup_mode_config
	 */
	frame->begin_x = cpu_to_le16(r.y1);
	frame->begin_y = cpu_to_le16(adev->height - r.x2);
	frame->width = cpu_to_le16(height);
	frame->height = cpu_to_le16(width);
	frame->buf_size = cpu_to_le32(buf_size);

	/*
	 * The shadow buffer already holds BGR888 pixels; extract the coalesced
	 * rect row by row into the tightly-packed frame buffer. Read without the
	 * lock (like gud) — a rare overlapping write only causes a transient tear
	 * the next coalesced send corrects.
	 */
	dst = frame->buf;
	src = (const u8 *)adev->shadow_buf + r.y1 * adev->shadow_pitch +
		r.x1 * BITS_TO_BYTES(APPLETBDRM_BITS_PER_PIXEL);
	for (y = 0; y < height; y++) {
		memcpy(dst, src, width * BITS_TO_BYTES(APPLETBDRM_BITS_PER_PIXEL));
		dst += width * BITS_TO_BYTES(APPLETBDRM_BITS_PER_PIXEL);
		src += adev->shadow_pitch;
	}

	footer = (struct appletbdrm_fb_request_footer *)&request->data[frames_size];
	footer->unk_0c = cpu_to_le32(0xfffe);
	footer->unk_1c = cpu_to_le32(0x80001);
	footer->unk_34 = cpu_to_le32(0x80002);
	footer->unk_4c = cpu_to_le32(0xffff);
	footer->timestamp = cpu_to_le64(timestamp);

	t_send = ktime_get_ns();
	send_ret = appletbdrm_send_request(adev, &request->header, request_size);
	if (!send_ret) {
		read_ret = appletbdrm_read_response(adev, &response->header, sizeof(*response),
						    APPLETBDRM_MSG_UPDATE_COMPLETE, APPLETBDRM_ACK_TIMEOUT);
		if (!read_ret)
			off_ms = ((s64)timestamp -
				  (s64)le64_to_cpu(response->timestamp)) / (s64)NSEC_PER_MSEC;
	}

	/*
	 * Keep the response pipe from backing up: if this frame's ack was stale (we
	 * fell behind) or the read errored, drain the stale tail of acks the device
	 * already queued so the next frame starts current. THIS frame's own ack was
	 * already waited for in full above; this only mops up the already-arrived
	 * backlog — unlike Patch A, which shortened that wait and desynced the device.
	 */
	if (read_ret || off_ms > APPLETBDRM_DRIFT_DRAIN_MS)
		drained = appletbdrm_drain_responses(adev);

	t_done = ktime_get_ns();
	wall = ktime_get_real_ns();

	/*
	 * One line per frame the worker actually pushes. off_ms is the drift (how
	 * far behind the ack we got is); dt_ms is how long the send+ack took;
	 * drained is how many stale acks we mopped up. The epoch t= lines up with
	 * tiny-dfr's [dbg] timestamps for cross-correlation.
	 */
	if (flush_log)
		drm_info(drm,
			 "flush #%llu t=%llu.%09llu send=%d read=%d off=%lldms dt=%llums drained=%d rect=(%d,%d)-(%d,%d)\n",
			 adev->flush_seq++, wall / NSEC_PER_SEC, wall % NSEC_PER_SEC,
			 send_ret, read_ret, off_ms,
			 (t_done - t_send) / NSEC_PER_MSEC, drained,
			 r.x1, r.y1, r.x2, r.y2);

	kfree(request);

out:
	/*
	 * Re-queue (paced) if more damage arrived while we were busy. atomic_update
	 * queues with the same delay, so device updates stay >= the interval apart
	 * however they are triggered.
	 */
	mutex_lock(&adev->damage_lock);
	more = !appletbdrm_damage_empty(&adev->damage);
	mutex_unlock(&adev->damage_lock);
	if (more)
		queue_delayed_work(system_long_wq, &adev->flush_work,
				   msecs_to_jiffies(APPLETBDRM_FLUSH_INTERVAL_MS));

	drm_dev_exit(idx);
}

static void appletbdrm_primary_plane_helper_atomic_update(struct drm_plane *plane,
						     struct drm_atomic_state *old_state)
{
	struct appletbdrm_device *adev = drm_to_adev(plane->dev);
	struct drm_device *drm = plane->dev;
	struct drm_plane_state *plane_state = plane->state;
	struct drm_plane_state *old_plane_state = drm_atomic_get_old_plane_state(old_state, plane);
	int idx;

	if (!drm_dev_enter(drm, &idx))
		return;

	appletbdrm_flush_damage(adev, old_plane_state, plane_state);

	drm_dev_exit(idx);
}

static void appletbdrm_primary_plane_helper_atomic_disable(struct drm_plane *plane,
							   struct drm_atomic_state *state)
{
	struct drm_device *dev = plane->dev;
	struct appletbdrm_device *adev = drm_to_adev(dev);
	int idx;

	/*
	 * Drain the worker and free the shadow buffer unconditionally — on
	 * disconnect the device is already unplugged (so the drm_dev_enter below
	 * fails), but the work still needs cancelling and the buffer freeing.
	 * cancel_delayed_work_sync waits for any in-flight send+ack to finish; do not
	 * hold damage_lock across it.
	 */
	cancel_delayed_work_sync(&adev->flush_work);

	mutex_lock(&adev->damage_lock);
	kvfree(adev->shadow_buf);
	adev->shadow_buf = NULL;
	appletbdrm_damage_init(&adev->damage);
	mutex_unlock(&adev->damage_lock);

	if (!drm_dev_enter(dev, &idx))
		return;

	appletbdrm_clear_display(adev);

	drm_dev_exit(idx);
}

static const struct drm_plane_helper_funcs appletbdrm_primary_plane_helper_funcs = {
	DRM_GEM_SHADOW_PLANE_HELPER_FUNCS,
	.atomic_check = appletbdrm_primary_plane_helper_atomic_check,
	.atomic_update = appletbdrm_primary_plane_helper_atomic_update,
	.atomic_disable = appletbdrm_primary_plane_helper_atomic_disable,
};

static const struct drm_plane_funcs appletbdrm_primary_plane_funcs = {
	.update_plane = drm_atomic_helper_update_plane,
	.disable_plane = drm_atomic_helper_disable_plane,
	DRM_GEM_SHADOW_PLANE_FUNCS,
	.destroy = drm_plane_cleanup,
};

static enum drm_mode_status appletbdrm_crtc_helper_mode_valid(struct drm_crtc *crtc,
							  const struct drm_display_mode *mode)
{
	struct appletbdrm_device *adev = drm_to_adev(crtc->dev);

	return drm_crtc_helper_mode_valid_fixed(crtc, mode, &adev->mode);
}

static const struct drm_mode_config_funcs appletbdrm_mode_config_funcs = {
	.fb_create = drm_gem_fb_create_with_dirty,
	.atomic_check = drm_atomic_helper_check,
	.atomic_commit = drm_atomic_helper_commit,
};

static const struct drm_connector_funcs appletbdrm_connector_funcs = {
	.reset = drm_atomic_helper_connector_reset,
	.destroy = drm_connector_cleanup,
	.fill_modes = drm_helper_probe_single_connector_modes,
	.atomic_destroy_state = drm_atomic_helper_connector_destroy_state,
	.atomic_duplicate_state = drm_atomic_helper_connector_duplicate_state,
};

static const struct drm_connector_helper_funcs appletbdrm_connector_helper_funcs = {
	.get_modes = appletbdrm_connector_helper_get_modes,
};

static const struct drm_crtc_helper_funcs appletbdrm_crtc_helper_funcs = {
	.mode_valid = appletbdrm_crtc_helper_mode_valid,
};

static const struct drm_crtc_funcs appletbdrm_crtc_funcs = {
	.reset = drm_atomic_helper_crtc_reset,
	.destroy = drm_crtc_cleanup,
	.set_config = drm_atomic_helper_set_config,
	.page_flip = drm_atomic_helper_page_flip,
	.atomic_duplicate_state = drm_atomic_helper_crtc_duplicate_state,
	.atomic_destroy_state = drm_atomic_helper_crtc_destroy_state,
};

static const struct drm_encoder_funcs appletbdrm_encoder_funcs = {
	.destroy = drm_encoder_cleanup,
};

DEFINE_DRM_GEM_FOPS(appletbdrm_drm_fops);

static const struct drm_driver appletbdrm_drm_driver = {
	DRM_GEM_SHMEM_DRIVER_OPS,
	.name			= "appletbdrm",
	.desc			= "Apple Touch Bar DRM Driver",
	.major			= 1,
	.minor			= 0,
	.driver_features	= DRIVER_MODESET | DRIVER_GEM | DRIVER_ATOMIC,
	.fops			= &appletbdrm_drm_fops,
};

static int appletbdrm_setup_mode_config(struct appletbdrm_device *adev)
{
	struct drm_connector *connector = &adev->connector;
	struct drm_plane *primary_plane;
	struct drm_crtc *crtc;
	struct drm_encoder *encoder;
	struct drm_device *drm = &adev->drm;
	int ret;

	ret = drmm_mode_config_init(drm);
	if (ret) {
		drm_err(drm, "Failed to initialize mode configuration\n");
		return ret;
	}

	primary_plane = &adev->primary_plane;
	ret = drm_universal_plane_init(drm, primary_plane, 0,
				       &appletbdrm_primary_plane_funcs,
				       appletbdrm_primary_plane_formats,
				       ARRAY_SIZE(appletbdrm_primary_plane_formats),
				       NULL,
				       DRM_PLANE_TYPE_PRIMARY, NULL);
	if (ret) {
		drm_err(drm, "Failed to initialize universal plane object\n");
		return ret;
	}

	drm_plane_helper_add(primary_plane, &appletbdrm_primary_plane_helper_funcs);
	drm_plane_enable_fb_damage_clips(primary_plane);

	crtc = &adev->crtc;
	ret = drm_crtc_init_with_planes(drm, crtc, primary_plane, NULL,
					&appletbdrm_crtc_funcs, NULL);
	if (ret) {
		drm_err(drm, "Failed to initialize CRTC object\n");
		return ret;
	}

	drm_crtc_helper_add(crtc, &appletbdrm_crtc_helper_funcs);

	encoder = &adev->encoder;
	ret = drm_encoder_init(drm, encoder, &appletbdrm_encoder_funcs,
			       DRM_MODE_ENCODER_DAC, NULL);
	if (ret) {
		drm_err(drm, "Failed to initialize encoder\n");
		return ret;
	}

	encoder->possible_crtcs = drm_crtc_mask(crtc);

	/*
	 * The coordinate system used by the device is different from the
	 * coordinate system of the framebuffer in that the x and y axes are
	 * swapped, and that the y axis is inverted; so what the device reports
	 * as the height is actually the width of the framebuffer and vice
	 * versa.
	 */
	drm->mode_config.max_width = max(adev->height, DRM_SHADOW_PLANE_MAX_WIDTH);
	drm->mode_config.max_height = max(adev->width, DRM_SHADOW_PLANE_MAX_HEIGHT);
	drm->mode_config.preferred_depth = APPLETBDRM_BITS_PER_PIXEL;
	drm->mode_config.funcs = &appletbdrm_mode_config_funcs;

	adev->mode = (struct drm_display_mode) {
		DRM_MODE_INIT(60, adev->height, adev->width,
			      DRM_MODE_RES_MM(adev->height, 218),
			      DRM_MODE_RES_MM(adev->width, 218))
	};

	ret = drm_connector_init(drm, connector,
				 &appletbdrm_connector_funcs, DRM_MODE_CONNECTOR_USB);
	if (ret) {
		drm_err(drm, "Failed to initialize connector\n");
		return ret;
	}

	drm_connector_helper_add(connector, &appletbdrm_connector_helper_funcs);

	ret = drm_connector_set_panel_orientation(connector,
						  DRM_MODE_PANEL_ORIENTATION_RIGHT_UP);
	if (ret) {
		drm_err(drm, "Failed to set panel orientation\n");
		return ret;
	}

	connector->display_info.non_desktop = true;
	ret = drm_object_property_set_value(&connector->base,
					    drm->mode_config.non_desktop_property, true);
	if (ret) {
		drm_err(drm, "Failed to set non-desktop property\n");
		return ret;
	}

	ret = drm_connector_attach_encoder(connector, encoder);

	if (ret) {
		drm_err(drm, "Failed to initialize simple display pipe\n");
		return ret;
	}

	drm_mode_config_reset(drm);

	return 0;
}

static int appletbdrm_probe(struct usb_interface *intf,
			    const struct usb_device_id *id)
{
	struct usb_endpoint_descriptor *bulk_in, *bulk_out;
	struct device *dev = &intf->dev;
	struct appletbdrm_device *adev;
	struct drm_device *drm = NULL;
	struct device *dma_dev;
	int ret;

	ret = usb_find_common_endpoints(intf->cur_altsetting, &bulk_in, &bulk_out, NULL, NULL);
	if (ret) {
		drm_err(drm, "appletbdrm: Failed to find bulk endpoints\n");
		return ret;
	}

	adev = devm_drm_dev_alloc(dev, &appletbdrm_drm_driver, struct appletbdrm_device, drm);
	if (IS_ERR(adev))
		return PTR_ERR(adev);

	adev->in_ep = bulk_in->bEndpointAddress;
	adev->out_ep = bulk_out->bEndpointAddress;

	drm = &adev->drm;

	mutex_init(&adev->damage_lock);
	INIT_DELAYED_WORK(&adev->flush_work, appletbdrm_flush_work);
	appletbdrm_damage_init(&adev->damage);

	adev->response = devm_kzalloc(dev, sizeof(*adev->response), GFP_KERNEL);
	if (!adev->response)
		return -ENOMEM;

	usb_set_intfdata(intf, adev);

	dma_dev = usb_intf_get_dma_device(intf);
	if (dma_dev) {
		drm_dev_set_dma_dev(drm, dma_dev);
		put_device(dma_dev);
	} else {
		drm_warn(drm, "buffer sharing not supported"); /* not an error */
	}

	ret = appletbdrm_get_information(adev);
	if (ret) {
		drm_err(drm, "Failed to get display information\n");
		return ret;
	}

	ret = appletbdrm_signal_readiness(adev);
	if (ret) {
		drm_err(drm, "Failed to signal readiness\n");
		return ret;
	}

	ret = appletbdrm_setup_mode_config(adev);
	if (ret) {
		drm_err(drm, "Failed to setup mode config\n");
		return ret;
	}

	ret = drm_dev_register(drm, 0);
	if (ret) {
		drm_err(drm, "Failed to register DRM device\n");
		return ret;
	}

	ret = appletbdrm_clear_display(adev);
	if (ret) {
		drm_err(drm, "Failed to clear display\n");
		return ret;
	}

	return 0;
}

static void appletbdrm_disconnect(struct usb_interface *intf)
{
	struct appletbdrm_device *adev = usb_get_intfdata(intf);
	struct drm_device *drm = &adev->drm;

	drm_dev_unplug(drm);
	drm_atomic_helper_shutdown(drm);
}

static void appletbdrm_shutdown(struct usb_interface *intf)
{
	struct appletbdrm_device *adev = usb_get_intfdata(intf);

	/*
	 * The framebuffer needs to be cleared on shutdown since its content
	 * persists across boots
	 */
	drm_atomic_helper_shutdown(&adev->drm);
}

static const struct usb_device_id appletbdrm_usb_id_table[] = {
	{ USB_DEVICE_INTERFACE_CLASS(0x05ac, 0x8302, USB_CLASS_AUDIO_VIDEO) },
	{}
};
MODULE_DEVICE_TABLE(usb, appletbdrm_usb_id_table);

static struct usb_driver appletbdrm_usb_driver = {
	.name		= "appletbdrm",
	.probe		= appletbdrm_probe,
	.disconnect	= appletbdrm_disconnect,
	.shutdown	= appletbdrm_shutdown,
	.id_table	= appletbdrm_usb_id_table,
};
module_usb_driver(appletbdrm_usb_driver);

MODULE_AUTHOR("Kerem Karabay <kekrby@gmail.com>");
MODULE_DESCRIPTION("Apple Touch Bar DRM Driver");
MODULE_LICENSE("GPL");
